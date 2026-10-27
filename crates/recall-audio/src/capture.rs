//! Captures the microphone and the system audio from PipeWire into a
//! [`ChunkWriter`].
//!
//! Two capture streams run on a thread with its own PipeWire main loop: the
//! mic stream connects to the default source, and the sys stream captures the
//! monitor of the default sink (`stream.capture.sink`). Both negotiate F32LE
//! at 48 kHz with the channel count left to the graph, and are downmixed to
//! mono by averaging the channels.
//!
//! The sys stream follows the default sink: the capture watches the `default`
//! metadata object, and when `default.audio.sink` changes it destroys the sys
//! stream and creates a new one with `target.object` set to the new sink's
//! name. Samples from the old stream that are still in flight are written
//! before the new stream's, and the switch gap is filled with silence like any
//! other gap.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use pipewire as pw;
use pw::metadata::{Metadata, MetadataListener};
use pw::properties::PropertiesBox;
use pw::spa;
use pw::stream::{StreamFlags, StreamListener, StreamRc, StreamState};
use pw::types::ObjectType;
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;

use crate::writer::{ChunkWriter, Track};

const SAMPLE_RATE: u32 = 48_000;
/// A gap longer than this (100 ms at 48 kHz) between the samples received
/// and the wall clock is filled with silence.
const GAP_SAMPLES: u64 = 4_800;
static ZEROS: [f32; GAP_SAMPLES as usize] = [0.0; GAP_SAMPLES as usize];
const APP_NAME: &str = "recall";
const DEFAULT_SINK_KEY: &str = "default.audio.sink";

/// The peak absolute sample of the latest buffer of each track, as
/// [`f32::to_bits`]. Read with `f32::from_bits(levels.mic.load(..))`.
#[derive(Debug, Default)]
pub struct Levels {
    pub mic: AtomicU32,
    pub sys: AtomicU32,
}

/// A running capture of both tracks. [`Capture::stop`] ends it.
pub struct Capture {
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
    writer: Arc<Mutex<ChunkWriter>>,
    error: Arc<Mutex<Option<anyhow::Error>>>,
}

