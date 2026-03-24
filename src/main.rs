pub mod adpcm;
pub mod atvv;
pub mod ble;
#[cfg(feature = "dbus")]
pub mod dbus;
pub mod pw;

use clap::Parser;

#[derive(Parser)]
#[command(name = "atvvoice", about = "ATVVoice — BLE voice remote microphone daemon")]
struct Cli {
    /// Filter by Bluetooth address (e.g., AA:BB:CC:DD:EE:FF)
    #[arg(short, long)]
    device: Option<String>,

    /// BlueZ adapter name (default: auto-detect)
    #[arg(short, long)]
    adapter: Option<String>,

    /// Audio gain in dB (default: 20)
    #[arg(short, long, default_value = "20")]
    gain: f32,

    /// Mic button mode: "toggle" (press on/off) or "hold" (hold to stream)
    #[arg(short, long, value_enum, default_value = "toggle")]
    mode: atvv::MicMode,

    /// Close mic after N seconds without audio frames (device asleep). 0 = disabled.
    #[arg(long, default_value = "5")]
    frame_timeout: u64,

    /// Close mic after N seconds since last button press (user inactive). 0 = disabled.
    #[arg(short = 't', long, default_value = "0")]
    idle_timeout: u64,

    /// Instance name suffix. Sets PipeWire node name and D-Bus bus name.
    /// e.g. --name living-room → node "atvvoice-living-room",
    /// D-Bus "org.atvvoice.living-room"
    #[arg(short, long)]
    name: Option<String>,

    /// PipeWire node description (shown in audio settings).
    /// Default: "ATVVoice Microphone" (or "ATVVoice Microphone (<name>)" if --name is set)
    #[arg(long)]
    description: Option<String>,

    /// Disable D-Bus control interface
    #[cfg(feature = "dbus")]
    #[arg(long)]
    no_dbus: bool,

    /// Increase log verbosity (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Init tracing based on verbosity
    let filter = match cli.verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .init();

    // Connect to BlueZ
    let session = bluer::Session::new().await?;
    let adapter = match &cli.adapter {
        Some(name) => session.adapter(name)?,
        None => session.default_adapter().await?,
    };
    tracing::info!("Using adapter: {}", adapter.name());

    // Parse optional address filter
    let filter_addr = cli.device.map(|s| s.parse()).transpose()?;

