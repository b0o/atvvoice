use std::pin::Pin;
use std::time::Duration;

use anyhow::Result;
use futures::{Stream, StreamExt};
use tokio::sync::mpsc;
use tokio::time::{self, Instant};

use crate::protocol::types::{AudioFrame, AudioStopReason, CtlEvent, StreamId};
use crate::protocol::Protocol;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Disconnected,
    Connected,
    Opening,
    Streaming,
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected => write!(f, "disconnected"),
            Self::Connected => write!(f, "connected"),
            Self::Opening => write!(f, "opening"),
            Self::Streaming => write!(f, "streaming"),
        }
    }
}

/// Commands that can be sent from external sources (D-Bus, CLI, etc.).
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
#[allow(clippy::enum_variant_names)]
pub enum ExternalCommand {
    MicOpen,
    MicClose,
    MicToggle,
}

/// Session timeout configuration.
#[derive(Debug, Clone)]
pub struct SessionTimeouts {
    /// Auto-close mic if no frames arrive for this long (device went to sleep).
    /// Resets on every received audio frame. `0` = disabled.
    pub frame_timeout: Duration,
    /// Auto-close mic this long after the last mic button press (`START_SEARCH`).
    /// Does not reset on other remote buttons (volume, navigation, etc.). `0` = disabled.
    pub idle_timeout: Duration,
    /// Re-send keepalive at this interval to reset the remote's audio transfer
    /// timeout (spec §4.6.1). `0` = disabled.
    pub keepalive: Duration,
}

/// Events from the device (connection state changes).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DeviceConnectionEvent {
    Disconnected,
}

/// A boxed future that borrows `self` and returns `T`.
pub type BleFut<'a, T> = Pin<Box<dyn std::future::Future<Output = Result<T>> + Send + 'a>>;

/// A boxed async stream of `T`.
pub type BleStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;

/// BLE notification streams for an active session.
/// Created during negotiation and passed to `run_session`.
pub struct BleStreams {
    pub ctl: BleStream<Vec<u8>>,
    pub rx: BleStream<Vec<u8>>,
    pub events: BleStream<DeviceConnectionEvent>,
}

/// Abstraction over BLE device operations for testability.
pub trait BleDevice: Send {
    /// Write a command to the TX characteristic.
    fn write_command(&self, data: &[u8]) -> BleFut<'_, ()>;

    /// Get a stream of CTL notifications.
    fn ctl_notifications(&self) -> BleFut<'_, BleStream<Vec<u8>>>;

    /// Get a stream of RX (audio) notifications.
    fn rx_notifications(&self) -> BleFut<'_, BleStream<Vec<u8>>>;

    /// Get a stream of device connection events.
    fn connection_events(&self) -> BleFut<'_, BleStream<DeviceConnectionEvent>>;
}

/// Configuration and channels for a session.
pub struct SessionConfig<'a> {
    pub audio_tx: mpsc::Sender<AudioFrame>,
    pub timeouts: &'a SessionTimeouts,
    pub command_rx: Option<&'a mut mpsc::Receiver<ExternalCommand>>,
    pub state_tx: Option<&'a tokio::sync::watch::Sender<State>>,
}

