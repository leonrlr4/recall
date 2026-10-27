//! Captures the microphone and the system audio from PipeWire into a
//! [`ChunkWriter`].
//!
//! Two capture streams run on a thread with its own PipeWire main loop: the
//! mic stream connects to the default source, and the sys stream captures the
//! monitor of the default sink (`stream.capture.sink`). Both negotiate F32LE
//! at 48 kHz with the channel count left to the graph, and are downmixed to
//! mono by averaging the channels.
//!
//! The streams' `process` callbacks run on PipeWire's realtime thread, so
//! they only downmix into a fixed-size ring per track (5 s) and wake a writer
//! thread; they never block, allocate, log or touch files. The writer thread
//! drains both rings into the [`ChunkWriter`] and pads silence:
//!
//! - before a track's first buffer, exactly the time since the capture
//!   started, so both tracks share one origin;
//! - for samples the realtime side dropped because a ring was full;
//! - for a gap over 100 ms between consecutive buffers (a suspended source or
//!   sink, a stream being replaced). Gaps are measured buffer to buffer, so
//!   drift between the device clock and the wall clock never accumulates.
//!
//! The sys stream follows the default sink: the capture watches the `default`
//! metadata object, and when `default.audio.sink` changes it destroys the sys
//! stream and creates a new one with `target.object` set to the new sink's
//! name. Buffers still queued in the old stream are discarded (up to one
//! quantum), and the switch gap is padded with silence like any other gap.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError, mpsc};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use pipewire as pw;
use pw::metadata::{Metadata, MetadataListener};
use pw::properties::PropertiesBox;
use pw::spa;
use pw::stream::{StreamFlags, StreamListener, StreamRc, StreamState};
use pw::types::ObjectType;
use rtrb::{Consumer, Producer, RingBuffer};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;

use crate::writer::{ChunkWriter, Track};

const SAMPLE_RATE: u32 = 48_000;
/// A gap longer than this (100 ms at 48 kHz) between consecutive buffers is
/// padded with silence.
const GAP_SAMPLES: u64 = 4_800;
static ZEROS: [f32; GAP_SAMPLES as usize] = [0.0; GAP_SAMPLES as usize];
/// Samples each track's ring holds: 5 s of mono audio.
const RING_SAMPLES: usize = 5 * SAMPLE_RATE as usize;
/// Buffer headers each ring holds; 5 s of quanta down to 32 samples.
const RING_BUFFERS: usize = 7_500;
const APP_NAME: &str = "recall";
const DEFAULT_SINK_KEY: &str = "default.audio.sink";

/// The peak absolute sample of the latest buffer of each track, as
/// [`f32::to_bits`]. Read with `f32::from_bits(levels.mic.load(..))`.
#[derive(Debug, Default)]
pub struct Levels {
    pub mic: AtomicU32,
    pub sys: AtomicU32,
}

/// The first error that ended or damaged the capture.
#[derive(Default)]
struct Failure(Mutex<Option<anyhow::Error>>);

impl Failure {
    /// Keeps `err` unless an earlier error is already kept.
    fn record(&self, err: anyhow::Error) {
        let mut slot = lock(&self.0);
        if slot.is_none() {
            tracing::error!(err = format!("{err:#}"), "capture failed");
            *slot = Some(err);
        }
    }

