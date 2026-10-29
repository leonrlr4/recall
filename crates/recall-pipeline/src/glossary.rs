//! Names and project terms that the transcriber and the summary model should
//! spell right, taken from the config, the speaker registry, a list Claude
//! Code chose from the user's notes, or the notes themselves.
//!
//! The chosen list is the curated file (`<data_dir>/glossary.json`, see
//! [`Curated`]), written by `recall glossary refresh`. Without it, terms are
//! extracted from notes deterministically: a term is a ticket id
//! (`PROJ-220`), an acronym (`TRL6`, `BII`), a CamelCase name (`BixBench`,
//! `scRNA`) or a capitalised word written mid-sentence (`Capsule`, `Zenodo`),
//! and a `people/` directory names one person per subdirectory. Chinese terms
//! are not extracted: Breeze already spells Chinese well, and segmenting it
//! would need a dictionary.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use recall_store::Config;
use recall_store::config::GlossaryConfig;
use recall_store::speakers::{ME, Speakers};
use serde::{Deserialize, Serialize};

/// A directory with this name holds one subdirectory (or note) per person,
/// named after them.
const PEOPLE_DIR: &str = "people";
/// Notes larger than this are skipped: notes are hand-written, and a large
/// Markdown file is an export or a log whose terms are noise.
const MAX_NOTE_BYTES: u64 = 1 << 20;
/// At most this many notes are read per build, so a source pointed at a
/// huge tree cannot stall a run.
const MAX_NOTES: usize = 5_000;
/// A term found in notes must occur at least this often in total; one-off
/// words are more often typos or quotes than vocabulary.
const MIN_COUNT: usize = 2;
/// A capitalised word must be in at least this many notes. Within one note
/// they are often the title words of a reference list ("Genome Biology",
/// "Science").
const MIN_NOTES_CAPITALISED: usize = 2;
/// The curated file's name under `data_dir`.
const CURATED_FILE: &str = "glossary.json";

/// Where a glossary term came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// `[glossary] terms`.
    Typed,
    /// A name given to a voice.
    Voice,
    /// The curated file.
    Curated,
    /// A `people/` directory under the sources.
    People,
    /// Extracted from the notes under the sources.
    Notes,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Typed => "terms",
            Self::Voice => "voice",
            Self::Curated => "curated",
            Self::People => "people",
            Self::Notes => "notes",
        }
    }
}

/// The terms, most useful first, at most `max_terms` of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Glossary {
    terms: Vec<String>,
    /// Where each of `terms` came from, by position.
    origins: Vec<Origin>,
}

impl Glossary {
    /// [`Glossary::build`] with the curated file under `config.data_dir`.
    /// A curated file that cannot be read counts as missing.
    pub fn load(config: &Config, speakers: &Speakers) -> Self {
        let curated = Curated::load(&Curated::path(&config.data_dir))
            .ok()
            .flatten();
        Self::build(&config.glossary, curated.as_ref(), speakers)
    }

    /// The user's own `terms` first, in their order, then the names given
    /// to voices in `speakers`, then the `curated` terms or, without any,
    /// the people and terms found in `sources`; each name once (ignoring
    /// case), cut to `max_terms`. Sources that cannot be read are skipped.
    pub fn build(config: &GlossaryConfig, curated: Option<&Curated>, speakers: &Speakers) -> Self {
        if config.max_terms == 0 {
            return Self::default();
        }
        let typed = config.terms.iter().map(|t| (t.clone(), Origin::Typed));
        let named = speakers
            .people()
            .iter()
            .filter(|p| p.named && p.id.0 != ME && !is_cluster_label(&p.name))
            .map(|p| (p.name.clone(), Origin::Voice));
        let rest: Vec<(String, Origin)> = match curated.filter(|c| !c.terms.is_empty()) {
            Some(curated) => curated
                .terms
                .iter()
                .map(|t| (t.term.clone(), Origin::Curated))
                .collect(),
            None => {
                let notes = NoteTerms::read(&config.source_paths());
                let people = notes.people.into_iter().map(|p| (p, Origin::People));
                let terms = notes.terms.into_iter().map(|t| (t.term, Origin::Notes));
                people.chain(terms).collect()
            }
        };
        Self::from_terms(typed.chain(named).chain(rest), config.max_terms)
    }

