pub mod adpcm;
pub mod atvv;
pub mod ble;
#[cfg(feature = "dbus")]
pub mod dbus;
pub mod protocol;
pub mod pw;

use clap::Parser;
use std::time::Duration;

/// Delay between retries when resolving characteristics or polling for connection.
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Delay between retries when discovering ATVV devices.
const DISCOVERY_RETRY_DELAY: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(name = "atvvoice", about = "ATVVoice - BLE voice remote microphone daemon")]
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

    /// Close mic after N seconds since last mic button press. 0 = disabled.
    #[arg(short = 't', long, default_value = "0")]
    idle_timeout: u64,

    /// Re-send MIC_OPEN every N seconds to prevent remote's audio transfer timeout. 0 = disabled.
    #[arg(long, default_value = "10")]
    keep_alive: u64,

    /// Override remote protocol version (e.g. "0.4", "1.0"). Auto-detected from CAPS_RESP if not set.
    #[arg(long)]
    protocol_version: Option<String>,

    /// Instance name suffix. Sets PipeWire node name and D-Bus bus name.
    #[arg(short, long)]
    name: Option<String>,

    /// PipeWire node description (shown in audio settings).
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

/// Sanitize a string for use as a D-Bus name component and PipeWire node suffix.
/// Lowercases, replaces non-alphanumeric chars with hyphens, collapses runs, trims.
fn sanitize_name(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            result.push(c.to_ascii_lowercase());
        } else if !result.ends_with('-') {
            result.push('-');
        }
    }
    result.trim_matches('-').to_string()
}

/// Wait for device to be connected, using event stream if possible.
async fn ensure_connected(device: &bluer::Device) {
    if device.is_connected().await.unwrap_or(false) {
        return;
    }
    tracing::info!("Waiting for device to connect...");
    if let Ok(mut events) = device.events().await {
        while let Some(event) = futures::StreamExt::next(&mut events).await {
            if let bluer::DeviceEvent::PropertyChanged(bluer::DeviceProperty::Connected(true)) =
                event
            {
                return;
            }
        }
    }
    // Fallback: poll
    loop {
        tokio::time::sleep(RETRY_DELAY).await;
        if device.is_connected().await.unwrap_or(false) {
            return;
        }
    }
}

