use std::time::Duration;

use crate::ble::AtvvChars;

use anyhow::Result;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::time::{self, Instant};

// Commands (Host → Remote, written to TX)
pub const CMD_GET_CAPS: &[u8] = &[0x0A, 0x00, 0x04, 0x00, 0x01];
pub const CMD_MIC_OPEN: &[u8] = &[0x0C, 0x00, 0x01];
pub const CMD_MIC_CLOSE: &[u8] = &[0x0D];

// Control signals (Remote → Host, received on CTL)
const CTL_AUDIO_STOP: u8 = 0x00;
const CTL_AUDIO_START: u8 = 0x04;
const CTL_START_SEARCH: u8 = 0x08;
const CTL_GET_CAPS_RESP: u8 = 0x0B;

/// How the mic button behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum MicMode {
    /// Press to start, press again to stop (for remotes that toggle).
    Toggle,
    /// Hold to stream, release to stop (for remotes that send AUDIO_STOP on release).
    Hold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Init,
    Ready,
    Opening,
    Streaming,
}

/// Commands that can be sent from external sources (D-Bus, CLI, etc.).
#[derive(Debug, Clone, Copy)]
pub enum ExternalCommand {
    MicOpen,
    MicClose,
    MicToggle,
}

/// Session timeout configuration.
pub struct SessionTimeouts {
    /// Auto-close mic if no frames arrive for this long (device went to sleep).
    /// Resets on every received audio frame. 0 = disabled.
    pub frame_timeout: Duration,
    /// Auto-close mic this long after the last button press (user forgot mic is on).
    /// Resets on every START_SEARCH. 0 = disabled.
    pub idle_timeout: Duration,
}