impl Capture {
    pub fn start(writer: Arc<Mutex<ChunkWriter>>, levels: Arc<Levels>) -> Result<Capture> {
        let error = Arc::new(Mutex::new(None));
        let mic = Arc::new(TrackSink::new(
            Track::Mic,
            writer.clone(),
            levels.clone(),
            error.clone(),
        ));
        let sys = Arc::new(TrackSink::new(
            Track::Sys,
            writer.clone(),
            levels,
            error.clone(),
        ));
        let (quit, quit_rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("recall-capture".into())
            .spawn(move || run(mic, sys, quit_rx, ready_tx))
            .context("spawning the capture thread")?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Capture {
                quit,
                thread: Some(thread),
                writer,
                error,
            }),
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!("the capture thread exited during startup"))
            }
        }
    }

    /// Stops both streams and flushes the writer. Returns the first error
    /// writing captured samples hit, if any.
    pub fn stop(mut self) -> Result<()> {
        self.shutdown()?;
        lock(&self.writer).flush()?;
        match lock(&self.error).take() {
            Some(err) => Err(err.context("writing captured audio")),
            None => Ok(()),
        }
    }

    fn shutdown(&mut self) -> Result<()> {
        let _ = self.quit.send(());
        match self.thread.take() {
            Some(thread) => thread
                .join()
                .map_err(|_| anyhow!("the capture thread panicked")),
            None => Ok(()),
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Err(err) = self.shutdown() {
            tracing::error!(%err, "stopping the capture");
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where one track's samples go. Shared by the successive streams of the
/// track, so the silence fill spans a stream being replaced.
struct TrackSink {
    track: Track,
    writer: Arc<Mutex<ChunkWriter>>,
    levels: Arc<Levels>,
    error: Arc<Mutex<Option<anyhow::Error>>>,
    clock: Mutex<FillClock>,
}

#[derive(Default)]
struct FillClock {
    /// Wall-clock time of the first sample received.
    start: Option<Instant>,
    /// Samples written so far, silence included.
    written: u64,
}

impl TrackSink {
    fn new(
        track: Track,
        writer: Arc<Mutex<ChunkWriter>>,
        levels: Arc<Levels>,
        error: Arc<Mutex<Option<anyhow::Error>>>,
    ) -> Self {
        Self {
            track,
            writer,
            levels,
            error,
            clock: Mutex::new(FillClock::default()),
        }
    }

    /// Writes one buffer of mono samples that ended now, preceded by silence
    /// if the track has fallen more than 100 ms behind the wall clock (the
    /// source or sink suspended, or the stream was being replaced).
    fn push(&self, mono: &[f32], now: Instant) {
        let mut clock = lock(&self.clock);
        let gap = clock.gap_before(mono.len() as u64, now);
        let mut writer = lock(&self.writer);
        let mut result = Ok(());
        let mut silence = gap;
        while silence > 0 && result.is_ok() {
            let take = silence.min(GAP_SAMPLES);
            result = writer.push(self.track, &ZEROS[..take as usize]);
            silence -= take;
        }
        if result.is_ok() {
            result = writer.push(self.track, mono);
        }
        clock.written += gap + mono.len() as u64;
        drop(writer);
        drop(clock);

        let peak = mono.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        let level = match self.track {
            Track::Mic => &self.levels.mic,
            Track::Sys => &self.levels.sys,
        };
        level.store(peak.to_bits(), Ordering::Relaxed);

        if let Err(err) = result {
            let mut slot = lock(&self.error);
            if slot.is_none() {
                tracing::error!(track = ?self.track, %err, "writing captured audio");
                *slot = Some(err);
            }
        }
    }
}

impl FillClock {
    /// Samples of silence to write before `len` samples that ended at `now`.
    fn gap_before(&mut self, len: u64, now: Instant) -> u64 {
        let start = *self.start.get_or_insert_with(|| {
            // The first buffer's samples span the time before it arrived.
            now.checked_sub(samples_to_duration(len)).unwrap_or(now)
        });
        let expected = (now.duration_since(start).as_secs_f64() * f64::from(SAMPLE_RATE)) as u64;
        let missing = expected.saturating_sub(self.written + len);
        if missing > GAP_SAMPLES { missing } else { 0 }
    }
}

fn samples_to_duration(samples: u64) -> Duration {
    Duration::from_secs_f64(samples as f64 / f64::from(SAMPLE_RATE))
}

/// Averages interleaved F32LE frames of `channels` channels into `out`.
fn downmix(bytes: &[u8], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    let channels = channels.max(1);
    let scale = 1.0 / channels as f32;
    for frame in bytes.as_chunks::<4>().0.chunks_exact(channels) {
        let sum: f32 = frame.iter().map(|b| f32::from_le_bytes(*b)).sum();
        out.push(sum * scale);
    }
}

/// The EnumFormat param: F32LE at 48 kHz, any channel count.
fn format_param() -> Result<Vec<u8>> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|err| anyhow!("serializing the format: {err:?}"))?
    .0
    .into_inner();
    Ok(bytes)
}

type LiveStream = (StreamRc, StreamListener<()>);

/// Creates and connects a capture stream for `sink`'s track. `target` pins the
/// sys stream to a sink by name; without it the stream autoconnects to the
/// default.
fn connect_stream(
    core: &pw::core::CoreRc,
    sink: &Arc<TrackSink>,
    target: Option<&str>,
) -> Result<LiveStream> {
    let node_name = match sink.track {
        Track::Mic => "recall-mic",
        Track::Sys => "recall-sys",
    };
    let mut props = PropertiesBox::new();
    props.insert(*pw::keys::MEDIA_TYPE, "Audio");
    props.insert(*pw::keys::MEDIA_CATEGORY, "Capture");
    props.insert(*pw::keys::MEDIA_ROLE, "Communication");
    props.insert(*pw::keys::NODE_NAME, node_name);
    props.insert(*pw::keys::APP_NAME, APP_NAME);
    if sink.track == Track::Sys {
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }
    if let Some(target) = target {
        props.insert(*pw::keys::TARGET_OBJECT, target);
    }
    let stream = StreamRc::new(core.clone(), node_name, props)
        .with_context(|| format!("creating the {node_name} stream"))?;

    let channels = Arc::new(AtomicU32::new(0));
    let negotiated = channels.clone();
    let process_sink = sink.clone();
    let mut mono = Vec::new();
    let listener = stream
        .add_local_listener::<()>()
        .state_changed(move |_, _, _, state| {
            if let StreamState::Error(message) = state {
                tracing::error!(stream = node_name, message, "capture stream error");
            }
        })
        .param_changed(move |_, _, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((MediaType::Audio, MediaSubtype::Raw)) = format_utils::parse_format(param)
            else {
                return;
            };
            let mut info = spa::param::audio::AudioInfoRaw::new();
            if info.parse(param).is_err() {
                tracing::warn!(stream = node_name, "unparsable audio format");
                return;
            }
            if info.rate() != SAMPLE_RATE {
                tracing::warn!(stream = node_name, rate = info.rate(), "unexpected rate");
            }
            tracing::debug!(stream = node_name, channels = info.channels(), "negotiated");
            negotiated.store(info.channels(), Ordering::Release);
        })
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let channels = channels.load(Ordering::Acquire) as usize;
            if channels == 0 {
                return;
            }
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let offset = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            let Some(bytes) = data.data() else { return };
            let end = offset.saturating_add(size).min(bytes.len());
            downmix(&bytes[offset.min(end)..end], channels, &mut mono);
            if !mono.is_empty() {
                process_sink.push(&mono, Instant::now());
            }
        })
        .register()
        .with_context(|| format!("listening on the {node_name} stream"))?;

    let format = format_param()?;
    let mut params = [Pod::from_bytes(&format).context("building the format pod")?];
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .with_context(|| format!("connecting the {node_name} stream"))?;
    Ok((stream, listener))
}