    fn take(&self) -> Option<anyhow::Error> {
        lock(&self.0).take()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running capture of both tracks. [`Capture::stop`] ends it.
pub struct Capture {
    quit: pw::channel::Sender<()>,
    pw_thread: Option<JoinHandle<()>>,
    writer_thread: Option<JoinHandle<Result<()>>>,
    stop_writer: Arc<AtomicBool>,
    failure: Arc<Failure>,
}

impl Capture {
    pub fn start(writer: Arc<Mutex<ChunkWriter>>, levels: Arc<Levels>) -> Result<Capture> {
        let origin = Instant::now();
        let failure = Arc::new(Failure::default());
        let stop_writer = Arc::new(AtomicBool::new(false));
        let ([mic_ring, sys_ring], writer_thread) =
            spawn_writer(writer, origin, stop_writer.clone(), failure.clone())?;
        let wake = writer_thread.thread().clone();
        let mic = Arc::new(TrackInput::new(
            Track::Mic,
            mic_ring,
            levels.clone(),
            wake.clone(),
        ));
        let sys = Arc::new(TrackInput::new(Track::Sys, sys_ring, levels, wake));

        let (quit, quit_rx) = pw::channel::channel();
        let mut capture = Capture {
            quit,
            pw_thread: None,
            writer_thread: Some(writer_thread),
            stop_writer,
            failure: failure.clone(),
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        let pw_thread = std::thread::Builder::new()
            .name("recall-capture".into())
            .spawn(move || run(mic, sys, quit_rx, ready_tx, failure))
            .context("spawning the capture thread")?;
        capture.pw_thread = Some(pw_thread);
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(capture),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(anyhow!("the capture thread exited during startup")),
        }
        // On error, dropping `capture` joins both threads.
    }

    /// False once the PipeWire connection or the writer thread has ended on
    /// its own, as the writer does on its first failed write (a full disk);
    /// [`Capture::stop`] then returns the reason.
    pub fn is_running(&self) -> bool {
        let alive = |finished: Option<bool>| finished == Some(false);
        alive(self.pw_thread.as_ref().map(JoinHandle::is_finished))
            && alive(self.writer_thread.as_ref().map(JoinHandle::is_finished))
    }

    /// Stops both streams, writes out everything captured and flushes the
    /// writer. Returns the first error the capture hit, if any.
    pub fn stop(mut self) -> Result<()> {
        let pw = self.stop_pw();
        let written = self.stop_writer();
        pw?;
        if let Some(err) = self.failure.take() {
            return Err(err);
        }
        written
    }

    fn stop_pw(&mut self) -> Result<()> {
        let _ = self.quit.send(());
        match self.pw_thread.take() {
            Some(thread) => thread
                .join()
                .map_err(|_| anyhow!("the capture thread panicked")),
            None => Ok(()),
        }
    }

    /// Runs after [`Capture::stop_pw`], so no samples arrive any more; the
    /// writer drains what is queued, flushes and exits.
    fn stop_writer(&mut self) -> Result<()> {
        let Some(thread) = self.writer_thread.take() else {
            return Ok(());
        };
        self.stop_writer.store(true, Ordering::Release);
        thread.thread().unpark();
        thread
            .join()
            .map_err(|_| anyhow!("the capture writer thread panicked"))?
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let pw = self.stop_pw();
        let written = self.stop_writer();
        for result in [pw, written] {
            if let Err(err) = result {
                tracing::error!(err = format!("{err:#}"), "stopping the capture");
            }
        }
    }
}

/// One buffer's header in a track ring. Its samples precede it in the
/// sample ring.
#[derive(Clone, Copy, Debug)]
struct BufferMeta {
    arrival: Instant,
    len: usize,
    /// Samples dropped (ring full) since the previous header.
    dropped: u64,
}

struct RingProducers {
    samples: Producer<f32>,
    metas: Producer<BufferMeta>,
}

/// The realtime side of a track: what a stream's `process` callback writes
/// to. Shared by the successive streams of the track (one at a time).
struct TrackInput {
    track: Track,
    /// Locked only with `try_lock`, and only by the stream feeding the track.
    ring: Mutex<RingProducers>,
    dropped: AtomicU64,
    levels: Arc<Levels>,
    wake: Thread,
}

impl TrackInput {
    fn new(track: Track, ring: RingProducers, levels: Arc<Levels>, wake: Thread) -> Self {
        Self {
            track,
            ring: Mutex::new(ring),
            dropped: AtomicU64::new(0),
            levels,
            wake,
        }
    }

    /// Downmixes interleaved F32LE frames of `channels` channels that arrived
    /// at `arrival` into the ring, and wakes the writer. Realtime-safe: never
    /// blocks, allocates or logs. When the ring is full the buffer is counted
    /// as dropped, and the writer pads it with silence.
    fn push(&self, bytes: &[u8], channels: usize, arrival: Instant) {
        let channels = channels.max(1);
        let frames = bytes.as_chunks::<4>().0.chunks_exact(channels);
        let len = frames.len();
        if len == 0 {
            return;
        }
        let mut ring = match self.ring.try_lock() {
            Ok(ring) => ring,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                self.dropped.fetch_add(len as u64, Ordering::Relaxed);
                return;
            }
        };
        if ring.metas.slots() == 0 || ring.samples.slots() < len {
            self.dropped.fetch_add(len as u64, Ordering::Relaxed);
            drop(ring);
            self.wake.unpark();
            return;
        }
        let scale = 1.0 / channels as f32;
        let mut peak = 0.0f32;
        let chunk = ring
            .samples
            .write_chunk_uninit(len)
            .expect("free slots checked above");
        chunk.fill_from_iter(frames.map(|frame| {
            let sum: f32 = frame.iter().map(|b| f32::from_le_bytes(*b)).sum();
            let sample = sum * scale;
            peak = peak.max(sample.abs());
            sample
        }));
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        let _ = ring.metas.push(BufferMeta {
            arrival,
            len,
            dropped,
        });
        drop(ring);
        let level = match self.track {
            Track::Mic => &self.levels.mic,
            Track::Sys => &self.levels.sys,
        };
        level.store(peak.to_bits(), Ordering::Relaxed);
        self.wake.unpark();
    }
}

/// The writer-thread side of a track.
struct TrackOutput {
    track: Track,
    samples: Consumer<f32>,
    metas: Consumer<BufferMeta>,
    clock: FillClock,
}

fn track_ring(track: Track, origin: Instant) -> (RingProducers, TrackOutput) {
    let (samples_tx, samples_rx) = RingBuffer::new(RING_SAMPLES);
    let (metas_tx, metas_rx) = RingBuffer::new(RING_BUFFERS);
    (
        RingProducers {
            samples: samples_tx,
            metas: metas_tx,
        },
        TrackOutput {
            track,
            samples: samples_rx,
            metas: metas_rx,
            clock: FillClock::new(origin),
        },
    )
}

impl TrackOutput {
    /// Moves every queued buffer, and the silence before it, into `writer`.
    /// Keeps going after a write error so the ring keeps emptying; returns
    /// the first error.
    fn drain(&mut self, writer: &mut ChunkWriter) -> Result<()> {
        let mut first_error = Ok(());
        while let Ok(meta) = self.metas.pop() {
            let silence = self
                .clock
                .silence_before(meta.len as u64, meta.dropped, meta.arrival);
            let chunk = self
                .samples
                .read_chunk(meta.len)
                .expect("a buffer's samples are queued before its header");
            let (head, tail) = chunk.as_slices();
            let result = write_silence(writer, self.track, silence)
                .and_then(|()| writer.push(self.track, head))
                .and_then(|()| writer.push(self.track, tail));
            chunk.commit_all();
            if first_error.is_ok() {
                first_error = result;
            }
        }
        first_error
    }
}

fn write_silence(writer: &mut ChunkWriter, track: Track, mut samples: u64) -> Result<()> {
    while samples > 0 {
        let take = samples.min(GAP_SAMPLES);
        writer.push(track, &ZEROS[..take as usize])?;
        samples -= take;
    }
    Ok(())
}

/// Starts the writer thread on both tracks' rings and returns the rings'
/// realtime ends, mic first.
fn spawn_writer(
    writer: Arc<Mutex<ChunkWriter>>,
    origin: Instant,
    stop: Arc<AtomicBool>,
    failure: Arc<Failure>,
) -> Result<([RingProducers; 2], JoinHandle<Result<()>>)> {
    let (mic_ring, mic_out) = track_ring(Track::Mic, origin);
    let (sys_ring, sys_out) = track_ring(Track::Sys, origin);
    let thread = std::thread::Builder::new()
        .name("recall-capture-writer".into())
        .spawn(move || write_loop([mic_out, sys_out], &writer, &stop, &failure))
        .context("spawning the capture writer thread")?;
    Ok(([mic_ring, sys_ring], thread))
}

/// The writer thread: drains both rings whenever woken, until asked to stop
/// or a write fails. Blocks in `park` while no audio arrives.
///
/// A failed write ends the thread (after recording the error in `failure`),
/// which ends the capture: a full disk will not take the rest of the
/// meeting, and stopping now keeps what reached the disk usable.
fn write_loop(
    mut tracks: [TrackOutput; 2],
    writer: &Mutex<ChunkWriter>,
    stop: &AtomicBool,
    failure: &Failure,
) -> Result<()> {
    loop {
        // Read the flag before draining: a stop requested after this read
        // unparks us, so the samples queued before it are drained next round.
        let stopping = stop.load(Ordering::Acquire);
        let mut failed = false;
        {
            let mut writer = lock(writer);
            for track in &mut tracks {
                if let Err(err) = track.drain(&mut writer) {
                    failure.record(err.context("writing captured audio"));
                    failed = true;
                }
            }
        }
        if stopping || failed {
            break;
        }
        std::thread::park();
    }
    lock(writer).flush().context("flushing captured audio")
}

/// Decides how much silence goes before each buffer of a track.
struct FillClock {
    /// When the capture started; shared by both tracks.
    origin: Instant,
    last_arrival: Option<Instant>,
}

impl FillClock {
    fn new(origin: Instant) -> Self {
        Self {
            origin,
            last_arrival: None,
        }
    }