    /// `terms` trimmed, without empties and repeats (ignoring case), cut to
    /// `max_terms`.
    pub fn from_terms(terms: impl IntoIterator<Item = (String, Origin)>, max_terms: usize) -> Self {
        let mut seen = HashSet::new();
        let (terms, origins) = terms
            .into_iter()
            .map(|(t, origin)| (t.trim().replace('\0', ""), origin))
            .filter(|(t, _)| !t.is_empty() && seen.insert(t.to_lowercase()))
            .take(max_terms)
            .unzip();
        Self { terms, origins }
    }

    pub fn terms(&self) -> &[String] {
        &self.terms
    }

    /// Each term with where it came from.
    pub fn entries(&self) -> impl Iterator<Item = (&str, Origin)> {
        self.terms
            .iter()
            .map(String::as_str)
            .zip(self.origins.iter().copied())
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }
}

/// The terms Claude Code chose from the user's notes, most useful first, as
/// `recall glossary refresh` saves them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Curated {
    pub refreshed: DateTime<Utc>,
    /// The model that chose them.
    pub model: String,
    pub terms: Vec<CuratedTerm>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CuratedTerm {
    pub term: String,
    /// Why the term is in the list, for `recall glossary show`.
    #[serde(default)]
    pub reason: String,
}

impl Curated {
    /// `<data_dir>/glossary.json`.
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(CURATED_FILE)
    }

    /// The file at `path`; None when there is none.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        serde_json::from_str(&text)
            .map(Some)
            .with_context(|| format!("parsing {}", path.display()))
    }

    /// Writes the file through a temporary file and a rename, so a run that
    /// reads it meanwhile sees the old list or the new one, never half.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }
}

/// A term extracted from the notes, with how often it occurs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteTerm {
    pub term: String,
    /// Notes it occurs in.
    pub notes: usize,
    /// Occurrences in all notes.
    pub count: usize,
}

/// What extraction finds in the note sources.
#[derive(Debug, Default)]
pub struct NoteTerms {
    /// Names from `people/` directories, in path order.
    pub people: Vec<String>,
    /// Terms seen at least [`MIN_COUNT`] times, those in more notes first.
    pub terms: Vec<NoteTerm>,
}

impl NoteTerms {
    pub fn read(sources: &[PathBuf]) -> Self {
        let notes = Notes::read(sources);
        Self {
            terms: notes.counts.ranked(),
            people: notes.people,
        }
    }
}

/// Opening of the recognition prompt. A natural sentence that contains the
/// terms works better than a bare list: a comma-separated word list raised
/// the error rate on the other words (arXiv 2502.11572). Written in
/// Traditional Chinese so the prompt also leans Breeze toward Traditional
/// output.
const PROMPT_OPENING: &str = "這次會議提到 ";
const PROMPT_SEPARATOR: &str = "、";
const PROMPT_CLOSING: &str = "。";

