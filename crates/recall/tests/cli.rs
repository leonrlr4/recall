use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn status_without_a_daemon_fails_with_a_hint() {
    let runtime = tempfile::tempdir().unwrap();
    Command::cargo_bin("recall")
        .unwrap()
        .arg("status")
        .env("XDG_RUNTIME_DIR", runtime.path())
        .assert()
        .failure()
        .stderr(contains("is `recall daemon` running?"));
}

mod store {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use assert_cmd::Command;
    use chrono::{Local, TimeZone, Utc};
    use recall_store::{Config, Index, Meeting, MeetingDir, Speakers, Step, StepState, Summary};
    use tempfile::TempDir;

    /// A config file pointing at temporary meetings and data directories,
    /// holding two meetings: a done one at 09:00 and one still processing
    /// at 11:00.
    pub struct Env {
        pub tmp: TempDir,
        pub config: Config,
        pub done: MeetingDir,
        pub processing: MeetingDir,
    }

    impl Env {
        pub fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let config = Config {
                meetings_dir: tmp.path().join("Meetings"),
                data_dir: tmp.path().join("data"),
                summary_lang: "en".into(),
                ..Config::default()
            };
            std::fs::write(
                tmp.path().join("config.toml"),
                toml_config(&config.meetings_dir, &config.data_dir),
            )
            .unwrap();
            let speakers = Speakers::default();
            let index = Index::open(&config.index_path()).unwrap();
            let done = meeting(&config, &index, &speakers, 9, "Weekly sync", true);
            let processing = meeting(&config, &index, &speakers, 11, "Design review", false);
            Self {
                tmp,
                config,
                done,
                processing,
            }
        }

        pub fn recall(&self) -> Command {
            let mut cmd = Command::cargo_bin("recall").unwrap();
            cmd.env("RECALL_CONFIG", self.tmp.path().join("config.toml"))
                .env("RECALL_HOME", self.tmp.path().join("home"))
                .env("XDG_RUNTIME_DIR", self.tmp.path().join("run"));
            cmd
        }

        pub fn index(&self) -> Index {
            Index::open(&self.config.index_path()).unwrap()
        }
    }

    fn toml_config(meetings: &Path, data: &Path) -> String {
        format!(
            "meetings_dir = {:?}\ndata_dir = {:?}\nsummary_lang = \"en\"\n",
            meetings.display().to_string(),
            data.display().to_string()
        )
    }

    fn meeting(
        config: &Config,
        index: &Index,
        speakers: &Speakers,
        hour: u32,
        title: &str,
        done: bool,
    ) -> MeetingDir {
        let started = Local.with_ymd_and_hms(2026, 10, 27, hour, 0, 0).unwrap();
        let dir = MeetingDir::create(&config.meetings_dir, started, title).unwrap();
        let at = Utc::now();
        let meeting = Meeting {
            id: dir.id.clone(),
            title: title.into(),
            started: started.to_utc(),
            duration_ms: 754_000,
            source_app: None,
            steps: Step::ALL
                .iter()
                .map(|s| {
                    let state = if done || *s == Step::Segment {
                        StepState::Done { at }
                    } else {
                        StepState::Pending
                    };
                    (*s, state)
                })
                .collect(),
            segments: Vec::new(),
            clusters: Vec::new(),
            summary: done.then(|| Summary {
                one_line: format!("{title} went fine."),
                ..Summary::default()
            }),
            models: BTreeMap::new(),
            participants_hint: Vec::new(),
            screen_offset_ms: None,
            screen_error: None,
            sync: false,
            calendar: None,
        };
        meeting.save(&dir.path()).unwrap();
        std::fs::write(dir.audio(), b"opus").unwrap();
        if done {
            std::fs::write(dir.markdown(), format!("# {title}\n\nNotes.\n")).unwrap();
        }
        index.upsert(&meeting, speakers, true).unwrap();
        dir
    }

    pub fn id(dir: &MeetingDir) -> &str {
        &dir.id.0
    }

    pub fn path(dir: &MeetingDir) -> PathBuf {
        dir.path()
    }
}

use recall_store::Step;
use store::{Env, id};

#[test]
fn list_prints_meetings_from_index() {
    let env = Env::new();
    let out = env.recall().arg("list").assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        [
            format!("{}  12:34  processing:transcribe", id(&env.processing)),
            format!("{}  12:34  done  Weekly sync went fine.", id(&env.done)),
        ]
    );
}

#[test]
fn show_prints_markdown() {
    let env = Env::new();
    env.recall()
        .args(["show", id(&env.done)])
        .assert()
        .success()
        .stdout("# Weekly sync\n\nNotes.\n");
    env.recall()
        .args(["show", "2026-10-27 08:00 Nothing"])
        .assert()
        .failure()
        .stderr(contains("no meeting directory"));
}

#[test]
fn rerun_enqueues() {
    let env = Env::new();
    env.recall()
        .args(["rerun", id(&env.done), "summarize"])
        .assert()
        .success()
        .stdout(contains("queued job"));
    let job = env.index().active_job(&env.done.id).unwrap().unwrap();
    assert_eq!(job.from_step, Step::Summarize);

    env.recall()
        .args(["rerun", id(&env.processing), "bogus"])
        .assert()
        .failure()
        .stderr(contains("unknown step \"bogus\""));
}

#[test]
fn rerun_on_busy_meeting_fails() {
    let env = Env::new();
    let job = env
        .index()
        .enqueue(&env.processing.id, Step::Segment)
        .unwrap();
    env.recall()
        .args(["rerun", id(&env.processing), "write"])
        .assert()
        .failure()
        .stderr(contains("is being processed"));
    let active = env.index().active_job(&env.processing.id).unwrap().unwrap();
    assert_eq!((active.id, active.from_step), (job, Step::Segment));
}

#[test]
fn index_rebuild_reports_counts() {
    let env = Env::new();
    std::fs::write(store::path(&env.processing).join("meeting.json"), "{").unwrap();
    env.recall()
        .args(["index", "rebuild"])
        .assert()
        .failure()
        .stdout(contains("indexed 1 meetings"))
        .stdout(contains(format!("failed {}: ", id(&env.processing))));
    let rows = env
        .index()
        .list(&recall_store::ListFilter::default())
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, env.done.id);
}