    /// Samples of silence to write before a buffer of `len` samples that
    /// arrived at `arrival`, with `dropped` samples lost just before it.
    fn silence_before(&mut self, len: u64, dropped: u64, arrival: Instant) -> u64 {
        let silence = match self.last_arrival {
            // The first buffer's samples end at its arrival; everything from
            // the origin to their start is silence, gap threshold or not.
            None => {
                let start = arrival
                    .checked_sub(samples_to_duration(len))
                    .unwrap_or(arrival);
                duration_to_samples(start.saturating_duration_since(self.origin))
            }
            Some(last) => {
                let between = arrival.saturating_duration_since(last);
                let gap =
                    duration_to_samples(between.saturating_sub(samples_to_duration(len + dropped)));
                dropped + if gap > GAP_SAMPLES { gap } else { 0 }
            }
        };
        self.last_arrival = Some(arrival);
        silence
    }
}

fn samples_to_duration(samples: u64) -> Duration {
    let nanos = u128::from(samples) * 1_000_000_000 / u128::from(SAMPLE_RATE);
    Duration::from_nanos(nanos as u64)
}

/// Rounds to the nearest sample.
fn duration_to_samples(duration: Duration) -> u64 {
    ((duration.as_nanos() * u128::from(SAMPLE_RATE) + 500_000_000) / 1_000_000_000) as u64
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

/// Tears a stream down in the only safe order. `disconnect` removes the node
/// from the realtime loop under that loop's lock, so no `process` call is
/// running or can start afterwards; only then may the listener (which owns
/// the callbacks) go. The listener must also go before the stream: destroying
/// the stream frees the hook list the listener's drop unlinks itself from.
fn close_stream((stream, listener): LiveStream) {
    if let Err(err) = stream.disconnect() {
        tracing::warn!(%err, stream = stream.name(), "disconnecting a capture stream");
    }
    drop(listener);
    drop(stream);
}

/// Creates and connects a capture stream for `input`'s track. `target` pins
/// the sys stream to a sink by name; without it the stream autoconnects to
/// the default.
fn connect_stream(
    core: &pw::core::CoreRc,
    input: &Arc<TrackInput>,
    target: Option<&str>,
) -> Result<LiveStream> {
    let node_name = match input.track {
        Track::Mic => "recall-mic",
        Track::Sys => "recall-sys",
    };
    let mut props = PropertiesBox::new();
    props.insert(*pw::keys::MEDIA_TYPE, "Audio");
    props.insert(*pw::keys::MEDIA_CATEGORY, "Capture");
    props.insert(*pw::keys::MEDIA_ROLE, "Communication");
    props.insert(*pw::keys::NODE_NAME, node_name);
    props.insert(*pw::keys::APP_NAME, APP_NAME);
    if input.track == Track::Sys {
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }
    if let Some(target) = target {
        props.insert(*pw::keys::TARGET_OBJECT, target);
    }
    let stream = StreamRc::new(core.clone(), node_name, props)
        .with_context(|| format!("creating the {node_name} stream"))?;

    let channels = Arc::new(AtomicU32::new(0));
    let negotiated = channels.clone();
    let input = input.clone();
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
        // Runs on the realtime thread: see `TrackInput::push`.
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
            input.push(&bytes[offset.min(end)..end], channels, Instant::now());
        })
        .register()
        .with_context(|| format!("listening on the {node_name} stream"))?;