/// Whether `name` is a label recall gives a voice nobody has named,
/// `Remote 1` and so on. Voices registered before the registry recorded
/// whether a name was given count as named, but still carry such labels.
fn is_cluster_label(name: &str) -> bool {
    name.strip_prefix("Remote ")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The recognition prompt for `terms`: one sentence naming as many of them,
/// in order, as `fits` accepts, such as
/// 「這次會議提到 Alice、Bob、PROJ、TRL6。」. None when not even the
/// first term fits, or there are none.
pub fn prompt_sentence(terms: &[String], fits: impl Fn(&str) -> bool) -> Option<String> {
    let sentence = |n: usize| {
        format!(
            "{PROMPT_OPENING}{}{PROMPT_CLOSING}",
            terms[..n].join(PROMPT_SEPARATOR)
        )
    };
    let n = (1..=terms.len())
        .take_while(|&n| fits(&sentence(n)))
        .last()?;
    Some(sentence(n))
}

/// The paragraph added to the summary prompt: the known spellings, so the
/// model writes a misheard name or term the way it is spelled. Empty when
/// the glossary is.
pub fn summary_note(terms: &[String]) -> String {
    if terms.is_empty() {
        return String::new();
    }
    format!(
        "\nThe transcript comes from speech recognition, which can mishear names and terms. \
         These are the correct spellings of names and terms that may come up: {}. \
         Where a word in the transcript sounds like one of them and fits the context, \
         write it with that spelling.\n",
        terms.join(", ")
    )
}

/// What was read from the note sources.
#[derive(Debug, Default)]
struct Notes {
    /// Names from `people/` directories, in path order.
    people: Vec<String>,
    counts: TermCounts,
}

impl Notes {
    fn read(sources: &[PathBuf]) -> Self {
        let mut notes = Self::default();
        let mut files = Vec::new();
        for source in sources {
            collect(source, &mut files, &mut notes.people);
        }
        files.sort();
        files.dedup();
        for (i, file) in files.iter().take(MAX_NOTES).enumerate() {
            let too_big = fs::metadata(file).map_or(true, |m| m.len() > MAX_NOTE_BYTES);
            if too_big {
                continue;
            }
            if let Ok(text) = fs::read_to_string(file) {
                notes.counts.add_note(i, &text);
            }
        }
        notes
    }
}

/// Adds the Markdown files at or under `path` to `files`, and the people
/// named by any `people/` directory on the way to `people`. Hidden entries
/// (`.obsidian`, `.git`) and those starting with `_` (`_attachments`) are
/// skipped, and symbolic links to directories are not followed.
fn collect(path: &Path, files: &mut Vec<PathBuf>, people: &mut Vec<String>) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.is_file() {
        if is_markdown(path) {
            files.push(path.to_path_buf());
        }
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    let is_people = path.file_name().is_some_and(|n| n == PEOPLE_DIR);
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(['.', '_']) {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let child = entry.path();
        if is_people && kind.is_dir() {
            people.extend(person_name(&name));
        }
        if kind.is_dir() {
            collect(&child, files, people);
        } else if is_markdown(&child) {
            files.push(child);
        }
    }
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
}

/// A person's name from their directory name: `alice` is `Alice`,
/// `mary-jane` is `Mary Jane`. Names already holding capitals keep them.
/// None for names with digits or nothing alphabetic.
fn person_name(dir: &str) -> Option<String> {
    if dir.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    let words: Vec<String> = dir
        .split(['-', '_', ' '])
        .filter(|w| w.chars().any(char::is_alphabetic))
        .map(|w| {
            let mut chars = w.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// How often each candidate term occurs, and in how many notes.
#[derive(Debug, Default)]
struct TermCounts {
    /// Term to (notes it occurs in, total occurrences, last note seen).
    terms: HashMap<String, (usize, usize, usize)>,
    /// Lowercase word to occurrences written in lowercase; a capitalised
    /// word that is as often written in lowercase is an ordinary word.
    lowercase: HashMap<String, usize>,
    /// Capitalised words seen mid-sentence, counted apart from `terms`
    /// until checked against `lowercase`.
    capitalised: HashMap<String, (usize, usize, usize)>,
}

impl TermCounts {
    /// Counts the candidate terms in note number `note`.
    fn add_note(&mut self, note: usize, text: &str) {
        let prose = prose(text);
        for (start, word) in words(&prose) {
            let sentence_start = prose[..start]
                .chars()
                .rev()
                .find(|c| *c != ' ' && *c != '\t')
                .is_none_or(|c| !(c.is_alphanumeric() || matches!(c, ',' | ')' | '、' | '，')));
            for kind in classify(word) {
                match kind {
                    Kind::Term(t) => bump(&mut self.terms, t, note),
                    Kind::Capitalised(t) if !sentence_start && !is_common(t) => {
                        bump(&mut self.capitalised, t, note)
                    }
                    Kind::Capitalised(_) => {}
                    Kind::Lower(t) => *self.lowercase.entry(t.to_string()).or_default() += 1,
                }
            }
        }
    }

    /// Terms seen at least [`MIN_COUNT`] times, those in more notes first,
    /// then by total count, then alphabetically so the order is stable.
    fn ranked(&self) -> Vec<NoteTerm> {
        let capitalised = self.capitalised.iter().filter(|(word, (_, count, _))| {
            let lower = self.lowercase.get(&word.to_lowercase()).copied();
            lower.unwrap_or(0) < *count
        });
        let capitalised = capitalised.filter(|(_, (notes, _, _))| *notes >= MIN_NOTES_CAPITALISED);
        let mut all: Vec<(&String, usize, usize)> = self
            .terms
            .iter()
            .chain(capitalised)
            .filter(|(_, (_, count, _))| *count >= MIN_COUNT)
            .map(|(t, (notes, count, _))| (t, *notes, *count))
            .collect();
        all.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then(a.0.cmp(b.0)));
        all.into_iter()
            .map(|(t, notes, count)| NoteTerm {
                term: t.clone(),
                notes,
                count,
            })
            .collect()
    }
}

fn bump(map: &mut HashMap<String, (usize, usize, usize)>, term: &str, note: usize) {
    let entry = map.entry(term.to_string()).or_insert((0, 0, usize::MAX));
    if entry.2 != note {
        entry.0 += 1;
        entry.2 = note;
    }
    entry.1 += 1;
}

/// The prose of a note: YAML front matter (except its `description`),
/// fenced code blocks, inline code, URLs and link targets removed, since
/// code identifiers and paths are not spoken. Markup characters become
/// spaces, so they end words without joining them.
fn prose(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut lines = text.lines().peekable();
    if lines.peek().is_some_and(|l| l.trim_end() == "---") {
        lines.next();
        for line in lines.by_ref() {
            if line.trim_end() == "---" {
                break;
            }
            if let Some(description) = line.trim_start().strip_prefix("description:") {
                out.push_str(description);
                out.push('\n');
            }
        }
    }
    let mut fenced = false;
    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if !fenced {
            out.push_str(&strip_inline(line));
            out.push('\n');
        }
    }
    out
}

/// One line without inline code spans, URLs and Markdown link targets.
fn strip_inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('`') {
            // An unclosed backtick drops the rest of the line.
            rest = after.find('`').map_or("", |end| &after[end + 1..]);
            out.push(' ');
        } else if let Some(after) = rest.strip_prefix("](") {
            rest = after.find(')').map_or("", |end| &after[end + 1..]);
            out.push(' ');
        } else if rest.starts_with("http://") || rest.starts_with("https://") {
            rest = rest
                .find(char::is_whitespace)
                .map_or("", |end| &rest[end..]);
            out.push(' ');
        } else {
            let c = rest.chars().next().expect("not empty");
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// The ASCII words of `text` with their byte offsets: runs of letters,
/// digits and the joiners `-`, `_`, `.`, `/`, `'` and `@`, without joiners at
/// either end.
fn words(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let is_word =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '\'' | '@');
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < text.len() {
            let rest = &text[i..];
            let Some(start) = rest.find(is_word) else {
                i = text.len();
                return None;
            };
            let begin = i + start;
            let len = text[begin..]
                .find(|c: char| !is_word(c))
                .unwrap_or(text.len() - begin);
            i = begin + len;
            let raw = &text[begin..begin + len];
            let word = raw.trim_matches(|c: char| !c.is_ascii_alphanumeric());
            if !word.is_empty() {
                let offset = begin + raw.find(word).expect("trimmed from raw");
                return Some((offset, word));
            }
        }
        None
    })
}

