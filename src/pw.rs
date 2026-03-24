//! PipeWire virtual audio source.
//!
//! Runs on a dedicated OS thread (not tokio-compatible). Receives decoded PCM
//! frames via [`std::sync::mpsc`] and pushes them to PipeWire as a virtual
//! microphone source.
//!
//! Uses `MainLoopRc` so the shutdown handler can clone the mainloop reference,
//! and normal Rust drop order handles teardown after `mainloop.run()` returns.

use std::io;
use std::sync::mpsc;

use pipewire::context::ContextRc;
use pipewire::keys;
use pipewire::main_loop::MainLoopRc;
use pipewire::properties::properties;
use pipewire::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Object, Pod, Value};
use pipewire::spa::utils::{Direction, SpaTypes};
use pipewire::stream::{Stream, StreamFlags, StreamRc};

/// Sample rate for ATVV audio (8 kHz mono).
const SAMPLE_RATE: u32 = 8000;

/// Number of audio channels (mono).
const CHANNELS: u32 = 1;

/// Size of one sample in bytes (i16 = 2 bytes).
const SAMPLE_SIZE: usize = std::mem::size_of::<i16>();

/// Signal to shut down the PipeWire source cleanly.
pub struct Shutdown;

/// Run the PipeWire audio source on the current thread (blocking).
///
/// Reads decoded PCM frames from `audio_rx` and pushes them to PipeWire as a
/// virtual microphone. Returns when `shutdown_rx` receives a [`Shutdown`] message.
///
/// Call from a dedicated `std::thread::spawn`.
pub fn run_pw_source(
    audio_rx: mpsc::Receiver<Vec<i16>>,
    gain_db: f32,
    node_name: &str,
    node_description: &str,
    shutdown_rx: pipewire::channel::Receiver<Shutdown>,
) -> Result<(), pipewire::Error> {
    pipewire::init();

    let mainloop = MainLoopRc::new(None)?;

    // Shared slot for the stream — set after creation, used by shutdown handler.
    let stream_slot: std::rc::Rc<std::cell::RefCell<Option<StreamRc>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let _receiver = shutdown_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        let stream_slot = stream_slot.clone();
        move |_: Shutdown| {
            tracing::info!("PipeWire source shutting down");
            // Disconnect stream while mainloop is alive (cpal pattern).
            if let Some(stream) = stream_slot.borrow().as_ref() {
                let _ = stream.disconnect();
            }
            mainloop.quit();
        }
    });

    let context = ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let stream = StreamRc::new(
        core,
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

    // Give the shutdown handler a reference to the stream.
    *stream_slot.borrow_mut() = Some(stream.clone());

    // Buffer of pending PCM samples not yet consumed by PipeWire callbacks.
    let pending: std::cell::RefCell<Vec<i16>> = std::cell::RefCell::new(Vec::new());

    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(|_, _, old, new| {
            tracing::debug!("PipeWire stream state: {old:?} -> {new:?}");
        })
        .process(move |stream: &Stream, _| {
            {
                let mut buf = pending.borrow_mut();
                loop {
                    match audio_rx.try_recv() {
                        Ok(mut frame) => {
                            crate::adpcm::apply_gain(&mut frame, gain_db);
                            buf.extend_from_slice(&frame);
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => break,
                    }
                }
            }

            let Some(mut pw_buf) = stream.dequeue_buffer() else {
                return;
            };

            let requested = pw_buf.requested() as usize;

            let Some(pw_data) = pw_buf.datas_mut().first_mut() else {
                return;
            };

            let Some(slice) = pw_data.data() else {
                return;
            };

            let buf_capacity = slice.len() / SAMPLE_SIZE;
            let max_samples = if requested > 0 {
                buf_capacity.min(requested)
            } else {
                buf_capacity
            };

            let mut buf = pending.borrow_mut();
            let available = buf.len().min(max_samples);

            for (i, &sample) in buf.iter().take(available).enumerate() {
                let bytes = sample.to_le_bytes();
                let offset = i * SAMPLE_SIZE;
                slice[offset] = bytes[0];
                slice[offset + 1] = bytes[1];
            }

            let silence_start = available * SAMPLE_SIZE;
            let silence_end = max_samples * SAMPLE_SIZE;
            for byte in &mut slice[silence_start..silence_end] {
                *byte = 0;
            }

            if available > 0 {
                buf.drain(..available);
            }

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

    stream.connect(
        Direction::Output,
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    tracing::info!("PipeWire source running (8kHz S16LE mono, gain={gain_db}dB)");

    // Block until quit.
    mainloop.run();

    // Follow cpal's exact drop pattern: explicitly drop listener and context
    // first, let stream, _receiver, and mainloop drop at function scope end.
    // Do NOT call pipewire::deinit() — it's process-global and we may create
    // another PW thread on reconnect.
    drop(_listener);
    drop(context);

    tracing::info!("PipeWire source stopped");
    Ok(())
}
