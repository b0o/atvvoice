//! atvvoice — BLE voice remote audio daemon.

mod adpcm;
mod atvv;
mod ble;
mod consumer;
#[cfg(feature = "dbus")]
mod dbus;
mod protocol;
mod pw;

use anyhow::Context;
use clap::Parser;
use std::collections::HashSet;
use std::time::Duration;

/// Delay between retries when resolving characteristics or polling for connection.
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Delay between retries when discovering ATVV devices.
const DISCOVERY_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Capacity of the async channel between the ATVV session and the decoder task.
const DECODER_CHANNEL_CAPACITY: usize = 64;

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

    /// Close mic after N seconds without audio frames (device asleep). 0 = disabled.
    #[arg(long, default_value = "5")]
    frame_timeout: u64,

    /// Re-send MIC_OPEN every N seconds to prevent remote's audio transfer timeout. 0 = disabled.
    #[arg(long, default_value = "10")]
    keep_alive: u64,

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

    /// Enable automatic mic open/close based on PipeWire consumer presence.
    /// When a PipeWire client connects to the virtual source, the mic opens
    /// automatically. When all clients disconnect, the mic closes immediately.
    #[arg(long)]
    mic_on_demand: bool,

    /// Increase log verbosity (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

/// Sanitize a string for use as a D-Bus name component and PipeWire node suffix.
/// Lowercases, replaces non-alphanumeric chars with hyphens, collapses runs, trims.
#[must_use]
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
    match device.is_connected().await {
        Ok(true) => return,
        Err(e) => tracing::warn!("is_connected() failed, assuming disconnected: {e}"),
        _ => {}
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
        match device.is_connected().await {
            Ok(true) => return,
            Err(e) => tracing::warn!("is_connected() poll failed: {e}"),
            _ => {}
        }
    }
}

/// Check if an error indicates the device is locked by another instance.
fn is_device_locked_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("exclusive") || msg.contains("NotPermitted") || msg.contains("InProgress")
}

/// Timeout waiting for CAPS_RESP during negotiation.
const NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(5);