/// Run the ATVV protocol session.
/// Audio frames are sent to `audio_tx`.
/// External commands (e.g. from D-Bus) are received on `command_rx`.
/// State changes are broadcast via `state_tx` (for D-Bus signals, etc.).
/// Returns on device disconnect or unrecoverable error.
pub async fn run_session(
    chars: &AtvvChars,
    audio_tx: mpsc::Sender<Vec<u8>>,
    mic_mode: MicMode,
    timeouts: &SessionTimeouts,
    mut command_rx: Option<&mut mpsc::Receiver<ExternalCommand>>,
    state_tx: Option<&tokio::sync::watch::Sender<State>>,
) -> Result<()> {
    #[allow(unused_assignments)] // Init is overwritten after GET_CAPS; kept for clarity
    let mut state = State::Init;

    // Helper to update state and notify observers.
    let set_state = |s: State, state: &mut State| {
        *state = s;
        if let Some(tx) = state_tx {
            let _ = tx.send(s);
        }
    };

    // Subscribe to notifications
    let ctl_stream = chars.ctl.notify().await?;
    let rx_stream = chars.rx.notify().await?;
    tokio::pin!(ctl_stream);
    tokio::pin!(rx_stream);

    // Send GET_CAPS
    chars.tx.write(CMD_GET_CAPS).await?;
    tracing::info!("Sent GET_CAPS");
    set_state(State::Ready, &mut state);

    let mut last_seq: Option<u16> = None;

    // Frame timeout: reset on every audio frame. Detects device going to sleep
    // so the next button press starts cleanly (no double-press needed).
    let frame_timer = time::sleep(timeouts.frame_timeout);
    tokio::pin!(frame_timer);
    let frame_timeout_enabled = !timeouts.frame_timeout.is_zero();

    // Idle timeout: reset on every START_SEARCH (button press). Detects "user
    // forgot the mic is on" — auto-closes after configured inactivity period.
    let idle_timer = time::sleep(timeouts.idle_timeout);
    tokio::pin!(idle_timer);
    let idle_timeout_enabled = !timeouts.idle_timeout.is_zero();

    loop {
        tokio::select! {
            Some(data) = ctl_stream.next() => {
                if data.is_empty() {
                    continue;
                }
                match data[0] {
                    CTL_AUDIO_STOP => {
                        tracing::info!("AUDIO_STOP");
                        if state == State::Streaming {
                            chars.tx.write(CMD_MIC_CLOSE).await?;
                            tracing::info!("Sent MIC_CLOSE");
                        }
                        set_state(State::Ready, &mut state);
                        last_seq = None;
                    }
                    CTL_AUDIO_START => {
                        tracing::info!("AUDIO_START");
                        set_state(State::Streaming, &mut state);
                    }
                    CTL_START_SEARCH => {
                        tracing::info!("START_SEARCH (state={:?}, mode={:?})", state, mic_mode);

                        // Reset idle timer on user activity (button press)
                        if idle_timeout_enabled {
                            idle_timer.as_mut().reset(Instant::now() + timeouts.idle_timeout);
                        }

                        if state == State::Streaming || state == State::Opening {
                            match mic_mode {
                                MicMode::Toggle => {
                                    // Toggle: second press stops streaming
                                    chars.tx.write(CMD_MIC_CLOSE).await?;
                                    tracing::info!("Sent MIC_CLOSE (toggle off)");
                                    set_state(State::Ready, &mut state);
                                    last_seq = None;
                                }
                                MicMode::Hold => {
                                    // Hold: second START_SEARCH while streaming = stop + re-open
                                    chars.tx.write(CMD_MIC_CLOSE).await?;
                                    tracing::info!("Sent MIC_CLOSE (re-open)");
                                    last_seq = None;
                                    chars.tx.write(CMD_MIC_OPEN).await?;
                                    tracing::info!("Sent MIC_OPEN");
                                    set_state(State::Opening, &mut state);
                                    if frame_timeout_enabled {
                                        frame_timer.as_mut().reset(Instant::now() + timeouts.frame_timeout);
                                    }
                                }
                            }
                        } else {
                            chars.tx.write(CMD_MIC_OPEN).await?;
                            tracing::info!("Sent MIC_OPEN");
                            set_state(State::Opening, &mut state);
                            if frame_timeout_enabled {
                                frame_timer.as_mut().reset(Instant::now() + timeouts.frame_timeout);
                            }
                        }
                    }
                    CTL_GET_CAPS_RESP => {
                        tracing::info!("GET_CAPS_RESP: {:02x?}", &data);
                    }
                    other => {
                        tracing::debug!("Unknown CTL: 0x{:02x} data={:02x?}", other, &data);
                    }
                }
            }
            Some(data) = rx_stream.next() => {
                if state == State::Streaming && data.len() == 134 {
                    // Reset frame timer on every audio frame
                    if frame_timeout_enabled {
                        frame_timer.as_mut().reset(Instant::now() + timeouts.frame_timeout);
                    }

                    // Sequence gap detection
                    let seq = u16::from_be_bytes([data[0], data[1]]);
                    if let Some(last) = last_seq {
                        let expected = last.wrapping_add(1);
                        if seq != expected {
                            tracing::warn!(
                                "Sequence gap: expected {}, got {} (dropped {} frames)",
                                expected,
                                seq,
                                seq.wrapping_sub(expected)
                            );
                        }
                    }
                    last_seq = Some(seq);

                    let _ = audio_tx.send(data).await;
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
                match cmd {
                    ExternalCommand::MicOpen => {
                        if state == State::Ready {
                            chars.tx.write(CMD_MIC_OPEN).await?;
                            tracing::info!("Sent MIC_OPEN (external)");
                            set_state(State::Opening, &mut state);
                            if frame_timeout_enabled {
                                frame_timer.as_mut().reset(Instant::now() + timeouts.frame_timeout);
                            }
                        }
                    }
                    ExternalCommand::MicClose => {
                        if state == State::Streaming || state == State::Opening {
                            chars.tx.write(CMD_MIC_CLOSE).await?;
                            tracing::info!("Sent MIC_CLOSE (external)");
                            set_state(State::Ready, &mut state);
                            last_seq = None;
                        }
                    }
                    ExternalCommand::MicToggle => {
                        if state == State::Streaming || state == State::Opening {
                            chars.tx.write(CMD_MIC_CLOSE).await?;
                            tracing::info!("Sent MIC_CLOSE (external toggle)");
                            set_state(State::Ready, &mut state);
                            last_seq = None;
                        } else if state == State::Ready {
                            chars.tx.write(CMD_MIC_OPEN).await?;
                            tracing::info!("Sent MIC_OPEN (external toggle)");
                            set_state(State::Opening, &mut state);
                            if frame_timeout_enabled {
                                frame_timer.as_mut().reset(Instant::now() + timeouts.frame_timeout);
                            }
                        }
                    }
                }
            }
            () = &mut frame_timer, if frame_timeout_enabled && (state == State::Streaming || state == State::Opening) => {
                tracing::info!("Frame timeout ({:?}) — device likely asleep, closing mic", timeouts.frame_timeout);
                let _ = chars.tx.write(CMD_MIC_CLOSE).await;
                set_state(State::Ready, &mut state);
                last_seq = None;
            }
            () = &mut idle_timer, if idle_timeout_enabled && (state == State::Streaming || state == State::Opening) => {
                tracing::info!("Idle timeout ({:?}) — no button activity, closing mic", timeouts.idle_timeout);
                let _ = chars.tx.write(CMD_MIC_CLOSE).await;
                set_state(State::Ready, &mut state);
                last_seq = None;
            }
            else => {
                tracing::info!("Notification streams ended");
                break;
            }
        }
    }

    Ok(())
}
