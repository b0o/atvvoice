//! PipeWire virtual audio source.
//!
//! Runs on a dedicated OS thread (not tokio-compatible). Receives decoded PCM
//! frames via [`std::sync::mpsc`] and pushes them to PipeWire as a virtual
//! microphone source.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use pipewire::keys;
use pipewire::properties::properties;
use pipewire::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Object, Pod, Value};
use pipewire::spa::utils::{Direction, SpaTypes};
use pipewire::stream::{Stream, StreamFlags};

/// Sample rate for ATVV audio (8 kHz mono).
const SAMPLE_RATE: u32 = 8000;

/// Number of audio channels (mono).
const CHANNELS: u32 = 1;

/// Size of one sample in bytes (i16 = 2 bytes).
const SAMPLE_SIZE: usize = std::mem::size_of::<i16>();

/// Run the PipeWire audio source on the current thread (blocking).
///
/// Reads decoded PCM frames from `audio_rx` and pushes them to PipeWire as a
/// virtual microphone. Each received `Vec<i16>` is one decoded ATVV frame
/// (typically 257 samples at 8 kHz).
///
/// Gain (in dB) is applied to samples before writing to PipeWire.
///
/// # Threading
///
/// This function blocks the calling thread by running the PipeWire main loop.
/// Call from a dedicated `std::thread::spawn`.
/// Runs the PipeWire audio source. Returns when the `audio_rx` channel
/// disconnects (all senders dropped).
pub fn run_pw_source(
    audio_rx: mpsc::Receiver<Vec<i16>>,
    gain_db: f32,
    node_name: &str,
    node_description: &str,
) -> Result<(), pipewire::Error> {
    pipewire::init();

    let mainloop = pipewire::main_loop::MainLoop::new(None)?;
    let context = pipewire::context::Context::new(&mainloop)?;
    let core = context.connect(None)?;

    // Create the stream with media properties so it appears as a microphone
    // source in PipeWire/PulseAudio desktop audio settings.
    let stream = Stream::new(
        &core,
        node_name,
        properties! {
            *keys::MEDIA_TYPE => "Audio",
            *keys::MEDIA_CATEGORY => "Capture",
            *keys::MEDIA_CLASS => "Audio/Source",
            *keys::MEDIA_ROLE => "Communication",
            *keys::NODE_NAME => node_name,
            *keys::NODE_DESCRIPTION => node_description,
        },
    )?;

    // Buffer of pending PCM samples not yet consumed by PipeWire callbacks.
    // The process callback drains from the front.
    let pending: std::cell::RefCell<Vec<i16>> = std::cell::RefCell::new(Vec::new());

    // Shared flag: process callback sets this when the audio channel disconnects.
    // A timer on the mainloop checks it and performs orderly teardown.
    let should_quit = Arc::new(AtomicBool::new(false));
    let should_quit_cb = should_quit.clone();

    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(|_, _, old, new| {
            tracing::debug!("PipeWire stream state: {old:?} -> {new:?}");
        })
        .process(move |stream, _| {
            // Drain any newly arrived frames from the channel into our pending buffer.
            {
                let mut buf = pending.borrow_mut();
                loop {
                    match audio_rx.try_recv() {
                        Ok(mut frame) => {
                            crate::adpcm::apply_gain(&mut frame, gain_db);
                            buf.extend_from_slice(&frame);
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            should_quit_cb.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            }

            let Some(mut pw_buf) = stream.dequeue_buffer() else {
                return;
            };

            // How many frames (samples) PipeWire wants this cycle.
            // A value of 0 means "no preference" — use the full buffer capacity.
            let requested = pw_buf.requested() as usize;

            let Some(pw_data) = pw_buf.datas_mut().first_mut() else {
                return;
            };

            let Some(slice) = pw_data.data() else {
                return;
            };

            // Cap to the actual mapped buffer capacity.
            let buf_capacity = slice.len() / SAMPLE_SIZE;
            let max_samples = if requested > 0 {
                buf_capacity.min(requested)
            } else {
                buf_capacity
            };

            let mut buf = pending.borrow_mut();
            let available = buf.len().min(max_samples);

            // Write available samples as little-endian i16 into the PipeWire buffer.
            for (i, &sample) in buf.iter().take(available).enumerate() {
                let bytes = sample.to_le_bytes();
                let offset = i * SAMPLE_SIZE;
                slice[offset] = bytes[0];
                slice[offset + 1] = bytes[1];
            }

            // Fill remaining with silence (zeros).
            let silence_start = available * SAMPLE_SIZE;
            let silence_end = max_samples * SAMPLE_SIZE;
            for byte in &mut slice[silence_start..silence_end] {
                *byte = 0;
            }

            // Consume the samples we wrote.
            if available > 0 {
                buf.drain(..available);
            }

            // Tell PipeWire how much data we produced.
            let chunk = pw_data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = SAMPLE_SIZE as i32;
            *chunk.size_mut() = (max_samples * SAMPLE_SIZE) as u32;
        })
        .register()?;

    // Build the SPA audio format pod for format negotiation.
    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::S16LE);
    audio_info.set_rate(SAMPLE_RATE);
    audio_info.set_channels(CHANNELS);

    // Set channel position to MONO so PipeWire routes to both L+R.
    // Without this, PipeWire defaults to FL (front left) = left channel only.
    let mut position = [0u32; 64];
    position[0] = pipewire::spa::sys::SPA_AUDIO_CHANNEL_MONO;
    audio_info.set_position(position);

    let obj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = PodSerializer::serialize(
        io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    )
    .expect("failed to serialize audio format pod")
    .0
    .into_inner();

    let mut params = [Pod::from_bytes(&values).expect("invalid pod bytes")];

    // Direction::Output = audio source (we output data TO PipeWire).
    stream.connect(
        Direction::Output,
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    tracing::info!("PipeWire source running (8kHz S16LE mono, gain={gain_db}dB)");

    // Poll for shutdown every 100ms. When the process callback signals disconnect,
    // we disconnect the stream (removing it from PipeWire) while the loop is still
    // alive, then quit. This avoids the pw_loop_check SEGV that happens if
    // Stream is dropped after MainLoop has exited.
    let timer = mainloop.loop_().add_timer({
        let should_quit = should_quit.clone();
        let mainloop_weak = mainloop.downgrade();
        move |_| {
            if should_quit.load(Ordering::Relaxed) {
                tracing::info!("Audio channel disconnected, stopping PipeWire source");
                if let Some(ml) = mainloop_weak.upgrade() {
                    ml.quit();
                }
            }
        }
    });
    timer.update_timer(
        Some(std::time::Duration::from_millis(100)),
        Some(std::time::Duration::from_millis(100)),
    );

    // Blocks until quit is called from the timer callback.
    mainloop.run();

    // Orderly teardown while mainloop infrastructure is still intact.
    // disconnect() removes the stream from PipeWire's graph (node disappears
    // from pavucontrol). Then normal Rust drop order handles the rest.
    let _ = stream.disconnect();
    drop(_listener);
    drop(timer);
    drop(stream);
    drop(core);
    drop(context);
    // MainLoop is dropped last (implicit).

    tracing::info!("PipeWire source stopped");
    unsafe { pipewire::deinit() };

    Ok(())
}
