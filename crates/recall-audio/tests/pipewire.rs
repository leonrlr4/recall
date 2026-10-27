//! Tests against the running PipeWire daemon. They create a null sink and
//! change the default sink while they run, so they are skipped unless
//! `RECALL_PW_TESTS=1`, and must run one at a time:
//!
//! ```sh
//! RECALL_PW_TESTS=1 cargo test -p recall-audio --test pipewire -- --test-threads=1
//! ```

use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use recall_audio::capture::{Capture, Levels};
use recall_audio::detect::Detector;
use recall_audio::state::DetectEvent;
use recall_audio::writer::ChunkWriter;

const SINK_NAME: &str = "recall-test-sink";

fn enabled() -> bool {
    if std::env::var("RECALL_PW_TESTS").as_deref() == Ok("1") {
        true
    } else {
        eprintln!("skipped: set RECALL_PW_TESTS=1 to run PipeWire tests");
        false
    }
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().expect("running a PipeWire tool");
    assert!(out.status.success(), "{cmd:?} failed: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// The `pw-dump` object of the node named `name`.
fn node(name: &str) -> Option<serde_json::Value> {
    let dump: serde_json::Value = serde_json::from_str(&run(&mut Command::new("pw-dump"))).unwrap();
    dump.as_array()?
        .iter()
        .find(|obj| {
            obj.pointer("/info/props/node.name")
                .and_then(|n| n.as_str())
                == Some(name)
        })
        .cloned()
}

fn node_id(name: &str) -> Option<u64> {
    node(name)?.get("id")?.as_u64()
}

/// The id of the current default sink, from `wpctl inspect`, whose first line
/// is `id <N>, type ...`.
fn default_sink_id() -> u64 {
    let out = run(Command::new("wpctl").args(["inspect", "@DEFAULT_AUDIO_SINK@"]));
    out.trim_start()
        .strip_prefix("id ")
        .and_then(|rest| rest.split(',').next())
        .and_then(|id| id.parse().ok())
        .unwrap_or_else(|| panic!("unexpected wpctl inspect output: {out}"))
}

fn set_default(id: u64) {
    run(Command::new("wpctl").args(["set-default", &id.to_string()]));
}

/// A null sink owned by an interactive `pw-cli`. With `object.linger=false`
/// the sink lives as long as the `pw-cli` client, so dropping this (even
/// while unwinding from a failed assertion) removes it. The previous default
/// sink is restored first.
struct NullSink {
    id: u64,
    pw_cli: Child,
    previous_default: Option<u64>,
}

impl NullSink {
    fn create() -> NullSink {
        let mut pw_cli = Command::new("pw-cli")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawning pw-cli");
        // pw-cli keeps running after its stdin closes; the handle is kept
        // only so the child is not left with a broken pipe.
        writeln!(
            pw_cli.stdin.as_mut().unwrap(),
            "create-node adapter {{ factory.name=support.null-audio-sink \
             node.name={SINK_NAME} media.class=Audio/Sink object.linger=false \
             audio.position=[FL FR] }}"
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let id = loop {
            if let Some(id) = node_id(SINK_NAME) {
                break id;
            }
            assert!(Instant::now() < deadline, "null sink did not appear");
            sleep(Duration::from_millis(100));
        };
        NullSink {
            id,
            pw_cli,
            previous_default: None,
        }
    }

    fn make_default(&mut self) {
        if self.previous_default.is_none() {
            self.previous_default = Some(default_sink_id());
        }
        set_default(self.id);
    }
}

impl Drop for NullSink {
    fn drop(&mut self) {
        if let Some(previous) = self.previous_default {
            set_default(previous);
        }
        let _ = self.pw_cli.kill();
        let _ = self.pw_cli.wait();
    }
}

/// Writes a mono 16-bit 48 kHz WAV of a 440 Hz tone at half scale.
fn write_tone(path: &Path, seconds: u32) {
    let frames = 48_000 * seconds;
    let mut bytes = Vec::with_capacity(44 + frames as usize * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + frames * 2).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&48_000u32.to_le_bytes());
    bytes.extend_from_slice(&(48_000u32 * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(frames * 2).to_le_bytes());
    for i in 0..frames {
        let t = i as f32 / 48_000.0;
        let sample = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
        bytes.extend_from_slice(&((sample * f32::from(i16::MAX)) as i16).to_le_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}

/// All samples of one track, in chunk order.
fn read_track(cache: &Path, prefix: &str) -> Vec<f32> {
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

fn rms(samples: &[f32]) -> f32 {
    let sum: f64 = samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    (sum / samples.len().max(1) as f64).sqrt() as f32
}

/// Starts a capture into a fresh cache, plays a 2 s tone to the default sink,
/// stops, and returns (sys samples, mic samples, sys peak level, the sys
/// stream's `target.object` while it ran).
fn capture_tone(
    sink: &mut NullSink,
    default_before_start: bool,
) -> (Vec<f32>, Vec<f32>, f32, Option<String>) {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join(".cache");
    let tone = dir.path().join("tone.wav");
    write_tone(&tone, 2);

    if default_before_start {
        sink.make_default();
    }
    let writer = Arc::new(Mutex::new(ChunkWriter::new(&cache).unwrap()));
    let levels = Arc::new(Levels::default());
    let capture = Capture::start(writer.clone(), levels.clone()).unwrap();
    sleep(Duration::from_millis(500));
    if !default_before_start {
        sink.make_default();
        // Give the sys stream time to move to the new default.
        sleep(Duration::from_millis(500));
    }

    let sys_target = node("recall-sys").and_then(|n| {
        n.pointer("/info/props/target.object")?
            .as_str()
            .map(str::to_owned)
    });
    run(Command::new("pw-play").arg(&tone));
    sleep(Duration::from_millis(300));
    let sys_peak = f32::from_bits(levels.sys.load(std::sync::atomic::Ordering::Relaxed));
    assert!(capture.is_running());
    capture.stop().unwrap();
    writer.lock().unwrap().flush().unwrap();

    (
        read_track(&cache, "sys"),
        read_track(&cache, "mic"),
        sys_peak,
        sys_target,
    )
}

#[test]
fn capture_records_tone_played_to_null_sink() {
    if !enabled() {
        return;
    }
    let mut sink = NullSink::create();
    let (sys, mic, _, _) = capture_tone(&mut sink, true);
    let sys_rms = rms(&sys);
    eprintln!(
        "sys: {} samples, rms {sys_rms:.3}; mic: {} samples",
        sys.len(),
        mic.len()
    );
    assert!(sys_rms > 0.05, "sys rms {sys_rms} too low");
    assert!(!mic.is_empty(), "no mic chunk written");
}

#[test]
fn sys_stream_follows_default_sink_change() {
    if !enabled() {
        return;
    }
    let mut sink = NullSink::create();
    let (sys, _, sys_peak, sys_target) = capture_tone(&mut sink, false);
    let sys_rms = rms(&sys);
    eprintln!(
        "sys: {} samples, rms {sys_rms:.3}, peak {sys_peak:.3}, target {sys_target:?}",
        sys.len()
    );
    // WirePlumber also moves streams without a target to a new default, so
    // the tone alone would not show that the capture reconnected; only the
    // capture sets `target.object`.
    assert_eq!(sys_target.as_deref(), Some(SINK_NAME));
    assert!(sys_rms > 0.05, "sys rms {sys_rms} too low");
}

/// Waits up to `timeout` for an event matching `pred`, skipping others.
fn wait_for(
    rx: &mpsc::Receiver<DetectEvent>,
    timeout: Duration,
    pred: impl Fn(&DetectEvent) -> bool,
) -> Option<DetectEvent> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match rx.recv_timeout(left) {
            Ok(ev) if pred(&ev) => return Some(ev),
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

#[test]
fn detector_sees_pw_record() {
    if !enabled() {
        return;
    }
    let (tx, rx) = mpsc::channel();
    let detector = Detector::spawn(tx, "recall").unwrap();
    let mut record = Command::new("pw-record")
        .args(["--target", "0", "/dev/null"])
        .spawn()
        .unwrap();

    let opened = wait_for(
        &rx,
        Duration::from_secs(2),
        |ev| matches!(ev, DetectEvent::MicOpened(user) if user.app_name == "pw-record"),
    );
    record.kill().unwrap();
    record.wait().unwrap();
    let Some(DetectEvent::MicOpened(user)) = opened else {
        panic!("no MicOpened for pw-record within 2 s");
    };
    eprintln!("opened: {user:?}");
    // pw-record is a symlink to pw-cat, and PipeWire reports the binary
    // that actually runs.
    assert_eq!(user.binary, "pw-cat");

    let closed = wait_for(&rx, Duration::from_secs(2), |ev| {
        *ev == DetectEvent::MicClosed(user.node_id)
    });
    assert!(closed.is_some(), "no MicClosed for node {}", user.node_id);
    detector.stop();
}

#[test]
fn detector_ignores_recalls_own_streams() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::channel();
    let detector = Detector::spawn(tx, "recall").unwrap();
    let writer = Arc::new(Mutex::new(ChunkWriter::new(dir.path()).unwrap()));
    let capture = Capture::start(writer, Arc::new(Levels::default())).unwrap();

    let own = wait_for(
        &rx,
        Duration::from_secs(1),
        |ev| matches!(ev, DetectEvent::MicOpened(user) if user.app_name == "recall"),
    );
    capture.stop().unwrap();
    detector.stop();
    assert_eq!(own, None);
}