/// What a word counts as.
#[derive(Debug, PartialEq, Eq)]
enum Kind<'a> {
    /// A term in its own right: a ticket id, an acronym or a CamelCase name.
    Term(&'a str),
    /// A capitalised word, a name only if written so mid-sentence.
    Capitalised(&'a str),
    /// A lowercase word.
    Lower(&'a str),
}

/// The candidate terms in one word. A word with a path or identifier
/// joiner (`/`, `_`, `.`, `@`) is code or an address and gives none. A
/// ticket id (`PROJ-220`) gives itself and its project key; other
/// hyphenated words are classified part by part (`scRNA-seq` gives `scRNA`).
fn classify(word: &str) -> Vec<Kind<'_>> {
    if word.contains(['/', '_', '.', '@']) {
        return Vec::new();
    }
    if let Some((key, number)) = word.split_once('-')
        && is_acronym(key)
        && !number.is_empty()
        && number.len() <= 6
        && number.bytes().all(|b| b.is_ascii_digit())
    {
        let mut kinds = vec![Kind::Term(word)];
        if !is_common_acronym(key) {
            kinds.push(Kind::Term(key));
        }
        return kinds;
    }
    word.split('-').filter_map(classify_part).collect()
}

fn classify_part(part: &str) -> Option<Kind<'_>> {
    let part = part.trim_matches('\'');
    // A plural acronym (`CPUs`, `PRs`) is the acronym.
    let part = part
        .strip_suffix('s')
        .filter(|singular| is_acronym(singular))
        .unwrap_or(part);
    let letters = part.chars().filter(char::is_ascii_alphabetic).count();
    let upper = part.chars().filter(char::is_ascii_uppercase).count();
    if is_acronym(part) {
        return (!is_common_acronym(part)).then_some(Kind::Term(part));
    }
    if part.contains('\'') || part.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    let first_upper = part.starts_with(|c: char| c.is_ascii_uppercase());
    if letters >= 4 && part.chars().skip(1).any(|c| c.is_ascii_uppercase()) {
        // CamelCase or a lowercase prefix on capitals: BixBench, scRNA.
        // All-capitals words were taken as acronyms above; shorter ones are
        // mostly units (GiB).
        return Some(Kind::Term(part));
    }
    if first_upper && letters >= 3 && upper == 1 {
        return Some(Kind::Capitalised(part));
    }
    (upper == 0 && letters >= 3).then_some(Kind::Lower(part))
}

