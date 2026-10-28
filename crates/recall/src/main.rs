use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitCode};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::Local;
use clap::{Parser, Subcommand};
use recall::control::{self, Command, Reply};
use recall::selftest;
use recall::worker::step_name;
use recall_llm::Llm;
use recall_pipeline::models::{self, ModelId};
use recall_pipeline::runner::{Engines, run_from};
use recall_server::SelfTestResult;
use recall_store::{
    Config, Index, ListFilter, Meeting, MeetingDir, MeetingId, Side, Speakers, Step, StepState,
    markdown,
};
use serde::Serialize;
use tokio::sync::mpsc;

/// Records meetings and turns them into searchable notes.
#[derive(Parser)]
#[command(name = "recall", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the recorder daemon.
    Daemon,
    /// Start recording now.
    Start,
    /// Stop and keep the current recording.
    Stop,
    /// Stop and delete the current recording.
    Discard,
    /// Print the recorder's status as one line of JSON.
    Status,
    /// Control the screen recording of the current meeting.
    Screen {
        #[command(subcommand)]
        command: ScreenCmd,
    },
    /// List meetings, newest first: id, length, state and summary line.
    List,
    /// Print a meeting's notes (meeting.md).
    Show {
        /// Meeting id, as `recall list` prints it.
        id: String,
    },
    /// Queue a meeting for processing again from a step.
    Rerun {
        id: String,
        /// segment, transcribe, diarize, name, summarize or write.
        step: String,
    },
    /// Maintain the meeting index.
    Index {
        #[command(subcommand)]
        command: IndexCmd,
    },
    /// Record 5 s from the mic and the system audio and print their levels.
    Selftest,
    /// Manage the speech models.
    Models {
        #[command(subcommand)]
        command: ModelsCmd,
    },
    /// Transcribe a mic and a system recording as a meeting would be, without
    /// the summary, and print the segments.
    #[command(hide = true)]
    TranscribeFile {
        #[arg(long)]
        mic: PathBuf,
        #[arg(long)]
        computer: PathBuf,
        /// Print JSON: [{start_ms, end_ms, side, speaker, text}].
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ScreenCmd {
    /// Stop recording the screen and delete this meeting's video.
    Off,
}

#[derive(Subcommand)]
enum IndexCmd {
    /// Rebuild the index from the meeting directories.
    Rebuild,
}

#[derive(Subcommand)]
enum ModelsCmd {
    /// Download any missing model.
    Download,
}

/// How long the daemon's runtime may take to wind down its tasks once
/// `daemon::run` returns. The worker thread is not one of them.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(2);
/// A self-test through the daemon records for 5 s, then encodes and decodes.
const SELFTEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The models the pipeline loads.
const MODELS: [ModelId; 4] = [
    ModelId::SileroVad,
    ModelId::BreezeGgml,
    ModelId::PyannoteSeg,
    ModelId::CamPlusPlus,
];

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("recall: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let cmd = match cli.command {
        Cmd::Daemon => {
            let config = Config::load()?;
            let runtime = tokio::runtime::Runtime::new()?;
            let result = runtime.block_on(recall::daemon::run(config));
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
            let code = match result {
                Ok(()) => 0,
                Err(err) => {
                    eprintln!("recall: {err:#}");
                    1
                }
            };
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            // The worker thread is detached and may be inside whisper or
            // onnxruntime. A normal exit runs atexit handlers and C++ static
            // destructors that would tear those libraries down under it, so
            // leave without them; the startup sweep handles the interrupted
            // job.
            // SAFETY: `_exit` only ends the process; nothing runs after it.
            unsafe { libc::_exit(code) }
        }
        Cmd::Start => Command::Start,
        Cmd::Stop => Command::Stop,
        Cmd::Discard => Command::Discard,
        Cmd::Status => Command::Status,
        Cmd::Screen {
            command: ScreenCmd::Off,
        } => Command::ScreenOff,
        Cmd::List => return list(&Config::load()?),
        Cmd::Show { id } => return show(&Config::load()?, &id),
        Cmd::Rerun { id, step } => return rerun(&Config::load()?, &id, &step),
        Cmd::Index {
            command: IndexCmd::Rebuild,
        } => return rebuild(&Config::load()?),
        Cmd::Selftest => return run_selftest(&Config::load()?),
        Cmd::Models {
            command: ModelsCmd::Download,
        } => return download_models(&Config::load()?),
        Cmd::TranscribeFile {
            mic,
            computer,
            json,
        } => return transcribe_file(&Config::load()?, &mic, &computer, json),
    };
    let reply = control::send(cmd)?;
    println!("{}", serde_json::to_string(&reply)?);
    Ok(match reply {
        Reply::Error { .. } => ExitCode::FAILURE,
        _ => ExitCode::SUCCESS,
    })
}

