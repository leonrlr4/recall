//! The global speaker registry stored in `speakers.json`.
//!
//! `meeting.json` stores only speaker ids; names live here, so a rename
//! happens in one place.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::meeting::SpeakerId;

/// Id of the person at the microphone. The registry always contains it.
pub const ME: &str = "me";
const ME_NAME: &str = "Me";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Person {
    pub id: SpeakerId,
    pub name: String,
    /// Other names this person has gone by, e.g. names merged into it.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Voice embeddings, oldest first.
    #[serde(default)]
    pub embeddings: Vec<Vec<f32>>,
}

impl Person {
    fn new(id: SpeakerId, name: &str) -> Self {
        Self {
            id,
            name: name.to_string(),
            aliases: Vec::new(),
            embeddings: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Speakers {
    people: Vec<Person>,
}

/// A registry holding only `me`.
impl Default for Speakers {
    fn default() -> Self {
        Self {
            people: vec![Person::new(SpeakerId(ME.into()), ME_NAME)],
        }
    }
}

impl Speakers {
    /// Reads the registry from `path`. A missing file gives the default
    /// registry; a file without `me` gets it added at the front.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut speakers: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        let me = SpeakerId(ME.into());
        if speakers.get(&me).is_none() {
            speakers.people.insert(0, Person::new(me, ME_NAME));
        }
        Ok(speakers)
    }

    /// Writes the registry as pretty JSON through a temporary file and a
    /// rename, creating the parent directory if needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(self)?;
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(&json)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))
    }

    pub fn me(&self) -> &Person {
        self.get(&SpeakerId(ME.into()))
            .expect("load and default always include me, and merge never drops it")
    }

    pub fn people(&self) -> &[Person] {
        &self.people
    }

    pub fn get(&self, id: &SpeakerId) -> Option<&Person> {
        self.people.iter().find(|p| &p.id == id)
    }

    fn get_mut(&mut self, id: &SpeakerId) -> Option<&mut Person> {
        self.people.iter_mut().find(|p| &p.id == id)
    }

    /// The person's name, or the raw id when the id is unknown.
    pub fn name_of(&self, id: &SpeakerId) -> String {
        self.get(id)
            .map_or_else(|| id.0.clone(), |p| p.name.clone())
    }

    /// Adds a new person with a fresh uuid id.
    pub fn add(&mut self, name: &str, embedding: Option<Vec<f32>>) -> SpeakerId {
        let id = SpeakerId(uuid::Uuid::new_v4().to_string());
        let mut person = Person::new(id.clone(), name);
        person.embeddings.extend(embedding);
        self.people.push(person);
        id
    }

    pub fn rename(&mut self, id: &SpeakerId, name: &str) -> Result<()> {
        let Some(person) = self.get_mut(id) else {
            bail!("no speaker with id {:?}", id.0);
        };
        person.name = name.to_string();
        Ok(())
    }

    /// Appends an embedding, dropping the oldest ones beyond `max_per_person`.
    /// An unknown id is ignored.
    pub fn add_embedding(&mut self, id: &SpeakerId, e: Vec<f32>, max_per_person: usize) {
        if let Some(person) = self.get_mut(id) {
            person.embeddings.push(e);
            let excess = person.embeddings.len().saturating_sub(max_per_person);
            person.embeddings.drain(..excess);
        }
    }

    /// Folds `drop` into `keep`: `drop`'s embeddings and aliases move over,
    /// its name becomes an alias of `keep`, and `drop` is removed. `me` can
    /// be kept but never dropped.
    pub fn merge(&mut self, keep: &SpeakerId, drop: &SpeakerId) -> Result<()> {
        if keep == drop {
            bail!("cannot merge speaker {:?} into itself", keep.0);
        }
        if drop.0 == ME {
            bail!("cannot merge away {ME:?}");
        }
        if self.get(keep).is_none() {
            bail!("no speaker with id {:?}", keep.0);
        }
        let Some(i) = self.people.iter().position(|p| &p.id == drop) else {
            bail!("no speaker with id {:?}", drop.0);
        };
        let dropped = self.people.remove(i);
        let kept = self.get_mut(keep).expect("checked above");
        kept.embeddings.extend(dropped.embeddings);
        for alias in std::iter::once(dropped.name).chain(dropped.aliases) {
            if alias != kept.name && !kept.aliases.contains(&alias) {
                kept.aliases.push(alias);
            }
        }
        Ok(())
    }

    /// The person whose closest embedding is most similar to `e`, if that
    /// similarity is at least `threshold`.
    pub fn best_match(&self, e: &[f32], threshold: f32) -> Option<(SpeakerId, f32)> {
        self.people
            .iter()
            .flat_map(|p| p.embeddings.iter().map(move |x| (p, cosine(e, x))))
            .filter(|(_, score)| *score >= threshold)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(p, score)| (p.id.clone(), score))
    }
}