/// Two to twelve capitals and digits, at least two of them letters, not
/// starting with a digit: `TRL6`, `BII`, `ATAC`.
fn is_acronym(word: &str) -> bool {
    (2..=12).contains(&word.len())
        && word.starts_with(|c: char| c.is_ascii_uppercase())
        && word
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && word.chars().filter(char::is_ascii_uppercase).count() >= 2
}

/// Acronyms every speech model already spells, or that are markup and
/// units rather than vocabulary. Listing them would only use up the prompt.
const COMMON_ACRONYMS: &[&str] = &[
    "AI", "AM", "API", "APIS", "CD", "CEO", "CI", "CLI", "CPU", "CSS", "CSV", "CTO", "DB", "DM",
    "DNS", "EN", "ETA", "FAQ", "FYI", "GB", "GPU", "HTML", "HTTP", "HTTPS", "ID", "IDS", "II",
    "III", "IO", "IP", "IV", "JS", "JSON", "KB", "LLM", "MB", "MD", "NA", "OK", "OS", "PDF", "PM",
    "PR", "PRS", "QA", "README", "SDK", "SHA", "SQL", "SSH", "TB", "TBD", "TODO", "TS", "TW", "UI",
    "URL", "USB", "UTC", "UX", "VI", "VII", "VS", "XML", "YAML", "ZH",
];