/// Check if an error indicates the device is locked by another instance.
fn is_device_locked_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("exclusive") || msg.contains("NotPermitted") || msg.contains("InProgress")
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

    // Validate --name early.
    if let Some(ref name) = cli.name {
        let sanitized = sanitize_name(name);
        if sanitized != *name {
            eprintln!(
                "error: --name {name:?} contains invalid characters. \
                 Use lowercase alphanumeric, hyphens, and underscores only (e.g. --name {sanitized:?})."
            );
            std::process::exit(1);
        }
    }

    // Connect to BlueZ
    let session = bluer::Session::new().await?;
    let adapter = match &cli.adapter {
        Some(name) => session.adapter(name)?,
        None => session.default_adapter().await?,
    };
    tracing::info!("Using adapter: {}", adapter.name());

    let filter_addr: Option<bluer::Address> = cli.device.map(|s| s.parse()).transpose()?;

    // Set up signal handling for graceful shutdown.
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let timeouts = atvv::SessionTimeouts {
        frame_timeout: std::time::Duration::from_secs(cli.frame_timeout),
        idle_timeout: std::time::Duration::from_secs(cli.idle_timeout),
        keepalive: std::time::Duration::from_secs(cli.keep_alive),
    };

    let protocol_version = match cli.protocol_version.as_deref() {
        Some(s) => protocol::types::ProtocolVersion::parse(s)
            .map_err(|e| anyhow::anyhow!(e))?,
        None => protocol::types::ProtocolVersion::V0_4,
    };

    // Addresses to skip during auto-discovery (locked by another instance).
    let mut excluded_addrs: Vec<bluer::Address> = Vec::new();

    // Outer loop: discover → connect → session. Restarts on lock errors in auto mode.
    'discover: loop {
        // Find ATVV device (retries until found, interruptible by ctrl+c)
        let device = loop {
            tokio::select! {
                result = ble::find_atvv_device(&adapter, filter_addr, &excluded_addrs) => {
                    match result {
                        Ok(device) => break device,
                        Err(e) => {
                            tracing::info!("No ATVV device found ({e}), retrying in 5s...");
                            tokio::time::sleep(DISCOVERY_RETRY_DELAY).await;
                        }
                    }
                }
                _ = &mut ctrl_c => {
                    tracing::info!("Shutting down");
                    return Ok(());
                }
            }
        };

        tokio::select! {
            _ = ensure_connected(&device) => {}
            _ = &mut ctrl_c => {
                tracing::info!("Shutting down");
                return Ok(());
            }
        }

        // Resolve GATT characteristics (retries on failure, interruptible)
        let mut chars = loop {
            tokio::select! {
                result = ble::resolve_chars(&device) => {
                    match result {
                        Ok(c) => {
                            tracing::info!("ATVV characteristics resolved");
                            break c;
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to resolve characteristics ({e}), retrying in 2s..."
                            );
                            tokio::time::sleep(RETRY_DELAY).await;
                        }
                    }
                }
                _ = &mut ctrl_c => {
                    tracing::info!("Shutting down");
                    return Ok(());
                }
            }
        };

        // Derive instance names from device.
        let ble_name = device.name().await.ok().flatten().unwrap_or_default();
        let suffix = cli.name.clone().unwrap_or_else(|| {
            if !ble_name.is_empty() {
                sanitize_name(&ble_name)
            } else {
                sanitize_name(&device.address().to_string())
            }
        });
        let node_name = format!("atvvoice-{suffix}");
        #[allow(unused_variables)]
        let dbus_name = format!("org.atvvoice.{suffix}");
        let node_description = cli.description.clone().unwrap_or_else(|| {
            if ble_name.is_empty() {
                "ATVVoice Microphone".to_string()
            } else {
                ble_name.clone()
            }
        });

        let gain = cli.gain;

        // State watch channel: atvv -> D-Bus (if enabled).
        let (state_tx, _state_rx) = tokio::sync::watch::channel(atvv::State::Init);

        // Set up D-Bus control interface (if feature and CLI allow).
        #[cfg(feature = "dbus")]
        let (mut dbus_cmd_rx, _dbus_conn) = if !cli.no_dbus {
            #[cfg(feature = "dbus")]
            let device_addr = device.address().to_string();
            let info = dbus::DaemonInfo {
                device_address: device_addr,
                node_name: node_name.clone(),
            };
            match dbus::serve(_state_rx, info, &dbus_name).await {
                Ok((cmd_rx, conn)) => (Some(cmd_rx), Some(conn)),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("already taken")
                        || msg.contains("NameAlreadyOwned")
                        || msg.contains("exists")
                    {
                        tracing::error!(
                            "D-Bus name '{dbus_name}' is already in use. \
                             Another ATVVoice instance may be running with the same name. \
                             Use --name <suffix> to differentiate instances, or --no-dbus to disable."
                        );
                        std::process::exit(1);
                    }
                    tracing::warn!("Failed to register D-Bus interface: {e}");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        // Inner loop: session → reconnect (same device).
        loop {
            let mut session_protocol = protocol::create_protocol(protocol_version);

            let (frame_tx, mut frame_rx) =
                tokio::sync::mpsc::channel::<protocol::types::AudioFrame>(64);
            let (pcm_tx, pcm_rx) = std::sync::mpsc::channel::<Vec<i16>>();
            let (pw_shutdown_tx, pw_shutdown_rx) =
                pipewire::channel::channel::<pw::Shutdown>();

            let pw_name = node_name.clone();
            let pw_desc = node_description.clone();
            let pw_thread = std::thread::spawn(move || {
                if let Err(e) =
                    pw::run_pw_source(pcm_rx, gain, &pw_name, &pw_desc, pw_shutdown_rx)
                {
                    tracing::error!("PipeWire error: {}", e);
                }
            });

            // Post-processing task: Protocol already decoded the audio,
            // so we just apply declip/lowpass/gain and forward PCM.
            let decoder_handle = tokio::spawn(async move {
                while let Some(frame) = frame_rx.recv().await {
                    let mut samples = frame.samples;
                    adpcm::declip(&mut samples);
                    adpcm::lowpass(&mut samples);
                    let _ = pcm_tx.send(samples);
                }
            });

            let ble_device = ble::BluerDevice {
                device: &device,
                chars: &chars,
            };
            let session_result = tokio::select! {
                result = atvv::run_session(
                    &ble_device,
                    &mut *session_protocol,
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
                    let mic_close = session_protocol.mic_close_cmd(
                        protocol::types::StreamId::ANY,
                    );
                    let _ = chars.tx.write(&mic_close).await;
                    tracing::info!("Sent MIC_CLOSE, shutting down");
                    let _ = pw_shutdown_tx.send(pw::Shutdown);
                    break 'discover;
                }
            };

            // Tear down audio pipeline.
            let _ = pw_shutdown_tx.send(pw::Shutdown);
            decoder_handle.abort();
            let _ = pw_thread.join();

            match &session_result {
                Ok(()) => tracing::info!("Session ended"),
                Err(e) if is_device_locked_error(e) => {
                    if filter_addr.is_some() {
                        // Explicit --device: fatal, user asked for this specific device.
                        tracing::error!(
                            "Device {} is already in use by another ATVVoice instance.",
                            device.address()
                        );
                        std::process::exit(1);
                    } else {
                        // Auto mode: skip this device, try to find another.
                        tracing::warn!(
                            "Device {} is locked by another instance, looking for another device...",
                            device.address()
                        );
                        excluded_addrs.push(device.address());
                        continue 'discover;
                    }
                }
                Err(e) => tracing::warn!("Session error: {e}"),
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
                        tracing::warn!(
                            "Failed to re-resolve characteristics ({e}), retrying in 2s..."
                        );
                        tokio::time::sleep(RETRY_DELAY).await;
                    }
                }
            };
        }
    }

    Ok(())
}