/// Cosine similarity. Vectors of different length, empty vectors and zero
/// vectors score 0.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> SpeakerId {
        SpeakerId(s.into())
    }

    #[test]
    fn speakers_default_has_me() {
        let dir = tempfile::tempdir().unwrap();
        let speakers = Speakers::load(&dir.path().join("speakers.json")).unwrap();
        assert_eq!(speakers.me().id, id(ME));
        assert_eq!(speakers.me().name, "Me");
        assert_eq!(speakers.name_of(&id(ME)), "Me");
        assert_eq!(speakers.name_of(&id("unknown")), "unknown");
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("speakers.json");
        let mut speakers = Speakers::load(&path).unwrap();
        let tom = speakers.add("Tom", Some(vec![0.5, 0.5]));
        speakers.save(&path).unwrap();
        let loaded = Speakers::load(&path).unwrap();
        assert_eq!(loaded, speakers);
        assert_eq!(loaded.name_of(&tom), "Tom");
    }

    #[test]
    fn load_adds_me_when_file_lacks_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("speakers.json");
        std::fs::write(&path, r#"{"people":[{"id":"x","name":"Tom"}]}"#).unwrap();
        let speakers = Speakers::load(&path).unwrap();
        assert_eq!(speakers.me().name, "Me");
        assert_eq!(speakers.name_of(&id("x")), "Tom");
    }

    #[test]
    fn rename_and_merge_move_embeddings() {
        let mut speakers = Speakers::default();
        let a = speakers.add("Speaker 1", Some(vec![1.0, 0.0]));
        let b = speakers.add("Tommy", Some(vec![0.0, 1.0]));
        speakers.rename(&a, "Tom").unwrap();
        assert_eq!(speakers.name_of(&a), "Tom");
        assert!(speakers.rename(&id("missing"), "X").is_err());

        speakers.add_embedding(&b, vec![0.5, 0.5], 10);
        speakers.merge(&a, &b).unwrap();
        assert!(speakers.get(&b).is_none());
        let tom = speakers.get(&a).unwrap();
        assert_eq!(
            tom.embeddings,
            vec![vec![1.0, 0.0], vec![0.0, 1.0], vec![0.5, 0.5]]
        );
        assert_eq!(tom.aliases, vec!["Tommy".to_string()]);
        assert!(speakers.merge(&a, &a).is_err());
        assert!(speakers.merge(&a, &id(ME)).is_err());
        assert!(speakers.merge(&a, &id("missing")).is_err());
    }

    #[test]
    fn add_embedding_keeps_the_newest() {
        let mut speakers = Speakers::default();
        let a = speakers.add("Tom", None);
        for i in 0..5 {
            speakers.add_embedding(&a, vec![i as f32], 3);
        }
        assert_eq!(
            speakers.get(&a).unwrap().embeddings,
            vec![vec![2.0], vec![3.0], vec![4.0]]
        );
    }

    #[test]
    fn best_match_picks_highest_cosine_above_threshold() {
        let mut speakers = Speakers::default();
        let first = speakers.add("First", Some(vec![1.0, 0.0]));
        speakers.add("Second", Some(vec![0.0, 1.0]));
        let (got, score) = speakers.best_match(&[0.9, 0.1], 0.6).unwrap();
        assert_eq!(got, first);
        assert!(score > 0.99 && score < 1.0);
        assert!(speakers.best_match(&[0.9, 0.1], 0.995).is_none());
    }

    #[test]
    fn cosine_handles_degenerate_input() {
        assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
    }
}