    let format = format_param()?;
    let mut params = [Pod::from_bytes(&format).context("building the format pod")?];
    let live = (stream, listener);
    if let Err(err) = live.0.connect(
        spa::utils::Direction::Input,
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    ) {
        close_stream(live);
        return Err(err).with_context(|| format!("connecting the {node_name} stream"));
    }
    Ok(live)
}

/// The sink name in a `default.audio.sink` value, `{"name":"<sink>"}`.
fn sink_name(value: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(value).ok()?;
    json.get("name")?.as_str().map(str::to_owned)
}

/// Replaces the sys stream when the default sink changes.
struct SinkFollower {
    core: pw::core::CoreRc,
    input: Arc<TrackInput>,
    stream: Option<LiveStream>,
    /// The default sink when the sys stream connected, once known.
    current: Option<String>,
    main_loop: pw::main_loop::MainLoopWeak,
    failure: Arc<Failure>,
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
        if let Some(old) = self.stream.take() {
            close_stream(old);
        }
        match connect_stream(&self.core, &self.input, Some(&name)) {
            Ok(stream) => self.stream = Some(stream),
            Err(err) => {
                // Without system audio the recording is broken; end the
                // capture so the owner sees it and can restart.
                self.failure
                    .record(err.context(format!("following the default sink to {name}")));
                if let Some(main_loop) = self.main_loop.upgrade() {
                    main_loop.quit();
                }
            }
        }
        self.current = Some(name);
    }

    fn close(&mut self) {
        if let Some(stream) = self.stream.take() {
            close_stream(stream);
        }
    }
}

