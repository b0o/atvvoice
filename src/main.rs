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

    /// PipeWire node name
    #[arg(long, default_value = "atvvoice")]
    node_name: String,

    /// PipeWire node description (shown in audio settings)
    #[arg(long, default_value = "ATVVoice Microphone")]
    node_description: String,

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

    // Create channels:
    // tokio mpsc: atvv -> decoder bridge
    // std mpsc: decoder -> pipewire bridge
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (pcm_tx, pcm_rx) = std::sync::mpsc::channel::<Vec<i16>>();

    // Spawn PipeWire thread
    let gain = cli.gain;
    let node_name = cli.node_name;
    let node_description = cli.node_description;
    #[cfg(feature = "dbus")]
    let pw_node_name = node_name.clone();
    std::thread::spawn(move || {
        if let Err(e) = pw::run_pw_source(pcm_rx, gain, &node_name, &node_description) {
            tracing::error!("PipeWire error: {}", e);
        }
    });

    // Spawn decoder task
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

    // State watch channel: atvv -> D-Bus (if enabled).
    let (state_tx, _state_rx) = tokio::sync::watch::channel(atvv::State::Init);

    // Set up D-Bus control interface (if feature and CLI allow).
    #[cfg(feature = "dbus")]
    let (mut dbus_cmd_rx, _dbus_conn) = if !cli.no_dbus {
        let info = dbus::DaemonInfo {
            device_address: device_addr,
            node_name: pw_node_name.clone(),
        };
        match dbus::serve(_state_rx, info).await {
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

    // Run ATVV session with reconnection loop
    loop {
        let ble_device = ble::BluerDevice { device: &device, chars: &chars };
        tokio::select! {
            result = atvv::run_session(
                &ble_device,
                frame_tx.clone(),
                cli.mode,
                &timeouts,
                {
                    #[cfg(feature = "dbus")]
                    { dbus_cmd_rx.as_mut() }
                    #[cfg(not(feature = "dbus"))]
                    { None }
                },
                Some(&state_tx),
            ) => {
                match result {
                    Ok(()) => tracing::info!("Session ended cleanly"),
                    Err(e) => tracing::warn!("Session error: {}", e),
                }
                // Wait for device to reconnect
                tracing::info!("Waiting for device to reconnect...");
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
            _ = &mut ctrl_c => {
                // Send MIC_CLOSE on graceful shutdown
                let _ = chars.tx.write(atvv::CMD_MIC_CLOSE).await;
                tracing::info!("Sent MIC_CLOSE, shutting down");
                break;
            }
        }
    }

    decoder_handle.abort();
    Ok(())
}