/// Run the ATVV protocol session.
///
/// Audio frames (decoded PCM) are sent to `config.audio_tx`.
/// External commands (e.g. from D-Bus) are received on `config.command_rx`.
/// State changes are broadcast via `config.state_tx` (for D-Bus signals, etc.).
/// Returns on device disconnect or unrecoverable BLE error.
///
/// # Errors
///
/// Returns an error if a BLE write to the TX characteristic fails (e.g. the
/// device disconnects mid-command). Timeout-triggered `MIC_CLOSE` writes are
/// best-effort and do not propagate errors.
pub async fn run_session(
    ble: &(impl BleDevice + ?Sized),
    protocol: &mut dyn Protocol,
    streams: BleStreams,
    config: SessionConfig<'_>,
) -> Result<()> {
    let SessionConfig {
        audio_tx,
        timeouts,
        mut command_rx,
        state_tx,
    } = config;
    let mut state = State::Connected;

    // set_state takes &mut State as a parameter rather than capturing it because
    // `state` is used independently in the tokio::select! branches below. Capturing
    // it mutably would prevent its use in match guards and conditions.
    let set_state = |s: State, state: &mut State| {
        *state = s;
        if let Some(tx) = state_tx {
            if tx.send(s).is_err() {
                tracing::debug!("state_tx: no receivers");
            }
        }
    };

    // Helper: reset session state after mic close. Used in multiple `tokio::select!`
    // match arms where the pattern (set state → Connected, clear seq/stream_id) repeats.
    // Inlines `set_state` logic to avoid borrow conflicts.
    macro_rules! close_mic_reset {
        ($state:ident, $last_seq:ident, $current_stream_id:ident) => {
            set_state(State::Connected, &mut $state);
            $last_seq = None;
            $current_stream_id = None;
        };
    }

    // Destructure pre-subscribed BLE streams
    let ctl_stream = streams.ctl;
    let rx_stream = streams.rx;
    let device_events = streams.events;
    tokio::pin!(ctl_stream);
    tokio::pin!(rx_stream);
    tokio::pin!(device_events);

    let mut last_seq: Option<u16> = None;

    // Track the current stream ID (set on AudioStart, used for MIC_CLOSE/keepalive).
    let mut current_stream_id: Option<StreamId> = None;

    // Keepalive: reset the remote's audio transfer timeout (spec §4.6.1)
    // using protocol.keepalive_cmd() (MIC_EXTEND for v1.0, MIC_OPEN for v0.4).
    let keepalive_interval = timeouts.keepalive;
    let keepalive_enabled = !keepalive_interval.is_zero();
    let keepalive_timer = time::sleep(keepalive_interval);
    tokio::pin!(keepalive_timer);

    // Frame timeout: reset on every audio frame. Detects device going to sleep
    // so the next button press starts cleanly (no double-press needed).
    let frame_timer = time::sleep(timeouts.frame_timeout);
    tokio::pin!(frame_timer);
    let frame_timeout_enabled = !timeouts.frame_timeout.is_zero();

    // Idle timeout: reset on every START_SEARCH (button press). Detects "user
    // forgot the mic is on" - auto-closes after configured inactivity period.
    let idle_timer = time::sleep(timeouts.idle_timeout);
    tokio::pin!(idle_timer);
    let idle_timeout_enabled = !timeouts.idle_timeout.is_zero();

    loop {
        tokio::select! {
            Some(data) = ctl_stream.next() => {
                if data.is_empty() {
                    continue;
                }
                match protocol.parse_ctl(&data) {
                    CtlEvent::AudioStop { reason } => {
                        tracing::info!("AUDIO_STOP (reason={:?})", reason);
                        // For HTT button release, remote already stopped — don't send `MIC_CLOSE`
                        if !matches!(reason, AudioStopReason::HttButtonRelease)
                            && state == State::Streaming
                        {
                            let sid = current_stream_id.unwrap_or(StreamId::MIC_OPEN);
                            ble.write_command(&protocol.mic_close_cmd(sid)).await?;
                            tracing::info!("Sent MIC_CLOSE");
                        }
                        close_mic_reset!(state, last_seq, current_stream_id);
                    }
                    CtlEvent::AudioStart { reason, codec, stream_id } => {
                        tracing::info!("AUDIO_START (reason={:?}, codec={:?}, stream_id={:?})", reason, codec, stream_id);
                        // Remote resets its sequence counter on every AUDIO_START
                        last_seq = None;
                        current_stream_id = Some(stream_id);
                        set_state(State::Streaming, &mut state);
                        // Reset keepalive timer when streaming starts/restarts
                        if keepalive_enabled {
                            keepalive_timer.as_mut().reset(Instant::now() + keepalive_interval);
                        }
                    }
                    CtlEvent::StartSearch => {
                        tracing::info!("START_SEARCH (state={:?})", state);

                        // Reset idle timer on user activity (button press)
                        if idle_timeout_enabled {
                            idle_timer.as_mut().reset(Instant::now() + timeouts.idle_timeout);
                        }

                        if state == State::Streaming || state == State::Opening {
                            // Toggle: second press stops streaming
                            let sid = current_stream_id.unwrap_or(StreamId::MIC_OPEN);
                            ble.write_command(&protocol.mic_close_cmd(sid)).await?;
                            tracing::info!("Sent MIC_CLOSE (toggle off)");
                            close_mic_reset!(state, last_seq, current_stream_id);
                        } else {
                            ble.write_command(&protocol.mic_open_cmd()).await?;
                            tracing::info!("Sent MIC_OPEN");
                            set_state(State::Opening, &mut state);
                            if frame_timeout_enabled {
                                frame_timer.as_mut().reset(Instant::now() + timeouts.frame_timeout);
                            }
                        }
                    }
                    CtlEvent::AudioSync(ref sync) => {
                        tracing::debug!("AUDIO_SYNC: {:?}", sync);
                        protocol.on_audio_sync(sync);
                    }
                    CtlEvent::CapsResp(caps) => {
                        tracing::debug!("Ignoring unsolicited CAPS_RESP: {:?}", caps.version);
                    }
                    CtlEvent::MicOpenError(code) => {
                        tracing::warn!("MIC_OPEN_ERROR: {:?}", code);
                        if state == State::Opening {
                            set_state(State::Connected, &mut state);
                            current_stream_id = None;
                        }
                    }
                    CtlEvent::Unknown(ref raw) => {
                        tracing::debug!("Unknown CTL: {:02x?}", raw);
                    }
                }
            }
            Some(data) = rx_stream.next() => {
                if state == State::Streaming {
                    if let Some(frame) = protocol.decode_audio(&data) {
                        // Reset frame timer on every audio frame
                        if frame_timeout_enabled {
                            frame_timer.as_mut().reset(Instant::now() + timeouts.frame_timeout);
                        }

                        // Sequence gap detection
                        if let Some(last) = last_seq {
                            let expected = last.wrapping_add(1);
                            if frame.seq != expected {
                                tracing::warn!(
                                    "Sequence gap: expected {}, got {} (dropped {} frames)",
                                    expected,
                                    frame.seq,
                                    frame.seq.wrapping_sub(expected)
                                );
                            }
                        }
                        last_seq = Some(frame.seq);

                        if audio_tx.send(frame).await.is_err() {
                            tracing::warn!("audio frame dropped: receiver channel full or closed");
                        }
                    }
                }
            }
            // External commands (D-Bus, etc.)
            Some(cmd) = async {
                match command_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                tracing::info!("External command: {:?} (state={:?})", cmd, state);
                let sid = current_stream_id.unwrap_or(StreamId::MIC_OPEN);
                match cmd {
                    ExternalCommand::MicOpen => {
                        if state == State::Connected {
                            ble.write_command(&protocol.mic_open_cmd()).await?;
                            tracing::info!("Sent MIC_OPEN (external)");
                            set_state(State::Opening, &mut state);
                            if frame_timeout_enabled {
                                frame_timer.as_mut().reset(Instant::now() + timeouts.frame_timeout);
                            }
                        }
                    }
                    ExternalCommand::MicClose => {
                        if state == State::Streaming || state == State::Opening {
                            ble.write_command(&protocol.mic_close_cmd(sid)).await?;
                            tracing::info!("Sent MIC_CLOSE (external)");
                            close_mic_reset!(state, last_seq, current_stream_id);
                        }
                    }
                    ExternalCommand::MicToggle => {
                        if state == State::Streaming || state == State::Opening {
                            ble.write_command(&protocol.mic_close_cmd(sid)).await?;
                            tracing::info!("Sent MIC_CLOSE (external toggle)");
                            close_mic_reset!(state, last_seq, current_stream_id);
                        } else if state == State::Connected {
                            ble.write_command(&protocol.mic_open_cmd()).await?;
                            tracing::info!("Sent MIC_OPEN (external toggle)");
                            set_state(State::Opening, &mut state);
                            if frame_timeout_enabled {
                                frame_timer.as_mut().reset(Instant::now() + timeouts.frame_timeout);
                            }
                        }
                    }
                }
            }
            // Keepalive: protocol handles the version-appropriate command
            // (MIC_EXTEND for v1.0, MIC_OPEN for v0.4).
            () = &mut keepalive_timer, if keepalive_enabled && state == State::Streaming => {
                let sid = current_stream_id.unwrap_or(StreamId::MIC_OPEN);
                tracing::debug!("Sending keepalive");
                let _ = ble.write_command(&protocol.keepalive_cmd(sid)).await;
                keepalive_timer.as_mut().reset(Instant::now() + keepalive_interval);
            }
            () = &mut frame_timer, if frame_timeout_enabled && (state == State::Streaming || state == State::Opening) => {
                tracing::info!("Frame timeout ({:?}) - device likely asleep, closing mic", timeouts.frame_timeout);
                let sid = current_stream_id.unwrap_or(StreamId::MIC_OPEN);
                let _ = ble.write_command(&protocol.mic_close_cmd(sid)).await;
                close_mic_reset!(state, last_seq, current_stream_id);
            }
            () = &mut idle_timer, if idle_timeout_enabled && (state == State::Streaming || state == State::Opening) => {
                tracing::info!("Idle timeout ({:?}) - no button activity, closing mic", timeouts.idle_timeout);
                let sid = current_stream_id.unwrap_or(StreamId::MIC_OPEN);
                let _ = ble.write_command(&protocol.mic_close_cmd(sid)).await;
                close_mic_reset!(state, last_seq, current_stream_id);
            }
            Some(event) = device_events.next() => {
                match event {
                    DeviceConnectionEvent::Disconnected => {
                        tracing::info!("Device disconnected");
                        break;
                    }
                }
            }
            else => {
                tracing::info!("Notification streams ended");
                break;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::{
        AudioFrameSize, Capabilities, Codecs, InteractionModel, ProtocolVersion,
    };
    use crate::protocol::v04::ProtocolV04;
    use tokio::sync::mpsc as tokio_mpsc;
    use tokio::sync::Mutex;
    use tokio_stream::wrappers::UnboundedReceiverStream;

    /// Mock BLE device for testing the ATVV state machine.
    struct MockBleDevice {
        /// Commands written by the state machine (for assertion).
        commands: tokio_mpsc::UnboundedSender<Vec<u8>>,
        /// Inject CTL notifications.
        ctl_rx: Mutex<Option<tokio_mpsc::UnboundedReceiver<Vec<u8>>>>,
        /// Inject RX (audio) notifications.
        rx_rx: Mutex<Option<tokio_mpsc::UnboundedReceiver<Vec<u8>>>>,
        /// Inject device events.
        event_rx: Mutex<Option<tokio_mpsc::UnboundedReceiver<DeviceConnectionEvent>>>,
    }

    struct MockControls {
        commands_rx: tokio_mpsc::UnboundedReceiver<Vec<u8>>,
        ctl_tx: tokio_mpsc::UnboundedSender<Vec<u8>>,
        rx_tx: tokio_mpsc::UnboundedSender<Vec<u8>>,
        event_tx: tokio_mpsc::UnboundedSender<DeviceConnectionEvent>,
    }

    fn mock_device() -> (MockBleDevice, MockControls) {
        let (commands_tx, commands_rx) = tokio_mpsc::unbounded_channel();
        let (ctl_tx, ctl_rx) = tokio_mpsc::unbounded_channel();
        let (rx_tx, rx_rx) = tokio_mpsc::unbounded_channel();
        let (event_tx, event_rx) = tokio_mpsc::unbounded_channel();

        let device = MockBleDevice {
            commands: commands_tx,
            ctl_rx: Mutex::new(Some(ctl_rx)),
            rx_rx: Mutex::new(Some(rx_rx)),
            event_rx: Mutex::new(Some(event_rx)),
        };

        let controls = MockControls {
            commands_rx,
            ctl_tx,
            rx_tx,
            event_tx,
        };

        (device, controls)
    }

    impl BleDevice for MockBleDevice {
        fn write_command(&self, data: &[u8]) -> BleFut<'_, ()> {
            let data = data.to_vec();
            Box::pin(async move {
                self.commands
                    .send(data)
                    .map_err(|e| anyhow::anyhow!("mock send error: {}", e))?;
                Ok(())
            })
        }

        fn ctl_notifications(&self) -> BleFut<'_, BleStream<Vec<u8>>> {
            Box::pin(async {
                let rx = self
                    .ctl_rx
                    .lock()
                    .await
                    .take()
                    .expect("ctl_notifications called more than once");
                Ok(Box::pin(UnboundedReceiverStream::new(rx)) as BleStream<Vec<u8>>)
            })
        }

        fn rx_notifications(&self) -> BleFut<'_, BleStream<Vec<u8>>> {
            Box::pin(async {
                let rx = self
                    .rx_rx
                    .lock()
                    .await
                    .take()
                    .expect("rx_notifications called more than once");
                Ok(Box::pin(UnboundedReceiverStream::new(rx)) as BleStream<Vec<u8>>)
            })
        }

        fn connection_events(&self) -> BleFut<'_, BleStream<DeviceConnectionEvent>> {
            Box::pin(async {
                let rx = self
                    .event_rx
                    .lock()
                    .await
                    .take()
                    .expect("connection_events called more than once");
                Ok(
                    Box::pin(UnboundedReceiverStream::new(rx))
                        as BleStream<DeviceConnectionEvent>,
                )
            })
        }
    }

    /// Helper: make a fake 134-byte audio frame with a given sequence ID.
    /// Uses v0.4 format: 6-byte header + 128 ADPCM bytes.
    fn make_audio_frame(seq: u16) -> Vec<u8> {
        let mut frame = vec![0u8; 134];
        let seq_bytes = seq.to_be_bytes();
        frame[0] = seq_bytes[0];
        frame[1] = seq_bytes[1];
        frame
    }

    /// Helper: receive all currently buffered commands from the mock controls.
    async fn try_recv_all_commands(rx: &mut tokio_mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<Vec<u8>> {
        let mut cmds = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            cmds.push(cmd);
        }
        cmds
    }

    /// Helper: wait for a specific state on the watch channel (with timeout).
    async fn wait_for_state(
        state_rx: &mut tokio::sync::watch::Receiver<State>,
        expected: State,
        timeout: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if *state_rx.borrow() == expected {
                return true;
            }
            tokio::select! {
                result = state_rx.changed() => {
                    if result.is_err() {
                        return false;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return *state_rx.borrow() == expected;
                }
            }
        }
    }

    const V04_MIC_OPEN: &[u8] = &[0x0C, 0x00, 0x01];
    const V04_MIC_CLOSE: &[u8] = &[0x0D];

    /// CTL opcodes for test injection
    const CTL_AUDIO_START: u8 = 0x04;
    const CTL_START_SEARCH: u8 = 0x08;
    const CTL_AUDIO_STOP: u8 = 0x00;

    // ── Test 1: Frame timeout reset regression ──────────────────────────

    #[tokio::test]
    async fn test_frame_timeout_reset_regression() {
        tokio::time::pause();

        let (device, mut ctrl) = mock_device();
        let (audio_tx, _audio_rx) = tokio_mpsc::channel(64);
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(State::Connected);

        let timeouts = SessionTimeouts {
            frame_timeout: Duration::from_millis(100),
            idle_timeout: Duration::ZERO,
            keepalive: Duration::ZERO,
        };

        // Pre-initialize protocol with capabilities
        let caps = Capabilities {
            version: ProtocolVersion::V0_4,
            codecs: Codecs::ADPCM_8KHZ,
            interaction_model: InteractionModel::OnRequest,
            audio_frame_size: AudioFrameSize(134),
        };
        let mut protocol = ProtocolV04::new();
        protocol.on_caps_resp(&caps).unwrap();

        // Create BleStreams from mock (consumes receivers)
        let ctl = device.ctl_notifications().await.unwrap();
        let rx = device.rx_notifications().await.unwrap();
        let events = device.connection_events().await.unwrap();
        let streams = BleStreams { ctl, rx, events };

        let session = tokio::spawn(async move {
            run_session(
                &device,
                &mut protocol,
                streams,
                SessionConfig {
                    audio_tx,

                    timeouts: &timeouts,
                    command_rx: None,
                    state_tx: Some(&state_tx),
                },
            )
            .await
        });

        // No GET_CAPS to drain -- session starts in Ready
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Connected, Duration::from_millis(10)).await);

        // START_SEARCH → Opening
        ctrl.ctl_tx.send(vec![CTL_START_SEARCH]).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Opening, Duration::from_millis(10)).await);
        try_recv_all_commands(&mut ctrl.commands_rx).await; // MIC_OPEN

        // AUDIO_START → Streaming
        ctrl.ctl_tx.send(vec![CTL_AUDIO_START]).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Streaming, Duration::from_millis(10)).await);

        // Send audio frames for 200ms (resets timer each time)
        for i in 0..6 {
            ctrl.rx_tx.send(make_audio_frame(i)).unwrap();
            tokio::time::advance(Duration::from_millis(33)).await;
        }
        // State should still be Streaming
        assert_eq!(*state_rx.borrow(), State::Streaming);

        // START_SEARCH → toggle off → Ready
        ctrl.ctl_tx.send(vec![CTL_START_SEARCH]).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Connected, Duration::from_millis(10)).await);
        try_recv_all_commands(&mut ctrl.commands_rx).await; // MIC_CLOSE

        // Wait 200ms - timer would have fired if not properly managed (but state is Ready,
        // so the guard `if frame_timeout_enabled && (state == Streaming || Opening)` prevents it)
        tokio::time::advance(Duration::from_millis(200)).await;
        assert_eq!(*state_rx.borrow(), State::Connected);

        // START_SEARCH → Opening again
        ctrl.ctl_tx.send(vec![CTL_START_SEARCH]).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Opening, Duration::from_millis(10)).await);
        try_recv_all_commands(&mut ctrl.commands_rx).await; // MIC_OPEN

        // Verify frame timeout does NOT fire immediately (wait 50ms, should still be Opening)
        tokio::time::advance(Duration::from_millis(50)).await;
        assert_eq!(*state_rx.borrow(), State::Opening);

        // Verify frame timeout DOES fire after the full 100ms from the new Opening
        tokio::time::advance(Duration::from_millis(60)).await;
        assert!(wait_for_state(&mut state_rx, State::Connected, Duration::from_millis(10)).await);

        // Clean up: disconnect
        ctrl.event_tx
            .send(DeviceConnectionEvent::Disconnected)
            .unwrap();
        let result = session.await.unwrap();
        assert!(result.is_ok());
    }

    // ── Test 2: Toggle mode state transitions ───────────────────────────

    #[tokio::test]
    async fn test_toggle_mode_state_transitions() {
        tokio::time::pause();

        let (device, mut ctrl) = mock_device();
        let (audio_tx, _audio_rx) = tokio_mpsc::channel(64);
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(State::Connected);

        let timeouts = SessionTimeouts {
            frame_timeout: Duration::ZERO,
            idle_timeout: Duration::ZERO,
            keepalive: Duration::ZERO,
        };

        let caps = Capabilities {
            version: ProtocolVersion::V0_4,
            codecs: Codecs::ADPCM_8KHZ,
            interaction_model: InteractionModel::OnRequest,
            audio_frame_size: AudioFrameSize(134),
        };
        let mut protocol = ProtocolV04::new();
        protocol.on_caps_resp(&caps).unwrap();

        let ctl = device.ctl_notifications().await.unwrap();
        let rx = device.rx_notifications().await.unwrap();
        let events = device.connection_events().await.unwrap();
        let streams = BleStreams { ctl, rx, events };

        let session = tokio::spawn(async move {
            run_session(
                &device,
                &mut protocol,
                streams,
                SessionConfig {
                    audio_tx,

                    timeouts: &timeouts,
                    command_rx: None,
                    state_tx: Some(&state_tx),
                },
            )
            .await
        });

        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Connected, Duration::from_millis(10)).await);

        // START_SEARCH from Ready → Opening, MIC_OPEN sent
        ctrl.ctl_tx.send(vec![CTL_START_SEARCH]).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Opening, Duration::from_millis(10)).await);
        let cmds = try_recv_all_commands(&mut ctrl.commands_rx).await;
        assert_eq!(cmds, vec![V04_MIC_OPEN.to_vec()]);

        // AUDIO_START → Streaming
        ctrl.ctl_tx.send(vec![CTL_AUDIO_START]).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Streaming, Duration::from_millis(10)).await);

        // START_SEARCH from Streaming → Ready (toggle off), MIC_CLOSE sent (NOT MIC_OPEN)
        ctrl.ctl_tx.send(vec![CTL_START_SEARCH]).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Connected, Duration::from_millis(10)).await);
        let cmds = try_recv_all_commands(&mut ctrl.commands_rx).await;
        assert_eq!(cmds, vec![V04_MIC_CLOSE.to_vec()]);

        // START_SEARCH from Ready → Opening, MIC_OPEN sent again
        ctrl.ctl_tx.send(vec![CTL_START_SEARCH]).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Opening, Duration::from_millis(10)).await);
        let cmds = try_recv_all_commands(&mut ctrl.commands_rx).await;
        assert_eq!(cmds, vec![V04_MIC_OPEN.to_vec()]);

        // Clean up
        ctrl.event_tx
            .send(DeviceConnectionEvent::Disconnected)
            .unwrap();
        let result = session.await.unwrap();
        assert!(result.is_ok());
    }

    // ── Test 3: Disconnect detection ────────────────────────────────────

    #[tokio::test]
    async fn test_disconnect_detection() {
        tokio::time::pause();

        let (device, ctrl) = mock_device();
        let (audio_tx, _audio_rx) = tokio_mpsc::channel(64);
        let (state_tx, _state_rx) = tokio::sync::watch::channel(State::Connected);

        let timeouts = SessionTimeouts {
            frame_timeout: Duration::ZERO,
            idle_timeout: Duration::ZERO,
            keepalive: Duration::ZERO,
        };

        let caps = Capabilities {
            version: ProtocolVersion::V0_4,
            codecs: Codecs::ADPCM_8KHZ,
            interaction_model: InteractionModel::OnRequest,
            audio_frame_size: AudioFrameSize(134),
        };
        let mut protocol = ProtocolV04::new();
        protocol.on_caps_resp(&caps).unwrap();

        let ctl = device.ctl_notifications().await.unwrap();
        let rx = device.rx_notifications().await.unwrap();
        let events = device.connection_events().await.unwrap();
        let streams = BleStreams { ctl, rx, events };

        let session = tokio::spawn(async move {
            run_session(
                &device,
                &mut protocol,
                streams,
                SessionConfig {
                    audio_tx,

                    timeouts: &timeouts,
                    command_rx: None,
                    state_tx: Some(&state_tx),
                },
            )
            .await
        });

        tokio::time::advance(Duration::from_millis(1)).await;

        // Send disconnect event
        ctrl.event_tx
            .send(DeviceConnectionEvent::Disconnected)
            .unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;

        // Verify run_session returns Ok
        let result = session.await.unwrap();
        assert!(result.is_ok());
    }

    // ── Test 5: External commands ───────────────────────────────────────

    #[tokio::test]
    async fn test_external_commands() {
        tokio::time::pause();

        let (device, mut ctrl) = mock_device();
        let (audio_tx, _audio_rx) = tokio_mpsc::channel(64);
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(State::Connected);
        let (cmd_tx, mut cmd_rx) = tokio_mpsc::channel::<ExternalCommand>(16);

        let timeouts = SessionTimeouts {
            frame_timeout: Duration::ZERO,
            idle_timeout: Duration::ZERO,
            keepalive: Duration::ZERO,
        };

        let caps = Capabilities {
            version: ProtocolVersion::V0_4,
            codecs: Codecs::ADPCM_8KHZ,
            interaction_model: InteractionModel::OnRequest,
            audio_frame_size: AudioFrameSize(134),
        };
        let mut protocol = ProtocolV04::new();
        protocol.on_caps_resp(&caps).unwrap();

        let ctl = device.ctl_notifications().await.unwrap();
        let rx = device.rx_notifications().await.unwrap();
        let events = device.connection_events().await.unwrap();
        let streams = BleStreams { ctl, rx, events };

        let session = tokio::spawn(async move {
            run_session(
                &device,
                &mut protocol,
                streams,
                SessionConfig {
                    audio_tx,

                    timeouts: &timeouts,
                    command_rx: Some(&mut cmd_rx),
                    state_tx: Some(&state_tx),
                },
            )
            .await
        });

        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Connected, Duration::from_millis(10)).await);

        // MicOpen from Ready → Opening
        cmd_tx.send(ExternalCommand::MicOpen).await.unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Opening, Duration::from_millis(10)).await);
        let cmds = try_recv_all_commands(&mut ctrl.commands_rx).await;
        assert_eq!(cmds, vec![V04_MIC_OPEN.to_vec()]);

        // Transition to Streaming for close test
        ctrl.ctl_tx.send(vec![CTL_AUDIO_START]).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Streaming, Duration::from_millis(10)).await);

        // MicClose from Streaming → Ready
        cmd_tx.send(ExternalCommand::MicClose).await.unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Connected, Duration::from_millis(10)).await);
        let cmds = try_recv_all_commands(&mut ctrl.commands_rx).await;
        assert_eq!(cmds, vec![V04_MIC_CLOSE.to_vec()]);

        // MicToggle from Ready → Opening
        cmd_tx.send(ExternalCommand::MicToggle).await.unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Opening, Duration::from_millis(10)).await);
        let cmds = try_recv_all_commands(&mut ctrl.commands_rx).await;
        assert_eq!(cmds, vec![V04_MIC_OPEN.to_vec()]);

        // Transition to Streaming
        ctrl.ctl_tx.send(vec![CTL_AUDIO_START]).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Streaming, Duration::from_millis(10)).await);

        // MicToggle from Streaming → Ready
        cmd_tx.send(ExternalCommand::MicToggle).await.unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Connected, Duration::from_millis(10)).await);
        let cmds = try_recv_all_commands(&mut ctrl.commands_rx).await;
        assert_eq!(cmds, vec![V04_MIC_CLOSE.to_vec()]);

        // Clean up
        ctrl.event_tx
            .send(DeviceConnectionEvent::Disconnected)
            .unwrap();
        let result = session.await.unwrap();
        assert!(result.is_ok());
    }

    // ── Test 6: Idle timeout reset ──────────────────────────────────────

    #[tokio::test]
    async fn test_idle_timeout_reset() {
        tokio::time::pause();

        let (device, mut ctrl) = mock_device();
        let (audio_tx, _audio_rx) = tokio_mpsc::channel(64);
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(State::Connected);

        let timeouts = SessionTimeouts {
            frame_timeout: Duration::ZERO,
            idle_timeout: Duration::from_millis(100),
            keepalive: Duration::ZERO,
        };

        let caps = Capabilities {
            version: ProtocolVersion::V0_4,
            codecs: Codecs::ADPCM_8KHZ,
            interaction_model: InteractionModel::OnRequest,
            audio_frame_size: AudioFrameSize(134),
        };
        let mut protocol = ProtocolV04::new();
        protocol.on_caps_resp(&caps).unwrap();

        let ctl = device.ctl_notifications().await.unwrap();
        let rx = device.rx_notifications().await.unwrap();
        let events = device.connection_events().await.unwrap();
        let streams = BleStreams { ctl, rx, events };

        let session = tokio::spawn(async move {
            run_session(
                &device,
                &mut protocol,
                streams,
                SessionConfig {
                    audio_tx,

                    timeouts: &timeouts,
                    command_rx: None,
                    state_tx: Some(&state_tx),
                },
            )
            .await
        });

        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Connected, Duration::from_millis(10)).await);

        // START_SEARCH → Opening (resets idle timer)
        ctrl.ctl_tx.send(vec![CTL_START_SEARCH]).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Opening, Duration::from_millis(10)).await);
        try_recv_all_commands(&mut ctrl.commands_rx).await;

        // AUDIO_START → Streaming
        ctrl.ctl_tx.send(vec![CTL_AUDIO_START]).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Streaming, Duration::from_millis(10)).await);

        // Wait 50ms, then toggle off (resets idle timer)
        tokio::time::advance(Duration::from_millis(50)).await;
        ctrl.ctl_tx.send(vec![CTL_START_SEARCH]).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Connected, Duration::from_millis(10)).await);
        try_recv_all_commands(&mut ctrl.commands_rx).await;

        // Wait 50ms, then START_SEARCH again → Opening (resets idle timer)
        tokio::time::advance(Duration::from_millis(50)).await;
        ctrl.ctl_tx.send(vec![CTL_START_SEARCH]).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Opening, Duration::from_millis(10)).await);
        try_recv_all_commands(&mut ctrl.commands_rx).await;

        // Verify idle timer hasn't fired (was reset each time). Still Opening after 50ms.
        tokio::time::advance(Duration::from_millis(50)).await;
        assert_eq!(*state_rx.borrow(), State::Opening);

        // But it DOES fire at the full 100ms from the last reset
        tokio::time::advance(Duration::from_millis(60)).await;
        assert!(wait_for_state(&mut state_rx, State::Connected, Duration::from_millis(10)).await);

        // Clean up
        ctrl.event_tx
            .send(DeviceConnectionEvent::Disconnected)
            .unwrap();
        let result = session.await.unwrap();
        assert!(result.is_ok());
    }

    // ── Test 7: HTT button release does NOT send MIC_CLOSE ──────────

    #[tokio::test]
    async fn test_htt_button_release_skips_mic_close() {
        use crate::protocol::v10::ProtocolV10;

        tokio::time::pause();

        let (device, mut ctrl) = mock_device();
        let (audio_tx, _audio_rx) = tokio_mpsc::channel(64);
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(State::Connected);

        let timeouts = SessionTimeouts {
            frame_timeout: Duration::ZERO,
            idle_timeout: Duration::ZERO,
            keepalive: Duration::ZERO,
        };

        let caps = Capabilities {
            version: ProtocolVersion::V1_0,
            codecs: Codecs::ADPCM_8KHZ,
            interaction_model: InteractionModel::HoldToTalk,
            audio_frame_size: AudioFrameSize(20),
        };
        let mut protocol = ProtocolV10::new();
        protocol.on_caps_resp(&caps).unwrap();

        let ctl = device.ctl_notifications().await.unwrap();
        let rx = device.rx_notifications().await.unwrap();
        let events = device.connection_events().await.unwrap();
        let streams = BleStreams { ctl, rx, events };

        let session = tokio::spawn(async move {
            run_session(
                &device,
                &mut protocol,
                streams,
                SessionConfig {
                    audio_tx,

                    timeouts: &timeouts,
                    command_rx: None,
                    state_tx: Some(&state_tx),
                },
            )
            .await
        });

        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Connected, Duration::from_millis(10)).await);

        // START_SEARCH → Opening, MIC_OPEN sent
        ctrl.ctl_tx.send(vec![CTL_START_SEARCH]).unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Opening, Duration::from_millis(10)).await);
        try_recv_all_commands(&mut ctrl.commands_rx).await; // consume MIC_OPEN

        // v1.0 AUDIO_START: [0x04, reason=HoldToTalk(0x03), codec=0x01, stream_id=0x00]
        ctrl.ctl_tx
            .send(vec![CTL_AUDIO_START, 0x03, 0x01, 0x00])
            .unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Streaming, Duration::from_millis(10)).await);

        // AUDIO_STOP with reason=HttButtonRelease (0x02) → should NOT send MIC_CLOSE
        ctrl.ctl_tx
            .send(vec![CTL_AUDIO_STOP, 0x02])
            .unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_state(&mut state_rx, State::Connected, Duration::from_millis(10)).await);

        // Verify NO MIC_CLOSE was written
        let cmds = try_recv_all_commands(&mut ctrl.commands_rx).await;
        assert!(
            cmds.is_empty(),
            "Expected no commands after HttButtonRelease, got: {cmds:?}"
        );

        // Clean up
        ctrl.event_tx
            .send(DeviceConnectionEvent::Disconnected)
            .unwrap();
        let result = session.await.unwrap();
        assert!(result.is_ok());
    }
}