/// Send GET_CAPS and wait for CAPS_RESP to determine protocol version.
async fn negotiate(
    ble: &(impl atvv::BleDevice + ?Sized),
) -> anyhow::Result<(protocol::types::Capabilities, atvv::BleStreams)> {
    use futures::StreamExt;

    // Subscribe to all streams up front. Only CTL is needed for negotiation,
    // but the others must be acquired now since BLE notification handles are
    // exclusive (can't re-subscribe later).
    let mut ctl_stream = ble.ctl_notifications().await?;
    let rx_stream = ble.rx_notifications().await?;
    let device_events = ble.connection_events().await?;

    ble.write_command(&protocol::get_caps_cmd()).await?;
    tracing::info!("Sent GET_CAPS (v1.0)");

    let deadline = tokio::time::sleep(NEGOTIATE_TIMEOUT);
    tokio::pin!(deadline);

    // BleStream<T> is Pin<Box<dyn Stream>>, already Unpin -- no tokio::pin! needed.
    loop {
        tokio::select! {
            data = ctl_stream.next() => {
                match data {
                    Some(data) => {
                        if let Some(caps) = protocol::parse_caps_resp(&data) {
                            tracing::info!(
                                "CAPS_RESP: version={}, codecs={:?}, model={:?}, frame_size={}",
                                caps.version, caps.codecs, caps.interaction_model, caps.audio_frame_size.0
                            );
                            return Ok((caps, atvv::BleStreams {
                                ctl: ctl_stream,
                                rx: rx_stream,
                                events: device_events,
                            }));
                        }
                        // Not a CAPS_RESP (e.g., stale START_SEARCH) -- keep waiting
                        tracing::debug!("Ignoring non-CAPS_RESP CTL during negotiation: {:02x?}", data);
                    }
                    None => {
                        anyhow::bail!("device disconnected during negotiation");
                    }
                }
            }
            _ = &mut deadline => {
                anyhow::bail!("timeout waiting for CAPS_RESP ({}s)", NEGOTIATE_TIMEOUT.as_secs());
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
            anyhow::bail!(
                "--name {name:?} contains invalid characters. \
                 Use lowercase alphanumeric, hyphens, and underscores only (e.g. --name {sanitized:?})."
            );
        }
    }

    if cli.mic_on_demand && cli.frame_timeout == 0 {
        tracing::warn!("--mic-on-demand without --frame-timeout: mic may get stuck in Opening state if remote is asleep");
    }

    // Connect to BlueZ
    let session = bluer::Session::new().await?;
    let adapter = match &cli.adapter {
        Some(name) => session.adapter(name)?,
        None => session.default_adapter().await?,
    };
    tracing::info!("Using adapter: {}", adapter.name());

    let filter_addr: Option<bluer::Address> = cli
        .device
        .map(|s| s.parse().context("failed to parse device address"))
        .transpose()?;

    let timeouts = atvv::SessionTimeouts {
        frame_timeout: std::time::Duration::from_secs(cli.frame_timeout),
        keepalive: std::time::Duration::from_secs(cli.keep_alive),
    };

    // Addresses to skip during auto-discovery (locked by another instance).
    let mut excluded_addrs: HashSet<bluer::Address> = HashSet::new();

    // Outer loop: discover → connect → session. Restarts on lock errors in auto mode.
    'discover: loop {
        // Find ATVV device (retries until found, interruptible by ctrl+c)
        let excluded_vec: Vec<bluer::Address> = excluded_addrs.iter().copied().collect();
        let device = loop {
            tokio::select! {
                result = ble::find_atvv_device(&adapter, filter_addr, &excluded_vec) => {
                    match result {
                        Ok(device) => break device,
                        Err(e) => {
                            tracing::info!("No ATVV device found ({e}), retrying in 5s...");
                            tokio::time::sleep(DISCOVERY_RETRY_DELAY).await;
                        }
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Shutting down");
                    return Ok(());
                }
            }
        };

        tokio::select! {
            _ = ensure_connected(&device) => {}
            _ = tokio::signal::ctrl_c() => {
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
                _ = tokio::signal::ctrl_c() => {
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

        // Created unconditionally: the session loop sends state updates regardless of D-Bus,
        // and the cost of an unused watch channel is negligible.
        let (state_tx, _state_rx) = tokio::sync::watch::channel(atvv::State::Disconnected);

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
                        anyhow::bail!(
                            "D-Bus name '{dbus_name}' is already in use. \
                             Another ATVVoice instance may be running with the same name. \
                             Use --name <suffix> to differentiate instances, or --no-dbus to disable."
                        );
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
            let ble_device = ble::BluerDevice {
                device: &device,
                chars: &chars,
            };

            // Negotiate protocol version
            let (caps, streams) = tokio::select! {
                result = negotiate(&ble_device) => match result {
                    Ok(r) => r,
                    Err(e) => {
                        if is_device_locked_error(&e) {
                            if filter_addr.is_some() {
                                anyhow::bail!(
                                    "Device {} is already in use by another ATVVoice instance.",
                                    device.address()
                                );
                            } else {
                                tracing::warn!(
                                    "Device {} is locked by another instance, looking for another device...",
                                    device.address()
                                );
                                excluded_addrs.insert(device.address());
                                continue 'discover;
                            }
                        }
                        tracing::error!("Negotiation failed: {e}");
                        tokio::select! {
                            _ = tokio::time::sleep(RETRY_DELAY) => {}
                            _ = tokio::signal::ctrl_c() => {
                                tracing::info!("Shutting down");
                                break 'discover;
                            }
                        }
                        continue;
                    }
                },
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("interrupted during negotiation");
                    break 'discover;
                }
            };
            let _ = state_tx.send(atvv::State::Connected);
            let (mut session_protocol, codec) = match protocol::create_protocol(&caps) {
                Ok((p, c)) => (p, c),
                Err(e) => {
                    tracing::error!("Protocol creation failed: {e}");
                    continue;
                }
            };
            tracing::info!("Negotiated protocol: {} ({:?}, {}Hz)", session_protocol.version(), codec, codec.sample_rate());

            let sample_rate = codec.sample_rate();
            let (frame_tx, mut frame_rx) =
                tokio::sync::mpsc::channel::<protocol::types::AudioFrame>(DECODER_CHANNEL_CAPACITY);
            let (pcm_tx, pcm_rx) = std::sync::mpsc::channel::<Vec<i16>>();
            let (pw_shutdown_tx, pw_shutdown_rx) =
                pipewire::channel::channel::<pw::Shutdown>();

            let (consumer_tx, consumer_rx) = if cli.mic_on_demand {
                let (tx, rx) = tokio::sync::mpsc::channel(16);
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };

            let pw_name = node_name.clone();
            let pw_desc = node_description.clone();
            let pw_thread = std::thread::spawn(move || {
                if let Err(e) = pw::run_pw_source(
                    pcm_rx,
                    gain,
                    sample_rate,
                    &pw_name,
                    &pw_desc,
                    pw_shutdown_rx,
                    consumer_tx,
                ) {
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

            let session_result = tokio::select! {
                result = atvv::run_session(
                    &ble_device,
                    &mut *session_protocol,
                    streams,
                    atvv::SessionConfig {
                        audio_tx: frame_tx,
                        timeouts: &timeouts,
                        command_rx: {
                            #[cfg(feature = "dbus")]
                            { dbus_cmd_rx.as_mut() }
                            #[cfg(not(feature = "dbus"))]
                            { None }
                        },
                        state_tx: Some(&state_tx),
                        consumer_rx,
                        mic_on_demand: cli.mic_on_demand,
                    },
                ) => result,
                _ = tokio::signal::ctrl_c() => {
                    let mic_close = session_protocol.mic_close_cmd(
                        protocol::types::StreamId::ANY,
                    );
                    let _ = chars.tx.write(&mic_close).await;
                    tracing::info!("Sent MIC_CLOSE, shutting down");
                    let _ = pw_shutdown_tx.send(pw::Shutdown);
                    break 'discover;
                }
            };

            // Tear down audio pipeline: frame_tx was moved into run_session and
            // is now dropped, so the decoder task's recv() returns None and it
            // finishes naturally. Await it for a clean shutdown instead of aborting.
            let _ = pw_shutdown_tx.send(pw::Shutdown);
            let _ = decoder_handle.await;
            if let Err(panic) = pw_thread.join() {
                tracing::error!("PipeWire thread panicked: {panic:?}");
            }

            match &session_result {
                Ok(()) => tracing::info!("Session ended"),
                Err(e) if is_device_locked_error(e) => {
                    if filter_addr.is_some() {
                        // Explicit --device: fatal, user asked for this specific device.
                        anyhow::bail!(
                            "Device {} is already in use by another ATVVoice instance.",
                            device.address()
                        );
                    } else {
                        // Auto mode: skip this device, try to find another.
                        tracing::warn!(
                            "Device {} is locked by another instance, looking for another device...",
                            device.address()
                        );
                        excluded_addrs.insert(device.address());
                        continue 'discover;
                    }
                }
                Err(e) => tracing::warn!("Session error: {e}"),
            }

            // Update D-Bus state
            let _ = state_tx.send(atvv::State::Disconnected);

            // Wait for device to reconnect (C1: interruptible by ctrl+c)
            tokio::select! {
                _ = ensure_connected(&device) => {
                    tracing::info!("Device reconnected");
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Interrupted during reconnection, shutting down");
                    break 'discover;
                }
            }

            // Re-resolve characteristics (handles may change after reconnect)
            chars = loop {
                tokio::select! {
                    result = ble::resolve_chars(&device) => {
                        match result {
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
                    }
                    _ = tokio::signal::ctrl_c() => {
                        tracing::info!("Interrupted during characteristic resolution, shutting down");
                        break 'discover;
                    }
                }
            };
        }
    }

    Ok(())
}
