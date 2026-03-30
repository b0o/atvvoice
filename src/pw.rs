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
use pipewire::types::ObjectType;

/// Number of audio channels (mono).
const CHANNELS: u32 = 1;

/// Size of one sample in bytes (i16 = 2 bytes).
const SAMPLE_SIZE: usize = std::mem::size_of::<i16>();

/// Signal to shut down the PipeWire source cleanly.
#[derive(Debug)]
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
    sample_rate: u32,
    node_name: &str,
    node_description: &str,
    shutdown_rx: pipewire::channel::Receiver<Shutdown>,
    consumer_tx: Option<tokio::sync::mpsc::Sender<crate::consumer::ConsumerEvent>>,
) -> Result<(), pipewire::Error> {
    pipewire::init();

    let mainloop = MainLoopRc::new(None)?;

    // Shared slot for the stream - set after creation, used by shutdown handler.
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
    let core_clone = core.clone(); // Keep a clone for registry creation later

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

    // Precompute linear gain multiplier outside the RT callback to avoid
    // recomputing powf() on every frame.
    let gain_linear = 10f32.powf(gain_db / 20.0);

    /// Maximum pending samples before overflow truncation (~500ms at 16kHz).
    const MAX_PENDING: usize = 8000;

    // Track our PipeWire node ID once the stream reaches Paused/Streaming.
    let our_node_id: std::rc::Rc<std::cell::Cell<u32>> =
        std::rc::Rc::new(std::cell::Cell::new(u32::MAX));

    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let our_node_id = our_node_id.clone();
            let stream_for_id = stream.clone();
            move |_, _, old, new| {
                tracing::debug!("PipeWire stream state: {old:?} -> {new:?}");
                if matches!(
                    new,
                    pipewire::stream::StreamState::Paused
                        | pipewire::stream::StreamState::Streaming
                ) {
                    let id = stream_for_id.node_id();
                    if id != u32::MAX {
                        our_node_id.set(id);
                        tracing::debug!("PipeWire node ID: {id}");
                    }
                }
            }
        })
        .process(move |stream: &Stream, _| {
            // NOTE: Vec operations (extend, drain) allocate inside this RT callback.
            // At ~30 fps with 257 samples/frame, allocation pressure is negligible
            // and not a real-time concern for voice audio rates.
            let mut buf = pending.borrow_mut();

            loop {
                match audio_rx.try_recv() {
                    Ok(mut frame) => {
                        crate::adpcm::apply_gain_linear(&mut frame, gain_linear);
                        buf.extend_from_slice(&frame);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }

            // Bound pending buffer to prevent unbounded growth under backpressure.
            if buf.len() > MAX_PENDING {
                let dropped = buf.len() - MAX_PENDING;
                buf.drain(..dropped);
                tracing::warn!("audio buffer overflow: dropped {dropped} samples");
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

            let available = buf.len().min(max_samples);

            // Copy i16 samples to the output buffer as little-endian bytes.
            for (src, dst) in buf
                .iter()
                .take(available)
                .zip(slice.chunks_exact_mut(SAMPLE_SIZE))
            {
                dst.copy_from_slice(&src.to_le_bytes());
            }

            // Zero remaining buffer bytes (silence).
            let silence_start = available * SAMPLE_SIZE;
            let silence_end = max_samples * SAMPLE_SIZE;
            slice[silence_start..silence_end].fill(0);

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
    audio_info.set_rate(sample_rate);
    audio_info.set_channels(CHANNELS);

    let mut position = [0u32; 64];
    position[0] = pipewire::spa::sys::SPA_AUDIO_CHANNEL_MONO;
    audio_info.set_position(position);

    let obj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> =
        PodSerializer::serialize(io::Cursor::new(Vec::new()), &Value::Object(obj))
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

    tracing::info!(
        "PipeWire source running ({}kHz S16LE mono, gain={gain_db}dB)",
        sample_rate / 1000
    );

    // Registry link monitoring for --mic-on-demand.
    //
    // Tracks PipeWire Link objects that connect to our stream node. Excludes
    // passive links (pavucontrol peak meters, level meters) so they don't
    // trigger mic open or hold it open.
    //
    // Detection uses bound Link proxies: the registry global props don't include
    // `link.passive`, but the Link proxy's `info` event does. When a link to our
    // node appears, we bind its proxy and wait for the info callback to determine
    // if it's passive. This handles the race where link globals arrive before
    // their full properties are available.
    //
    // Detection: for each link to our node, we bind the OTHER node's proxy and
    // check its full info properties for `node.want-driver == "true"`. The
    // registry global only shows a subset of node properties; the bound Node
    // proxy's `info` event includes everything — specifically `stream.monitor`
    // which is set on peak detect streams (pavucontrol) but not real consumers.
    #[allow(clippy::type_complexity)]
    let (_registry, _registry_listener, _proxies) = if let Some(consumer_tx) = consumer_tx {
        let registry = core_clone.get_registry_rc()?;

        // Links to our node: link_id → other_node_id.
        let our_links: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<u32, u32>>> =
            std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()));
        // Whether each Stream/Input/Audio node is a monitor (from bound Node info).
        // true = monitor (don't count), false = real consumer.
        // Nodes not yet in the map haven't received info yet — treated as "not
        // monitor" so real consumers work before info arrives.
        let node_is_monitor: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<u32, bool>>> =
            std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()));
        // Last emitted consumer count (to avoid duplicate events).
        let last_count: std::rc::Rc<std::cell::Cell<u32>> =
            std::rc::Rc::new(std::cell::Cell::new(0));
        // Bound proxies (must stay alive): link proxies + node proxies with listeners.
        let proxies: std::rc::Rc<
            std::cell::RefCell<std::collections::HashMap<u32, Box<dyn std::any::Any>>>,
        > = std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()));

        // Count links where the other node is NOT a monitor. Emit Changed if count changed.
        let recount = {
            let our_links = our_links.clone();
            let node_is_monitor = node_is_monitor.clone();
            let last_count = last_count.clone();
            let consumer_tx = consumer_tx.clone();
            move || {
                let links = our_links.borrow();
                let monitors = node_is_monitor.borrow();
                let count = links
                    .values()
                    .filter(|other_node| monitors.get(other_node) != Some(&true))
                    .count() as u32;
                if count != last_count.get() {
                    last_count.set(count);
                    tracing::debug!("PipeWire consumer count: {count}");
                    if let Err(e) =
                        consumer_tx.try_send(crate::consumer::ConsumerEvent::Changed(count))
                    {
                        tracing::warn!("Consumer event dropped: {e}");
                    }
                }
            }
        };

        let listener = registry
            .add_listener_local()
            .global({
                let our_node_id = our_node_id.clone();
                let our_links = our_links.clone();
                let node_is_monitor = node_is_monitor.clone();
                let proxies = proxies.clone();
                let registry = registry.clone();
                let recount = recount.clone();
                move |global| {
                    match global.type_ {
                        ObjectType::Node => {
                            // Bind Stream/Input/Audio nodes to get their full info.
                            let is_audio_input =
                                global.props.as_ref().and_then(|p| p.get("media.class"))
                                    == Some("Stream/Input/Audio");
                            if !is_audio_input {
                                return;
                            }
                            let node_id = global.id;
                            let Ok(node) = registry.bind::<pipewire::node::Node, _>(global) else {
                                return;
                            };
                            let node_listener = node
                                .add_listener_local()
                                .info({
                                    let node_is_monitor = node_is_monitor.clone();
                                    let recount = recount.clone();
                                    move |info| {
                                        let is_monitor =
                                            info.props().and_then(|p| p.get("stream.monitor"))
                                                == Some("true");
                                        let mut map = node_is_monitor.borrow_mut();
                                        let was_monitor =
                                            map.get(&node_id).copied().unwrap_or(false);
                                        if is_monitor && !was_monitor {
                                            // Sticky: once classified as monitor, stays monitor.
                                            // stream.monitor flickers during PW reconfiguration
                                            // but a peak detect stream never becomes a real consumer.
                                            tracing::debug!(
                                                "PipeWire node (id={node_id}) classified as monitor"
                                            );
                                            map.insert(node_id, true);
                                            drop(map);
                                            recount();
                                        } else if let std::collections::hash_map::Entry::Vacant(e) =
                                            map.entry(node_id)
                                        {
                                            // First info: not a monitor stream.
                                            e.insert(false);
                                            drop(map);
                                            recount();
                                        }
                                    }
                                })
                                .register();
                            proxies
                                .borrow_mut()
                                .insert(node_id, Box::new((node, node_listener)));
                        }
                        ObjectType::Link => {
                            let our_id = our_node_id.get();
                            if our_id == u32::MAX {
                                return;
                            }
                            let Some(props) = &global.props else {
                                return;
                            };
                            let output_node = props
                                .get("link.output.node")
                                .and_then(|v| v.parse::<u32>().ok());
                            let input_node = props
                                .get("link.input.node")
                                .and_then(|v| v.parse::<u32>().ok());
                            if output_node != Some(our_id) && input_node != Some(our_id) {
                                return;
                            }
                            let other_node = if output_node == Some(our_id) {
                                input_node
                            } else {
                                output_node
                            };
                            let Some(other_id) = other_node else { return };

                            let link_id = global.id;
                            tracing::debug!(
                                "PipeWire link to our node (id={link_id}, other_node={other_id})"
                            );
                            our_links.borrow_mut().insert(link_id, other_id);
                            // Recount — if node info is already cached, this will
                            // immediately include or exclude the link. If not yet
                            // cached, the node's info callback will trigger recount.
                            recount();
                        }
                        _ => {}
                    }
                }
            })
            .global_remove({
                let our_links = our_links.clone();
                let node_is_monitor = node_is_monitor.clone();
                let proxies = proxies.clone();
                let recount = recount.clone();
                move |id| {
                    proxies.borrow_mut().remove(&id);
                    let had_link = our_links.borrow_mut().remove(&id).is_some();
                    let had_node = node_is_monitor.borrow_mut().remove(&id).is_some();
                    if had_link || had_node {
                        recount();
                    }
                }
            })
            .register();

        (Some(registry), Some(listener), Some(proxies))
    } else {
        (None, None, None)
    };

    // Block until quit.
    mainloop.run();

    // Follow cpal's exact drop pattern: explicitly drop listener and context
    // first, let stream, _receiver, and mainloop drop at function scope end.
    // Do NOT call pipewire::deinit() - it's process-global and we may create
    // another PW thread on reconnect.
    drop(_listener);
    drop(context);

    tracing::info!("PipeWire source stopped");
    Ok(())
}