fn is_common_acronym(word: &str) -> bool {
    COMMON_ACRONYMS.contains(&word)
}

/// English words that are often capitalised mid-sentence in notes (titles,
/// labels after a dash, days and months) without being names. The
/// lowercase check in [`TermCounts::ranked`] catches the rest.
const COMMON_WORDS: &[&str] = &[
    "about",
    "after",
    "all",
    "also",
    "and",
    "any",
    "approach",
    "april",
    "are",
    "august",
    "back",
    "because",
    "before",
    "both",
    "but",
    "can",
    "case",
    "check",
    "context",
    "current",
    "data",
    "december",
    "default",
    "description",
    "done",
    "each",
    "example",
    "february",
    "for",
    "friday",
    "from",
    "goal",
    "has",
    "have",
    "how",
    "if",
    "into",
    "issue",
    "its",
    "january",
    "july",
    "june",
    "march",
    "may",
    "monday",
    "more",
    "must",
    "new",
    "next",
    "not",
    "note",
    "notes",
    "november",
    "now",
    "october",
    "once",
    "only",
    "open",
    "other",
    "our",
    "out",
    "plan",
    "problem",
    "result",
    "results",
    "saturday",
    "see",
    "september",
    "should",
    "status",
    "step",
    "summary",
    "sunday",
    "task",
    "test",
    "that",
    "the",
    "then",
    "there",
    "these",
    "they",
    "this",
    "thursday",
    "todo",
    "tuesday",
    "update",
    "wednesday",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "why",
    "will",
    "with",
    "without",
    "yes",
    "you",
    "your",
];