fn open_index(config: &Config) -> Result<Index> {
    let path = config.index_path();
    Index::open(&path).with_context(|| format!("opening {}", path.display()))
}

fn open_meeting(config: &Config, id: &str) -> Result<MeetingDir> {
    MeetingDir::open(&config.meetings_dir, &MeetingId(id.to_owned()))
}

fn list(config: &Config) -> Result<ExitCode> {
    let mut out = std::io::stdout().lock();
    for row in open_index(config)?.list(&ListFilter::default())? {
        let secs = row.duration_ms / 1000;
        let line = format!(
            "{}  {}:{:02}  {}  {}",
            row.id.0,
            secs / 60,
            secs % 60,
            row.state,
            row.one_line.unwrap_or_default()
        );
        writeln!(out, "{}", line.trim_end())?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Prints `meeting.md`, or renders it from `meeting.json` before the Write
/// step has made it.
fn show(config: &Config, id: &str) -> Result<ExitCode> {
    let dir = open_meeting(config, id)?;
    let text = match std::fs::read_to_string(dir.markdown()) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let meeting = Meeting::load(&dir.path())?;
            let speakers = Speakers::load(&config.speakers_path())?;
            markdown::render(&meeting, &speakers, &config.summary_lang)
        }
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", dir.markdown().display()));
        }
    };
    print!("{text}");
    Ok(ExitCode::SUCCESS)
}

/// Queues the job in the index; a running daemon's worker takes it within
/// its idle poll (30 s).
fn rerun(config: &Config, id: &str, step: &str) -> Result<ExitCode> {
    let parsed: Step =
        serde_json::from_value(serde_json::Value::String(step.to_owned())).map_err(|_| {
            let names: Vec<String> = Step::ALL.into_iter().map(step_name).collect();
            anyhow::anyhow!(
                "unknown step {step:?}; expected one of {}",
                names.join(", ")
            )
        })?;
    let dir = open_meeting(config, id)?;
    let index = open_index(config)?;
    if let Some(job) = index.active_job(&dir.id)? {
        bail!(
            "meeting {:?} is being processed (job {}, from {}); try again once it finishes",
            dir.id.0,
            job.id,
            step_name(job.from_step)
        );
    }
    let job = index.enqueue(&dir.id, parsed)?;
    println!("queued job {job}: {} from {}", dir.id.0, step_name(parsed));
    Ok(ExitCode::SUCCESS)
}

fn rebuild(config: &Config) -> Result<ExitCode> {
    let speakers = Speakers::load(&config.speakers_path())?;
    let report = open_index(config)?.rebuild(&config.meetings_dir, &speakers)?;
    println!("indexed {} meetings", report.indexed);
    for (id, error) in &report.failed {
        println!("failed {}: {error}", id.0);
    }
    Ok(if report.failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Runs the self-test in the daemon when it answers on `config.port`, so the
/// test hears what the daemon would record; otherwise captures here.
fn run_selftest(config: &Config) -> Result<ExitCode> {
    eprintln!("recording {} s...", selftest::DURATION.as_secs());
    let result = match post_to_daemon(config.port, "/api/selftest")? {
        Some(body) => serde_json::from_str::<SelfTestResult>(&body)
            .with_context(|| format!("parsing the daemon's reply {body:?}"))?,
        None => selftest::run()?,
    };
    println!("mic |{}|", selftest::bar(&result.mic_peaks));
    println!("sys |{}|", selftest::bar(&result.sys_peaks));
    Ok(ExitCode::SUCCESS)
}

/// `POST`s an empty body to the daemon's HTTP server and returns the
/// response body, or `None` when nothing listens on the port. A non-2xx
/// response is an error.
fn post_to_daemon(port: u16, path: &str) -> Result<Option<String>> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(1)) {
        Ok(stream) => stream,
        Err(e) if matches!(e.kind(), ErrorKind::ConnectionRefused | ErrorKind::TimedOut) => {
            return Ok(None);
        }
        Err(e) => return Err(e).with_context(|| format!("connecting to {addr}")),
    };
    stream.set_read_timeout(Some(SELFTEST_TIMEOUT))?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\n\
         Connection: close\r\n\r\n"
    )?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .with_context(|| format!("reading the reply from {addr}"))?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .with_context(|| format!("malformed HTTP reply from {addr}"))?;
    let status = head.split(' ').nth(1).unwrap_or_default();
    if !status.starts_with('2') {
        bail!("the daemon answered {status}: {body}");
    }
    Ok(Some(body.to_owned()))
}

