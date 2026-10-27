//! Meeting directories under the recordings root.
//!
//! Each meeting lives in `<root>/<YYYY-MM-DD HH:MM> <title>/`. The directory
//! name is the [`MeetingId`]; because it starts with the local start time,
//! sorting names lexically sorts meetings by time.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveDateTime};

use crate::meeting::MeetingId;

const MEETING_JSON: &str = "meeting.json";
const AUDIO: &str = "audio.opus";
const MARKDOWN: &str = "meeting.md";
const CACHE: &str = ".cache";
/// Raw capture chunks in the cache; their presence means a recording never
/// finished encoding.
const CACHE_EXT: &str = "f32";

const TIME_FORMAT: &str = "%Y-%m-%d %H:%M";
/// Byte length of `"YYYY-MM-DD HH:MM "`, the prefix every id starts with.
const PREFIX_LEN: usize = 17;
const MAX_TITLE_CHARS: usize = 80;
const DEFAULT_TITLE: &str = "Meeting";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeetingDir {
    pub root: PathBuf,
    pub id: MeetingId,
}

impl MeetingDir {
    /// Creates `<root>/<started> <title>`, creating `root` if needed. The
    /// title is passed through [`sanitize_title`]; if the name is taken, the
    /// first free of `" (2)"`, `" (3)"`, ... is appended.
    pub fn create(root: &Path, started: DateTime<Local>, title: &str) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
        let base = format!("{} {}", started.format(TIME_FORMAT), sanitize_title(title));
        for name in candidates(&base) {
            let path = root.join(&name);
            match fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        root: root.to_path_buf(),
                        id: MeetingId(name),
                    });
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
        unreachable!("candidates() is infinite")
    }

    /// Opens an existing meeting directory. Errors if it is not a directory.
    pub fn open(root: &Path, id: &MeetingId) -> Result<Self> {
        let dir = Self {
            root: root.to_path_buf(),
            id: id.clone(),
        };
        if !dir.path().is_dir() {
            bail!("no meeting directory at {}", dir.path().display());
        }
        Ok(dir)
    }

    /// Ids of meeting directories that contain `meeting.json`, newest first.
    /// A missing `root` has no meetings.
    pub fn list(root: &Path) -> Result<Vec<MeetingId>> {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", root.display()))?;
            if !entry.path().join(MEETING_JSON).is_file() {
                continue;
            }
            // Ids are strings; a non-UTF-8 name was not made by `create`.
            if let Ok(name) = entry.file_name().into_string() {
                ids.push(MeetingId(name));
            }
        }
        ids.sort_unstable_by(|a, b| b.cmp(a));
        Ok(ids)
    }

    pub fn path(&self) -> PathBuf {
        self.root.join(&self.id.0)
    }

    pub fn audio(&self) -> PathBuf {
        self.path().join(AUDIO)
    }

    pub fn markdown(&self) -> PathBuf {
        self.path().join(MARKDOWN)
    }

    pub fn cache(&self) -> PathBuf {
        self.path().join(CACHE)
    }

    /// True when `.cache/` holds at least one `*.f32` capture chunk.
    pub fn has_unfinished_cache(&self) -> bool {
        let Ok(entries) = fs::read_dir(self.cache()) else {
            return false;
        };
        entries
            .flatten()
            .any(|e| e.path().extension().is_some_and(|ext| ext == CACHE_EXT))
    }

    /// Removes `.cache/` and everything in it. An absent cache is not an error.
    pub fn clear_cache(&self) -> Result<()> {
        let cache = self.cache();
        match fs::remove_dir_all(&cache) {
            Err(e) if e.kind() != ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", cache.display()))
            }
            _ => Ok(()),
        }
    }

    /// Renames the directory to `<same time prefix> <title>`, sanitizing the
    /// title and suffixing `" (2)"`, ... on collision like [`create`]. Renaming
    /// to the name it already has (including its own suffix) changes nothing.
    ///
    /// Only the directory moves; `meeting.json`'s `id` and `title` are the
    /// caller's to update.
    ///
    /// [`create`]: MeetingDir::create
    pub fn rename(self, title: &str) -> Result<Self> {
        let prefix = self
            .id
            .0
            .get(..PREFIX_LEN)
            .filter(|p| {
                p.ends_with(' ')
                    && NaiveDateTime::parse_from_str(&p[..PREFIX_LEN - 1], TIME_FORMAT).is_ok()
            })
            .with_context(|| format!("meeting id {:?} has no time prefix", self.id.0))?;
        let base = format!("{prefix}{}", sanitize_title(title));
        for name in candidates(&base) {
            if name == self.id.0 {
                return Ok(self);
            }
            let to = self.root.join(&name);
            // `fs::rename` would silently replace an empty directory, so
            // check first. A dangling symlink still counts as taken.
            if to.symlink_metadata().is_ok() {
                continue;
            }
            let from = self.path();
            fs::rename(&from, &to)
                .with_context(|| format!("renaming {} to {}", from.display(), to.display()))?;
            return Ok(Self {
                root: self.root,
                id: MeetingId(name),
            });
        }
        unreachable!("candidates() is infinite")
    }
}