    // Find ATVV device (retries until found)
    let device = loop {
        match ble::find_atvv_device(&adapter, filter_addr).await {
            Ok(device) => break device,
            Err(e) => {
                tracing::info!("No ATVV device found ({e}), retrying in 5s...");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    };
    #[cfg(feature = "dbus")]
    let device_addr = device.address().to_string();

    // Wait for device to be connected, using event stream if possible.
    async fn ensure_connected(device: &bluer::Device) {
        if device.is_connected().await.unwrap_or(false) {
            return;
        }
        tracing::info!("Waiting for device to connect...");
        // Try event-driven wait first
        if let Ok(mut events) = device.events().await {
            while let Some(event) = futures::StreamExt::next(&mut events).await {
                if let bluer::DeviceEvent::PropertyChanged(
                    bluer::DeviceProperty::Connected(true),
                ) = event
                {
                    return;
                }
            }
        }
        // Fallback: poll
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if device.is_connected().await.unwrap_or(false) {
                return;
            }
        }
    }
    ensure_connected(&device).await;

    // Resolve GATT characteristics (retries on failure)
    let mut chars = loop {
        match ble::resolve_chars(&device).await {
            Ok(c) => {
                tracing::info!("ATVV characteristics resolved");
                break c;
            }
            Err(e) => {
                tracing::warn!("Failed to resolve characteristics ({e}), retrying in 2s...");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    };

    let gain = cli.gain;
    #[allow(unused_variables)]
    let (node_name, dbus_name, default_description) = match &cli.name {
        Some(suffix) => (
            format!("atvvoice-{suffix}"),
            format!("org.atvvoice.{suffix}"),
            format!("ATVVoice Microphone ({suffix})"),
        ),
        None => (
            "atvvoice".to_string(),
            "org.atvvoice".to_string(),
            "ATVVoice Microphone".to_string(),
        ),
    };
    let node_description = cli.description.unwrap_or(default_description);

    // State watch channel: atvv -> D-Bus (if enabled).
    let (state_tx, _state_rx) = tokio::sync::watch::channel(atvv::State::Init);

    // Set up D-Bus control interface (if feature and CLI allow).
    #[cfg(feature = "dbus")]
    let (mut dbus_cmd_rx, _dbus_conn) = if !cli.no_dbus {
        let info = dbus::DaemonInfo {
            device_address: device_addr,
            node_name: node_name.clone(),
        };
        match dbus::serve(_state_rx, info, &dbus_name).await {
            Ok((cmd_rx, conn)) => (Some(cmd_rx), Some(conn)),
            Err(e) => {
                tracing::warn!("Failed to register D-Bus interface: {}", e);
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    // Set up signal handling for graceful shutdown.
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let timeouts = atvv::SessionTimeouts {
        frame_timeout: std::time::Duration::from_secs(cli.frame_timeout),
        idle_timeout: std::time::Duration::from_secs(cli.idle_timeout),
    };

    // Run ATVV session with reconnection loop.
    // Channels and PipeWire thread are created per-session so the PW source
    // disappears from audio settings when the device disconnects.
    loop {
        // Create per-session channels and audio pipeline.
        let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (pcm_tx, pcm_rx) = std::sync::mpsc::channel::<Vec<i16>>();

        // PipeWire channel for clean shutdown signaling.
        let (pw_shutdown_tx, pw_shutdown_rx) = pipewire::channel::channel::<pw::Shutdown>();

        let pw_name = node_name.clone();
        let pw_desc = node_description.clone();
        let pw_thread = std::thread::spawn(move || {
            if let Err(e) = pw::run_pw_source(pcm_rx, gain, &pw_name, &pw_desc, pw_shutdown_rx) {
                tracing::error!("PipeWire error: {}", e);
            }
        });

        let decoder_handle = tokio::spawn(async move {
            while let Some(frame_data) = frame_rx.recv().await {
                if frame_data.len() == adpcm::FRAME_SIZE {
                    let frame: [u8; 134] = frame_data.try_into().unwrap();
                    let (_seq, mut samples) = adpcm::decode_frame(&frame);
                    adpcm::declip(&mut samples);
                    adpcm::lowpass(&mut samples);
                    let _ = pcm_tx.send(samples.to_vec());
                }
            }
        });

        let ble_device = ble::BluerDevice { device: &device, chars: &chars };
        let session_result = tokio::select! {
            result = atvv::run_session(
                &ble_device,
                frame_tx,
                cli.mode,
                &timeouts,
                {
                    #[cfg(feature = "dbus")]
                    { dbus_cmd_rx.as_mut() }
                    #[cfg(not(feature = "dbus"))]
                    { None }
                },
                Some(&state_tx),
            ) => result,
            _ = &mut ctrl_c => {
                // Send MIC_CLOSE on graceful shutdown
                let _ = chars.tx.write(atvv::CMD_MIC_CLOSE).await;
                tracing::info!("Sent MIC_CLOSE, shutting down");
                // Signal PW thread to disconnect stream and quit cleanly.
                let _ = pw_shutdown_tx.send(pw::Shutdown);
                break;
            }
        };

        // Session ended — signal PipeWire to disconnect stream cleanly (removes
        // the node from audio settings), then wait for the thread to finish.
        let _ = pw_shutdown_tx.send(pw::Shutdown);
        decoder_handle.abort();
        let _ = pw_thread.join();

        match session_result {
            Ok(()) => tracing::info!("Session ended"),
            Err(e) => tracing::warn!("Session error: {}", e),
        }

        // Update D-Bus state
        let _ = state_tx.send(atvv::State::Init);

        // Wait for device to reconnect
        ensure_connected(&device).await;
        tracing::info!("Device reconnected");

        // Re-resolve characteristics (handles may change after reconnect)
        chars = loop {
            match ble::resolve_chars(&device).await {
                Ok(c) => {
                    tracing::info!("ATVV characteristics re-resolved");
                    break c;
                }
                Err(e) => {
                    tracing::warn!("Failed to re-resolve characteristics ({e}), retrying in 2s...");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        };
    }

    Ok(())
}