fn download_models(config: &Config) -> Result<ExitCode> {
    let dir = config.models_dir();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    for id in MODELS {
        let file = models::spec(id).file;
        let downloading = std::cell::Cell::new(false);
        let path = runtime.block_on(models::ensure(&dir, id, |done, total| {
            downloading.set(true);
            let pct = (done * 100).checked_div(total).unwrap_or(0);
            eprint!("\r{file}: {pct:3}% ({} / {} MB)", done >> 20, total >> 20);
        }))?;
        if downloading.get() {
            eprintln!();
        }
        println!("{file}: {}", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

/// A summary with nothing in it, so `transcribe-file` never calls a model.
struct NoSummary;

const EMPTY_SUMMARY: &str =
    r#"{"one_line":"","body":"","decisions":[],"actions":[],"chapters":[]}"#;

#[async_trait]
impl Llm for NoSummary {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        Ok(EMPTY_SUMMARY.into())
    }

    async fn stream(&self, _: &str, _: &str, tx: mpsc::Sender<String>) -> Result<()> {
        let _ = tx.send(EMPTY_SUMMARY.into()).await;
        Ok(())
    }

    fn name(&self) -> String {
        "none".into()
    }
}

#[derive(Serialize)]
struct SegmentOut {
    start_ms: u32,
    end_ms: u32,
    side: Side,
    speaker: String,
    text: String,
}

/// Builds a stereo recording from the two files (mic left, system right) in
/// a temporary meeting directory and runs the pipeline on it with the real
/// speech models. The run's index, speaker registry and meeting stay in the
/// temporary directory; the registry starts as a copy of the real one, so
/// known voices are named.
fn transcribe_file(config: &Config, mic: &Path, computer: &Path, json: bool) -> Result<ExitCode> {
    let tmp = tempfile::tempdir().context("creating a temporary directory")?;
    let run_config = Config {
        meetings_dir: tmp.path().join("meetings"),
        data_dir: tmp.path().join("data"),
        ..config.clone()
    };
    let dir = MeetingDir::create(&run_config.meetings_dir, Local::now(), "transcribe-file")?;
    let duration_ms = stereo_opus(mic, computer, &dir.audio())?;
    Meeting {
        id: dir.id.clone(),
        title: "transcribe-file".into(),
        started: chrono::Utc::now(),
        duration_ms,
        source_app: None,
        steps: Step::ALL.map(|s| (s, StepState::Pending)).into(),
        segments: Vec::new(),
        clusters: Vec::new(),
        summary: None,
        models: Default::default(),
        participants_hint: Vec::new(),
        screen_offset_ms: None,
        screen_error: None,
    }
    .save(&dir.path())?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut speakers = Speakers::load(&config.speakers_path())?;
    let mut engines = runtime.block_on(Engines::load(config, |file, done, total| {
        let pct = (done * 100).checked_div(total).unwrap_or(0);
        eprint!("\rdownloading {file}: {pct:3}%");
    }))?;
    engines.llm = std::sync::Arc::new(NoSummary);
    runtime.block_on(run_from(
        &mut engines,
        &run_config,
        &dir,
        &mut speakers,
        Step::Segment,
        |step, _| eprintln!("{} done", step_name(step)),
    ))?;

    let meeting = Meeting::load(&dir.path())?;
    let segments: Vec<SegmentOut> = meeting
        .segments
        .into_iter()
        .map(|s| SegmentOut {
            start_ms: s.start_ms,
            end_ms: s.end_ms,
            side: s.side,
            speaker: s
                .speaker
                .map(|id| speakers.name_of(&id))
                .unwrap_or_default(),
            text: s.text,
        })
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&segments)?);
    } else {
        for s in segments {
            let secs = s.start_ms / 1000;
            println!("{}:{:02} {}: {}", secs / 60, secs % 60, s.speaker, s.text);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Encodes `left` and `right`, each downmixed to mono and the shorter padded
/// with silence, as one 48 kHz stereo Opus file at `out`. Returns its length
/// in milliseconds.
fn stereo_opus(left: &Path, right: &Path, out: &Path) -> Result<u32> {
    let secs = duration_secs(left)?.max(duration_secs(right)?);
    let mono = "aresample=48000,aformat=sample_fmts=fltp:channel_layouts=mono,apad";
    let status = Process::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(left)
        .arg("-i")
        .arg(right)
        .arg("-filter_complex")
        .arg(format!(
            "[0:a]{mono}[l];[1:a]{mono}[r];[l][r]amerge=inputs=2[a]"
        ))
        .args(["-map", "[a]", "-t"])
        .arg(format!("{secs:.3}"))
        .args(["-c:a", "libopus", "-b:a", "64k"])
        .arg(out)
        .status()
        .context("running ffmpeg")?;
    if !status.success() {
        bail!(
            "ffmpeg could not combine {} and {}",
            left.display(),
            right.display()
        );
    }
    Ok((secs * 1000.0).round() as u32)
}

fn duration_secs(path: &Path) -> Result<f64> {
    let output = Process::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .context("running ffprobe")?;
    if !output.status.success() {
        bail!(
            "ffprobe could not read {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim()
        .parse()
        .with_context(|| format!("ffprobe gave no duration for {}: {text:?}", path.display()))
}