/// `base`, then `base (2)`, `base (3)`, ...
fn candidates(base: &str) -> impl Iterator<Item = String> + '_ {
    std::iter::once(base.to_string()).chain((2u32..).map(move |n| format!("{base} ({n})")))
}

/// Makes `title` safe as the title part of a directory name: `/` and NUL
/// become spaces, whitespace runs collapse to one space, the ends are
/// trimmed, and the result is cut to 80 characters. An empty result becomes
/// `"Meeting"`.
pub fn sanitize_title(title: &str) -> String {
    let cleaned = title.replace(['/', '\0'], " ");
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = collapsed.chars().take(MAX_TITLE_CHARS).collect();
    let trimmed = truncated.trim_end();
    if trimmed.is_empty() {
        DEFAULT_TITLE.to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 27, h, m, 0).unwrap()
    }

    fn names(ids: &[MeetingId]) -> Vec<&str> {
        ids.iter().map(|id| id.0.as_str()).collect()
    }

    #[test]
    fn create_names_dir_by_time_and_title() {
        let root = tempfile::tempdir().unwrap();
        let dir = MeetingDir::create(root.path(), at(14, 30), "Biobuild weekly").unwrap();
        assert_eq!(dir.id.0, "2026-10-27 14:30 Biobuild weekly");
        assert_eq!(
            dir.path(),
            root.path().join("2026-10-27 14:30 Biobuild weekly")
        );
        assert!(dir.path().is_dir());
        assert_eq!(dir.audio(), dir.path().join("audio.opus"));
        assert_eq!(dir.markdown(), dir.path().join("meeting.md"));
        assert_eq!(dir.cache(), dir.path().join(".cache"));
    }

    #[test]
    fn create_sanitizes_slash_and_empty() {
        let root = tempfile::tempdir().unwrap();
        let slash = MeetingDir::create(root.path(), at(9, 0), "a/b").unwrap();
        assert_eq!(slash.id.0, "2026-10-27 09:00 a b");
        let empty = MeetingDir::create(root.path(), at(10, 0), "").unwrap();
        assert_eq!(empty.id.0, "2026-10-27 10:00 Meeting");
    }

    #[test]
    fn sanitize_title_rules() {
        assert_eq!(sanitize_title("  a \t\n b\0c  "), "a b c");
        assert_eq!(sanitize_title(" / "), "Meeting");
        let long = "x".repeat(100);
        assert_eq!(sanitize_title(&long).chars().count(), 80);
        // Truncation counts characters, never splits one, and drops a
        // trailing space it leaves behind.
        let cjk = format!("{} {}", "會".repeat(79), "議");
        assert_eq!(sanitize_title(&cjk), "會".repeat(79));
    }

    #[test]
    fn create_suffixes_on_collision() {
        let root = tempfile::tempdir().unwrap();
        let first = MeetingDir::create(root.path(), at(14, 30), "Sync").unwrap();
        let second = MeetingDir::create(root.path(), at(14, 30), "Sync").unwrap();
        let third = MeetingDir::create(root.path(), at(14, 30), "Sync").unwrap();
        assert_eq!(first.id.0, "2026-10-27 14:30 Sync");
        assert_eq!(second.id.0, "2026-10-27 14:30 Sync (2)");
        assert_eq!(third.id.0, "2026-10-27 14:30 Sync (3)");
    }

    #[test]
    fn open_requires_an_existing_dir() {
        let root = tempfile::tempdir().unwrap();
        let made = MeetingDir::create(root.path(), at(14, 30), "Sync").unwrap();
        let opened = MeetingDir::open(root.path(), &made.id).unwrap();
        assert_eq!(opened.path(), made.path());
        assert!(MeetingDir::open(root.path(), &MeetingId("nope".into())).is_err());
    }

    #[test]
    fn list_is_newest_first_and_skips_dirs_without_json() {
        let root = tempfile::tempdir().unwrap();
        for (h, title) in [(9, "Early"), (16, "Late"), (11, "Middle")] {
            let dir = MeetingDir::create(root.path(), at(h, 0), title).unwrap();
            fs::write(dir.path().join(MEETING_JSON), "{}").unwrap();
        }
        MeetingDir::create(root.path(), at(23, 0), "No json").unwrap();
        fs::write(root.path().join("stray.txt"), "").unwrap();
        let ids = MeetingDir::list(root.path()).unwrap();
        assert_eq!(
            names(&ids),
            [
                "2026-10-27 16:00 Late",
                "2026-10-27 11:00 Middle",
                "2026-10-27 09:00 Early"
            ]
        );
    }

    #[test]
    fn list_of_missing_root_is_empty() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            MeetingDir::list(&root.path().join("absent"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn cache_is_unfinished_only_with_f32_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = MeetingDir::create(root.path(), at(14, 30), "Sync").unwrap();
        assert!(!dir.has_unfinished_cache());
        fs::create_dir(dir.cache()).unwrap();
        fs::write(dir.cache().join("notes.txt"), "").unwrap();
        assert!(!dir.has_unfinished_cache());
        fs::write(dir.cache().join("mic.f32"), [0u8; 4]).unwrap();
        assert!(dir.has_unfinished_cache());
        dir.clear_cache().unwrap();
        assert!(!dir.cache().exists());
        assert!(!dir.has_unfinished_cache());
        // Clearing an absent cache is not an error.
        dir.clear_cache().unwrap();
    }

    #[test]
    fn rename_keeps_time_prefix() {
        let root = tempfile::tempdir().unwrap();
        let dir = MeetingDir::create(root.path(), at(14, 30), "Untitled").unwrap();
        fs::write(dir.audio(), b"opus").unwrap();
        let old = dir.path();
        let renamed = dir.rename("Biobuild / weekly").unwrap();
        assert_eq!(renamed.id.0, "2026-10-27 14:30 Biobuild weekly");
        assert!(!old.exists());
        assert_eq!(fs::read(renamed.audio()).unwrap(), b"opus");
    }

    #[test]
    fn rename_suffixes_on_collision_and_to_same_title_is_a_no_op() {
        let root = tempfile::tempdir().unwrap();
        MeetingDir::create(root.path(), at(14, 30), "Sync").unwrap();
        let other = MeetingDir::create(root.path(), at(14, 30), "Other").unwrap();
        let renamed = other.rename("Sync").unwrap();
        assert_eq!(renamed.id.0, "2026-10-27 14:30 Sync (2)");
        let same = renamed.rename("Sync").unwrap();
        assert_eq!(same.id.0, "2026-10-27 14:30 Sync (2)");
        assert!(same.path().is_dir());
    }
}