/// The sink name in a `default.audio.sink` value, `{"name":"<sink>"}`.
fn sink_name(value: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(value).ok()?;
    json.get("name")?.as_str().map(str::to_owned)
}

/// Replaces the sys stream when the default sink changes.
struct SinkFollower {
    core: pw::core::CoreRc,
    sink: Arc<TrackSink>,
    stream: Option<LiveStream>,
    /// The default sink when the sys stream connected, once known.
    current: Option<String>,
}

impl SinkFollower {
    fn default_changed(&mut self, name: String) {
        let Some(current) = &self.current else {
            // The first value is the default the stream autoconnected to.
            self.current = Some(name);
            return;
        };
        if *current == name {
            return;
        }
        tracing::info!(sink = %name, "default sink changed; reconnecting system audio");
        if let Some((stream, listener)) = self.stream.take() {
            drop(listener);
            if let Err(err) = stream.disconnect() {
                tracing::warn!(%err, "disconnecting the system audio stream");
            }
        }
        match connect_stream(&self.core, &self.sink, Some(&name)) {
            Ok(stream) => self.stream = Some(stream),
            Err(err) => tracing::error!(%err, "reconnecting system audio"),
        }
        self.current = Some(name);
    }
}

fn run(
    mic: Arc<TrackSink>,
    sys: Arc<TrackSink>,
    quit_rx: pw::channel::Receiver<()>,
    ready: mpsc::Sender<Result<()>>,
) {
    pw::init();
    let setup = || -> Result<_> {
        let main_loop = pw::main_loop::MainLoopRc::new(None).context("creating a main loop")?;
        let context =
            pw::context::ContextRc::new(&main_loop, None).context("creating a context")?;
        let core = context.connect_rc(None).context("connecting to PipeWire")?;
        let registry = core.get_registry_rc().context("getting the registry")?;
        let mic_stream = connect_stream(&core, &mic, None)?;
        let sys_stream = connect_stream(&core, &sys, None)?;
        Ok((main_loop, context, core, registry, mic_stream, sys_stream))
    };
    let (main_loop, _context, core, registry, _mic_stream, sys_stream) = match setup() {
        Ok(objects) => objects,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };

    let main_loop_weak = main_loop.downgrade();
    let _quit = quit_rx.attach(main_loop.loop_(), move |()| {
        if let Some(main_loop) = main_loop_weak.upgrade() {
            main_loop.quit();
        }
    });
    let main_loop_weak = main_loop.downgrade();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _seq, res, message| {
            tracing::error!(id, res, message, "PipeWire error in the capture");
            if id == pw::core::PW_ID_CORE
                && let Some(main_loop) = main_loop_weak.upgrade()
            {
                main_loop.quit();
            }
        })
        .register();

    let follower = Rc::new(RefCell::new(SinkFollower {
        core: core.clone(),
        sink: sys,
        stream: Some(sys_stream),
        current: None,
    }));
    let metadata: Rc<RefCell<Option<(Metadata, MetadataListener)>>> = Rc::default();
    let registry_weak = registry.downgrade();
    let metadata_slot = metadata.clone();
    let follower_weak = Rc::downgrade(&follower);
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != ObjectType::Metadata
                || global.props.and_then(|p| p.get("metadata.name")) != Some("default")
            {
                return;
            }
            let Some(registry) = registry_weak.upgrade() else {
                return;
            };
            let proxy: Metadata = match registry.bind(global) {
                Ok(proxy) => proxy,
                Err(err) => {
                    tracing::warn!(%err, "binding the default metadata");
                    return;
                }
            };
            let follower = follower_weak.clone();
            let listener = proxy
                .add_listener_local()
                .property(move |_subject, key, _type, value| {
                    if key == Some(DEFAULT_SINK_KEY)
                        && let Some(name) = value.and_then(sink_name)
                        && let Some(follower) = follower.upgrade()
                    {
                        follower.borrow_mut().default_changed(name);
                    }
                    0
                })
                .register();
            *metadata_slot.borrow_mut() = Some((proxy, listener));
        })
        .register();

    let _ = ready.send(Ok(()));
    main_loop.run();

    // Tear the sys stream down before the core it belongs to.
    metadata.borrow_mut().take();
    follower.borrow_mut().stream.take();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_averages_channels() {
        let bytes: Vec<u8> = [0.5f32, -0.5, 1.0, 0.0, 0.25]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut out = Vec::new();
        downmix(&bytes, 2, &mut out);
        // The trailing half frame is dropped.
        assert_eq!(out, vec![0.0, 0.5]);
        downmix(&bytes, 1, &mut out);
        assert_eq!(out, vec![0.5, -0.5, 1.0, 0.0, 0.25]);
    }

    #[test]
    fn fill_clock_pads_gaps_over_100ms_only() {
        let mut clock = FillClock::default();
        let t0 = Instant::now();
        let quantum = 1_024u64;
        let after = |samples: u64| t0 + samples_to_duration(samples);

        assert_eq!(clock.gap_before(quantum, after(quantum)), 0);
        clock.written += quantum;
        // On time.
        assert_eq!(clock.gap_before(quantum, after(2 * quantum)), 0);
        clock.written += quantum;
        // 50 ms late: jitter, not filled.
        assert_eq!(clock.gap_before(quantum, after(3 * quantum + 2_400)), 0);
        clock.written += quantum;
        // One second with no buffers.
        let gap = clock.gap_before(quantum, after(4 * quantum + 48_000));
        assert!((47_990..=48_000).contains(&gap), "gap {gap}");
    }

    #[test]
    fn sink_name_parses_metadata_value() {
        assert_eq!(
            sink_name(r#"{ "name": "alsa_output.pci" }"#).as_deref(),
            Some("alsa_output.pci")
        );
        assert_eq!(sink_name("not json"), None);
    }
}