fn run(
    mic: Arc<TrackInput>,
    sys: Arc<TrackInput>,
    quit_rx: pw::channel::Receiver<()>,
    ready: mpsc::Sender<Result<()>>,
    failure: Arc<Failure>,
) {
    pw::init();
    let setup = || -> Result<_> {
        let main_loop = pw::main_loop::MainLoopRc::new(None).context("creating a main loop")?;
        let context =
            pw::context::ContextRc::new(&main_loop, None).context("creating a context")?;
        let core = context.connect_rc(None).context("connecting to PipeWire")?;
        let registry = core.get_registry_rc().context("getting the registry")?;
        let mic_stream = connect_stream(&core, &mic, None)?;
        let sys_stream = match connect_stream(&core, &sys, None) {
            Ok(stream) => stream,
            Err(err) => {
                close_stream(mic_stream);
                return Err(err);
            }
        };
        Ok((main_loop, context, core, registry, mic_stream, sys_stream))
    };
    let (main_loop, _context, core, registry, mic_stream, sys_stream) = match setup() {
        Ok(objects) => objects,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };

    let quit_requested = Rc::new(Cell::new(false));
    let main_loop_weak = main_loop.downgrade();
    let requested = quit_requested.clone();
    let _quit = quit_rx.attach(main_loop.loop_(), move |()| {
        requested.set(true);
        if let Some(main_loop) = main_loop_weak.upgrade() {
            main_loop.quit();
        }
    });
    let main_loop_weak = main_loop.downgrade();
    let core_failure = failure.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _seq, res, message| {
            tracing::error!(id, res, message, "PipeWire error in the capture");
            if id == pw::core::PW_ID_CORE {
                core_failure.record(anyhow!("PipeWire connection failed ({res}): {message}"));
                if let Some(main_loop) = main_loop_weak.upgrade() {
                    main_loop.quit();
                }
            }
        })
        .register();

    let follower = Rc::new(RefCell::new(SinkFollower {
        core: core.clone(),
        input: sys,
        stream: Some(sys_stream),
        current: None,
        main_loop: main_loop.downgrade(),
        failure: failure.clone(),
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
    if !quit_requested.get() {
        failure.record(anyhow!("the PipeWire main loop ended unexpectedly"));
    }

    // Tear the streams down before the core they belong to.
    metadata.borrow_mut().take();
    follower.borrow_mut().close();
    close_stream(mic_stream);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::cache_duration;

    fn as_bytes(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn read_track(cache: &std::path::Path, prefix: &str) -> Vec<f32> {
        let mut paths: Vec<_> = std::fs::read_dir(cache)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with(&format!("{prefix}-"))
            })
            .collect();
        paths.sort();
        paths
            .iter()
            .flat_map(|p| {
                std::fs::read(p)
                    .unwrap()
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// A track's realtime input and writer output with no PipeWire and no
    /// writer thread: the test plays both sides.
    fn fake_track(track: Track, origin: Instant) -> (TrackInput, TrackOutput) {
        let (ring, output) = track_ring(track, origin);
        let input = TrackInput::new(
            track,
            ring,
            Arc::new(Levels::default()),
            std::thread::current(),
        );
        (input, output)
    }

    #[test]
    fn push_downmixes_channels_and_records_peak() {
        let origin = Instant::now();
        let (input, mut output) = fake_track(Track::Mic, origin);
        // Two stereo frames and a trailing half frame, which is dropped.
        let bytes = as_bytes(&[0.5, -0.5, 1.0, 0.0, 0.25]);
        input.push(&bytes, 2, origin + samples_to_duration(2));
        assert_eq!(
            f32::from_bits(input.levels.mic.load(Ordering::Relaxed)),
            0.5
        );

        let dir = tempfile::tempdir().unwrap();
        let mut writer = ChunkWriter::new(dir.path()).unwrap();
        output.drain(&mut writer).unwrap();
        writer.flush().unwrap();
        assert_eq!(read_track(dir.path(), "mic"), vec![0.0, 0.5]);
    }

    #[test]
    fn tracks_starting_500ms_apart_share_one_origin() {
        let origin = Instant::now();
        let (mic_in, mut mic_out) = fake_track(Track::Mic, origin);
        let (sys_in, mut sys_out) = fake_track(Track::Sys, origin);
        // 20 ms quanta, delivered back to back until 2 s. The mic's first
        // buffer starts at the origin, the sys track's 500 ms later.
        let quantum = 960u64;
        let end = 96_000u64;
        let buffer = as_bytes(&vec![0.25; quantum as usize]);
        let deliver = |input: &TrackInput, start: u64| {
            let mut t = start;
            while t < end {
                t += quantum;
                input.push(&buffer, 1, origin + samples_to_duration(t));
            }
        };
        deliver(&mic_in, 0);
        deliver(&sys_in, 24_000);

        let dir = tempfile::tempdir().unwrap();
        let mut writer = ChunkWriter::new(dir.path()).unwrap();
        mic_out.drain(&mut writer).unwrap();
        sys_out.drain(&mut writer).unwrap();
        writer.flush().unwrap();

        let mic = read_track(dir.path(), "mic");
        let sys = read_track(dir.path(), "sys");
        assert_eq!(mic.len(), end as usize);
        assert_eq!(sys.len(), mic.len());
        assert_eq!(mic[0], 0.25);
        assert!(sys[..24_000].iter().all(|s| *s == 0.0));
        assert!(sys[24_000..].iter().all(|s| *s == 0.25));
        assert_eq!(
            cache_duration(dir.path()).unwrap(),
            samples_to_duration(end)
        );
    }

    #[test]
    fn full_ring_drops_are_padded() {
        let origin = Instant::now();
        let (input, mut output) = fake_track(Track::Sys, origin);
        let quantum = 48_000usize;
        let buffer = as_bytes(&vec![0.5; quantum]);
        // Seven 1 s buffers into a 5 s ring with no draining: the last two
        // are dropped and padded once the next buffer fits.
        for i in 1..=7 {
            input.push(
                &buffer,
                1,
                origin + samples_to_duration((i * quantum) as u64),
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let mut writer = ChunkWriter::new(dir.path()).unwrap();
        output.drain(&mut writer).unwrap();
        input.push(
            &buffer,
            1,
            origin + samples_to_duration((8 * quantum) as u64),
        );
        output.drain(&mut writer).unwrap();
        writer.flush().unwrap();

        let sys = read_track(dir.path(), "sys");
        assert_eq!(sys.len(), 8 * quantum);
        assert!(sys[..5 * quantum].iter().all(|s| *s == 0.5));
        assert!(sys[5 * quantum..7 * quantum].iter().all(|s| *s == 0.0));
        assert!(sys[7 * quantum..].iter().all(|s| *s == 0.5));
    }

    #[test]
    fn fill_clock_pads_gaps_over_100ms_only() {
        let t0 = Instant::now();
        let mut clock = FillClock::new(t0);
        let quantum = 1_024u64;
        let at = |samples: u64| t0 + samples_to_duration(samples);

        // The first buffer starts 300 samples after the origin.
        assert_eq!(clock.silence_before(quantum, 0, at(300 + quantum)), 300);
        // On time.
        assert_eq!(clock.silence_before(quantum, 0, at(300 + 2 * quantum)), 0);
        // 50 ms late: jitter, not padded.
        let late = 300 + 3 * quantum + 2_400;
        assert_eq!(clock.silence_before(quantum, 0, at(late)), 0);
        // One second with no buffers.
        let after_gap = late + quantum + 48_000;
        assert_eq!(clock.silence_before(quantum, 0, at(after_gap)), 48_000);
        // Dropped samples are always padded, and do not count as a gap.
        assert_eq!(
            clock.silence_before(quantum, 2_048, at(after_gap + 3 * quantum)),
            2_048
        );
    }

    #[test]
    fn fill_clock_ignores_device_clock_drift() {
        let t0 = Instant::now();
        let quantum = 1_024u64;
        let period = samples_to_duration(quantum);
        for ppm in [-50.0, 50.0] {
            let mut clock = FillClock::new(t0);
            // An hour of buffers from a device running 50 ppm slow or fast
            // against the wall clock.
            let spacing = period.mul_f64(1.0 + ppm / 1e6);
            let mut arrival = t0 + period;
            let mut silence = 0;
            for _ in 0..(3_600 * u64::from(SAMPLE_RATE) / quantum) {
                silence += clock.silence_before(quantum, 0, arrival);
                arrival += spacing;
            }
            assert_eq!(silence, 0, "{ppm} ppm");
        }
    }

    #[test]
    fn write_error_ends_the_capture() {
        use std::sync::atomic::AtomicU64;

        let dir = tempfile::tempdir().unwrap();
        let writer = ChunkWriter::new(dir.path())
            .unwrap()
            .with_free_space(Arc::new(AtomicU64::new(0)));
        let origin = Instant::now();
        let failure = Arc::new(Failure::default());
        let stop = Arc::new(AtomicBool::new(false));
        let ([mic_ring, _sys_ring], writer_thread) = spawn_writer(
            Arc::new(Mutex::new(writer)),
            origin,
            stop.clone(),
            failure.clone(),
        )
        .unwrap();
        let mic = TrackInput::new(
            Track::Mic,
            mic_ring,
            Arc::new(Levels::default()),
            writer_thread.thread().clone(),
        );
        // A stand-in for the PipeWire thread that runs until the test ends.
        let (quit, _quit_rx) = pw::channel::channel();
        let (keep_tx, keep_rx) = mpsc::channel::<()>();
        let capture = Capture {
            quit,
            pw_thread: Some(std::thread::spawn(move || {
                let _ = keep_rx.recv_timeout(Duration::from_secs(10));
            })),
            writer_thread: Some(writer_thread),
            stop_writer: stop,
            failure,
        };
        assert!(capture.is_running());

        // One second: more than the chunk's write buffer, so it reaches
        // the (full) disk at once.
        mic.push(
            &as_bytes(&vec![0.5; 48_000]),
            1,
            origin + samples_to_duration(48_000),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while capture.is_running() {
            assert!(Instant::now() < deadline, "the capture kept running");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(keep_tx);
        let err = capture.stop().unwrap_err();
        assert!(
            format!("{err:#}").contains("writing captured audio"),
            "{err:#}"
        );
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