fn is_common(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    COMMON_WORDS.contains(&lower.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|t| t.to_string()).collect()
    }

    fn names(terms: Vec<NoteTerm>) -> Vec<String> {
        terms.into_iter().map(|t| t.term).collect()
    }

    #[test]
    fn classify_finds_tickets_acronyms_and_names() {
        use Kind::*;
        assert_eq!(classify("PROJ-220"), [Term("PROJ-220"), Term("PROJ")]);
        assert_eq!(classify("PR-12"), [Term("PR-12")]);
        assert_eq!(classify("TRL6"), [Term("TRL6")]);
        assert_eq!(classify("FASTQs"), [Term("FASTQ")]);
        assert_eq!(classify("BixBench"), [Term("BixBench")]);
        assert_eq!(classify("scRNA-seq"), [Term("scRNA"), Lower("seq")]);
        assert_eq!(classify("Zenodo"), [Capitalised("Zenodo")]);
        assert_eq!(classify("capsule"), [Lower("capsule")]);
        for none in [
            "API",
            "app/core/internal.py",
            "concurrent_runs",
            "v1.2",
            "10x",
            "a",
            "it's",
            "leon@example.com",
            "R2",
            "GiB",
            "PRs",
        ] {
            assert!(classify(none).is_empty(), "{none}");
        }
    }

    #[test]
    fn prose_drops_front_matter_code_and_links() {
        let note = "---\nname: trl6-notes\ndescription: TRL6 凍結\nmetadata:\n  originSessionId: X1\n---\n\
                    Text with `inline_code` and [a link](https://example.com/FooBar).\n\
                    ```rust\nlet BadTerm = 1;\n```\nhttps://example.com/QuxQux after\n";
        let p = prose(note);
        assert!(p.contains("TRL6 凍結"));
        assert!(p.contains("[a link"));
        assert!(p.contains("after"));
        for gone in [
            "trl6-notes",
            "originSessionId",
            "inline_code",
            "FooBar",
            "BadTerm",
            "QuxQux",
        ] {
            assert!(!p.contains(gone), "{gone} in {p:?}");
        }
    }

    #[test]
    fn ranking_prefers_terms_in_more_notes() {
        let mut counts = TermCounts::default();
        // BixBench is in two notes; PROJ-208 three times in one.
        counts.add_note(0, "今天看 BixBench 和 PROJ-208，PROJ-208 PROJ-208。");
        counts.add_note(
            1,
            "BixBench 再跑一次，跟 Tommy 確認。\nThe result went to Tommy.",
        );
        counts.add_note(2, "Once a TRL6 run ends, Tommy said Problem solved.");
        let ranked = counts.ranked();
        assert_eq!(
            ranked[3],
            NoteTerm {
                term: "PROJ-208".into(),
                notes: 1,
                count: 3
            }
        );
        assert_eq!(names(ranked), ["Tommy", "BixBench", "PROJ", "PROJ-208"]);
    }

    #[test]
    fn capitalised_words_count_only_mid_sentence_and_when_not_common() {
        let mut counts = TermCounts::default();
        // Sentence starts: never counted. "Capsule" is written in lowercase
        // more often than capitalised, so it is an ordinary word.
        counts.add_note(0, "Report one. Report two.\n# Heading\n- Bullet\n- Bullet");
        counts.add_note(1, "用 Capsule 跑，capsule 很多 capsule。用 Capsule。");
        counts.add_note(
            2,
            "Ask Zenodo, then ask Zenodo. Ask the Monday group on Monday.",
        );
        // In one note only: a capitalised word needs two.
        counts.add_note(3, "Nature Genome Biology, Genome Biology.");
        counts.add_note(4, "上傳 Zenodo。");
        assert_eq!(names(counts.ranked()), ["Zenodo"]);
    }

    #[test]
    fn notes_read_people_dirs_and_markdown_only() {
        let tmp = tempfile::tempdir().unwrap();
        let people = tmp.path().join("vault/people");
        for dir in ["alice/trl6", "bob", "mary-jane", "_attachments", "295"] {
            fs::create_dir_all(people.join(dir)).unwrap();
        }
        fs::create_dir_all(tmp.path().join("vault/.obsidian")).unwrap();
        fs::write(people.join("alice/trl6/a.md"), "PROJ-239 和 PROJ-239").unwrap();
        fs::write(people.join("bob/b.txt"), "NOTMD NOTMD").unwrap();
        fs::write(tmp.path().join("vault/.obsidian/c.md"), "HIDDEN HIDDEN").unwrap();
        let single = tmp.path().join("single.md");
        fs::write(&single, "ATAC 門檻 ATAC").unwrap();

        let notes = NoteTerms::read(&[
            tmp.path().join("vault"),
            single.clone(),
            tmp.path().join("missing"),
        ]);

        assert_eq!(notes.people, ["Alice", "Bob", "Mary Jane"]);
        assert_eq!(names(notes.terms), ["ATAC", "PROJ", "PROJ-239"]);
    }

    #[test]
    fn build_orders_user_terms_speakers_people_then_notes() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("people/alice")).unwrap();
        fs::write(tmp.path().join("n.md"), "TRL6 和 TRL6，跟 tommy 說 Alice").unwrap();
        let mut speakers = Speakers::default();
        speakers.add("Bob", None);
        speakers.add_unnamed("Remote 1", None);
        // Saved before names were tracked: named, but still a label.
        speakers.add("Remote 2", None);
        let config = GlossaryConfig {
            terms: s(&["capsule whitepaper", " bob ", ""]),
            sources: vec![tmp.path().to_path_buf()],
            max_terms: 30,
            ..GlossaryConfig::default()
        };
        let glossary = Glossary::build(&config, None, &speakers);
        // "Me" and the unnamed voice are left out; Bob is already a user
        // term and Alice already a person.
        assert_eq!(
            glossary.terms(),
            ["capsule whitepaper", "bob", "Alice", "TRL6"]
        );
        let origins: Vec<Origin> = glossary.entries().map(|(_, o)| o).collect();
        use Origin::*;
        assert_eq!(origins, [Typed, Typed, People, Notes]);

        let short = Glossary::build(
            &GlossaryConfig {
                max_terms: 2,
                ..config.clone()
            },
            None,
            &speakers,
        );
        assert_eq!(short.terms(), ["capsule whitepaper", "bob"]);
        let off = Glossary::build(
            &GlossaryConfig {
                max_terms: 0,
                ..config
            },
            None,
            &speakers,
        );
        assert!(off.is_empty());
    }

    fn curated(terms: &[&str]) -> Curated {
        Curated {
            refreshed: "2026-10-29T08:00:00Z".parse().unwrap(),
            model: "claude-opus-5-5".into(),
            terms: terms
                .iter()
                .map(|t| CuratedTerm {
                    term: t.to_string(),
                    reason: format!("why {t}"),
                })
                .collect(),
        }
    }

    #[test]
    fn build_uses_the_curated_terms_instead_of_the_notes() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("people/alice")).unwrap();
        fs::write(tmp.path().join("n.md"), "Exhibit 和 Exhibit，ZZTOP ZZTOP").unwrap();
        let mut speakers = Speakers::default();
        speakers.add("Bob", None);
        let config = GlossaryConfig {
            terms: s(&["TRL6"]),
            sources: vec![tmp.path().to_path_buf()],
            ..GlossaryConfig::default()
        };
        let list = curated(&["Alice", "trl6", "PROJ"]);

        let glossary = Glossary::build(&config, Some(&list), &speakers);
        let entries: Vec<(&str, Origin)> = glossary.entries().collect();
        assert_eq!(
            entries,
            [
                ("TRL6", Origin::Typed),
                ("Bob", Origin::Voice),
                ("Alice", Origin::Curated),
                ("PROJ", Origin::Curated),
            ]
        );
        // An empty list counts as none.
        let empty = Glossary::build(&config, Some(&curated(&[])), &speakers);
        assert_eq!(empty.terms(), ["TRL6", "Bob", "Alice", "ZZTOP"]);
    }

    #[test]
    fn curated_file_round_trips_and_missing_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Curated::path(&tmp.path().join("data"));
        assert_eq!(Curated::load(&path).unwrap(), None);
        let list = curated(&["TRL6"]);
        list.save(&path).unwrap();
        assert_eq!(Curated::load(&path).unwrap(), Some(list));
        fs::write(&path, "not json").unwrap();
        assert!(Curated::load(&path).is_err());

        let config = Config {
            data_dir: tmp.path().join("data"),
            ..Config::default()
        };
        // An unreadable file falls back to the notes (none here).
        assert!(Glossary::load(&config, &Speakers::default()).is_empty());
    }

    #[test]
    fn person_name_capitalises_dir_names() {
        assert_eq!(person_name("alice").as_deref(), Some("Alice"));
        assert_eq!(person_name("mary-jane").as_deref(), Some("Mary Jane"));
        assert_eq!(person_name("McKay").as_deref(), Some("McKay"));
        assert_eq!(person_name("295"), None);
        assert_eq!(person_name("-"), None);
    }

    #[test]
    fn prompt_sentence_stops_before_the_limit() {
        let terms = s(&["Alice", "Bob", "PROJ", "TRL6"]);
        assert_eq!(
            prompt_sentence(&terms, |_| true).as_deref(),
            Some("這次會議提到 Alice、Bob、PROJ、TRL6。")
        );
        // 19 bytes of opening, 6 + 3 + 6 bytes of names, 3 of closing.
        let two = prompt_sentence(&terms, |p| p.len() <= 37).unwrap();
        assert_eq!(two, "這次會議提到 Alice、Bob。");
        assert_eq!(prompt_sentence(&terms, |p| p.len() <= 20), None);
        assert_eq!(prompt_sentence(&[], |_| true), None);
    }

    #[test]
    fn summary_note_lists_spellings() {
        assert_eq!(summary_note(&[]), "");
        let note = summary_note(&s(&["Alice", "TRL6"]));
        assert!(note.contains("Alice, TRL6."), "{note}");
        assert!(note.contains("speech recognition"));
    }
}
