//! The on-disk chunk cache that holds a recording until it is finalized.
//!
//! Each track is a sequence of raw little-endian f32 mono files at 48 kHz,
//! `mic-00000.f32`, `mic-00001.f32`, ... and `sys-00000.f32`, ..., each
//! holding at most 20 s of samples. A crash loses only the samples still in
//! the open chunk's write buffer; every chunk on disk stays readable.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::opus::OggOpusWriter;

const SAMPLE_RATE: u32 = 48_000;
/// 20 s of mono samples at 48 kHz.
const CHUNK_SAMPLES: usize = 960_000;
const BITRATE: i32 = 64_000;
/// Stereo frames interleaved per step of `finalize`.
const FINALIZE_BLOCK: usize = 48_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    Mic,
    Sys,
}

impl Track {
    fn prefix(self) -> &'static str {
        match self {
            Track::Mic => "mic",
            Track::Sys => "sys",
        }
    }
}

struct TrackFiles {
    track: Track,
    /// Index of the chunk the next sample goes to.
    index: u32,
    /// The open chunk, created on the first sample pushed into it.
    file: Option<BufWriter<File>>,
    /// Samples already in the open chunk.
    in_chunk: usize,
}

impl TrackFiles {
    /// Starts after the highest chunk already in `cache`, so a writer reopened
    /// on a cache (after a crash or restart) never truncates existing chunks.
    fn open(cache: &Path, track: Track) -> Result<Self> {
        let index = match chunks(cache, track)?.last() {
            Some((last, _)) => last + 1,
            None => 0,
        };
        Ok(Self {
            track,
            index,
            file: None,
            in_chunk: 0,
        })
    }
}

/// Appends captured samples to the chunk cache.
pub struct ChunkWriter {
    cache: PathBuf,
    mic: TrackFiles,
    sys: TrackFiles,
}

impl ChunkWriter {
    /// Creates `cache` if needed. Chunk files appear only once samples arrive;
    /// each track continues after the chunks already in `cache`.
    pub fn new(cache: &Path) -> Result<Self> {
        std::fs::create_dir_all(cache).with_context(|| format!("creating {}", cache.display()))?;
        Ok(Self {
            cache: cache.to_path_buf(),
            mic: TrackFiles::open(cache, Track::Mic)?,
            sys: TrackFiles::open(cache, Track::Sys)?,
        })
    }

    pub fn push(&mut self, track: Track, mut samples: &[f32]) -> Result<()> {
        let files = match track {
            Track::Mic => &mut self.mic,
            Track::Sys => &mut self.sys,
        };
        while !samples.is_empty() {
            let file = match &mut files.file {
                Some(file) => file,
                None => {
                    let path = chunk_path(&self.cache, files.track, files.index);
                    let file = File::create(&path)
                        .with_context(|| format!("creating {}", path.display()))?;
                    files.file.insert(BufWriter::new(file))
                }
            };
            let take = samples.len().min(CHUNK_SAMPLES - files.in_chunk);
            let mut bytes = Vec::with_capacity(take * 4);
            for sample in &samples[..take] {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            file.write_all(&bytes).context("writing a chunk")?;
            files.in_chunk += take;
            samples = &samples[take..];
            if files.in_chunk == CHUNK_SAMPLES {
                // Move to the next chunk before flushing: if the flush fails,
                // the next push must open a new file, not recreate (and
                // truncate) this one.
                let mut full = files.file.take().expect("chunk is open");
                files.index += 1;
                files.in_chunk = 0;
                full.flush().context("flushing a full chunk")?;
            }
        }
        Ok(())
    }

    /// Writes buffered samples of both open chunks to the OS.
    pub fn flush(&mut self) -> Result<()> {
        for files in [&mut self.mic, &mut self.sys] {
            if let Some(file) = &mut files.file {
                file.flush().context("flushing a chunk")?;
            }
        }
        Ok(())
    }
}

fn chunk_path(cache: &Path, track: Track, index: u32) -> PathBuf {
    cache.join(format!("{}-{index:05}.f32", track.prefix()))
}

/// The track's chunk files with their indices, in numeric order.
fn chunks(cache: &Path, track: Track) -> Result<Vec<(u32, PathBuf)>> {
    let prefix = format!("{}-", track.prefix());
    let mut found = Vec::new();
    for entry in std::fs::read_dir(cache).with_context(|| format!("reading {}", cache.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(index) = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".f32"))
            .and_then(|digits| digits.parse::<u32>().ok())
        else {
            continue;
        };
        found.push((index, entry.path()));
    }
    found.sort();
    Ok(found)
}

fn chunk_paths(cache: &Path, track: Track) -> Result<Vec<PathBuf>> {
    Ok(chunks(cache, track)?
        .into_iter()
        .map(|(_, path)| path)
        .collect())
}

/// Whole samples stored in the track's chunks. A trailing partial sample left
/// by a crash mid-write is not counted.
fn track_samples(paths: &[PathBuf]) -> Result<u64> {
    let mut total = 0;
    for path in paths {
        let len = std::fs::metadata(path)
            .with_context(|| format!("reading {}", path.display()))?
            .len();
        total += len / 4;
    }
    Ok(total)
}

fn samples_to_duration(samples: u64) -> Duration {
    let nanos = u128::from(samples) * 1_000_000_000 / u128::from(SAMPLE_RATE);
    Duration::from_nanos(nanos as u64)
}

/// Reads one track's chunks back as a single sample stream, one chunk in
/// memory at a time.
struct TrackReader {
    paths: VecDeque<PathBuf>,
    current: Vec<f32>,
    pos: usize,
}

impl TrackReader {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            paths: paths.into(),
            current: Vec::new(),
            pos: 0,
        }
    }

    /// Fills `out` from the front and returns how many samples it wrote;
    /// fewer than `out.len()` only once the track is exhausted.
    fn read(&mut self, out: &mut [f32]) -> Result<usize> {
        let mut filled = 0;
        while filled < out.len() {
            if self.pos == self.current.len() {
                let Some(path) = self.paths.pop_front() else {
                    break;
                };
                let bytes =
                    std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                if bytes.len() % 4 != 0 {
                    tracing::warn!(
                        path = %path.display(),
                        "chunk ends in a partial sample; dropping it"
                    );
                }
                self.current = bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b))
                    .collect();
                self.pos = 0;
                continue;
            }
            let take = (out.len() - filled).min(self.current.len() - self.pos);
            out[filled..filled + take].copy_from_slice(&self.current[self.pos..self.pos + take]);
            filled += take;
            self.pos += take;
        }
        Ok(filled)
    }
}

/// Encodes the cache into a stereo Opus file at `out`, mic on the left and
/// system audio on the right, padding the shorter track with silence. Returns
/// the length of the longer track.
pub fn finalize(cache: &Path, out: &Path) -> Result<Duration> {
    let mic_paths = chunk_paths(cache, Track::Mic)?;
    let sys_paths = chunk_paths(cache, Track::Sys)?;
    if mic_paths.is_empty() && sys_paths.is_empty() {
        bail!("no audio chunks in {}", cache.display());
    }
    let total = track_samples(&mic_paths)?.max(track_samples(&sys_paths)?);

    let mut mic = TrackReader::new(mic_paths);
    let mut sys = TrackReader::new(sys_paths);
    let mut writer = OggOpusWriter::create(out, SAMPLE_RATE, 2, BITRATE)?;
    let mut mic_block = vec![0.0f32; FINALIZE_BLOCK];
    let mut sys_block = vec![0.0f32; FINALIZE_BLOCK];
    let mut stereo = Vec::with_capacity(FINALIZE_BLOCK * 2);
    loop {
        let mic_len = mic.read(&mut mic_block)?;
        let sys_len = sys.read(&mut sys_block)?;
        let len = mic_len.max(sys_len);
        if len == 0 {
            break;
        }
        mic_block[mic_len..len].fill(0.0);
        sys_block[sys_len..len].fill(0.0);
        stereo.clear();
        for (left, right) in mic_block[..len].iter().zip(&sys_block[..len]) {
            stereo.push(sanitize(*left));
            stereo.push(sanitize(*right));
        }
        writer.write_interleaved(&stereo)?;
    }
    writer.finish()?;
    Ok(samples_to_duration(total))
}

/// Maps a captured sample into the encoder's range: non-finite values (a
/// corrupt chunk, a misbehaving source) become silence, the rest is clamped
/// to [-1, 1].
fn sanitize(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// The length of the longer track in the cache.
pub fn cache_duration(cache: &Path) -> Result<Duration> {
    let mic = track_samples(&chunk_paths(cache, Track::Mic)?)?;
    let sys = track_samples(&chunk_paths(cache, Track::Sys)?)?;
    Ok(samples_to_duration(mic.max(sys)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, seconds: usize) -> Vec<f32> {
        (0..seconds * 48_000)
            .map(|i| 0.3 * (2.0 * std::f32::consts::PI * freq * i as f32 / 48_000.0).sin())
            .collect()
    }

    fn file_len(path: &Path) -> u64 {
        std::fs::metadata(path).unwrap().len()
    }

    #[test]
    fn chunks_roll_over_every_20s() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(".cache");
        let mut writer = ChunkWriter::new(&cache).unwrap();
        // Two full chunks plus ten samples, pushed in slices that do not
        // divide the chunk length.
        let samples = vec![0.25f32; 2 * 960_000 + 10];
        for slice in samples.chunks(7_001) {
            writer.push(Track::Mic, slice).unwrap();
        }
        writer.flush().unwrap();

        assert_eq!(file_len(&cache.join("mic-00000.f32")), 960_000 * 4);
        assert_eq!(file_len(&cache.join("mic-00001.f32")), 960_000 * 4);
        assert_eq!(file_len(&cache.join("mic-00002.f32")), 10 * 4);
        assert!(!cache.join("mic-00003.f32").exists());
        assert!(!cache.join("sys-00000.f32").exists());

        let bytes = std::fs::read(cache.join("mic-00002.f32")).unwrap();
        assert_eq!(f32::from_le_bytes(bytes[..4].try_into().unwrap()), 0.25);
        assert_eq!(
            cache_duration(&cache).unwrap(),
            Duration::from_nanos((2 * 960_000 + 10) * 1_000_000_000 / 48_000)
        );
    }

    #[test]
    fn finalize_pads_shorter_track() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(".cache");
        let mut writer = ChunkWriter::new(&cache).unwrap();
        writer.push(Track::Mic, &sine(440.0, 3)).unwrap();
        writer.push(Track::Sys, &sine(660.0, 2)).unwrap();
        writer.flush().unwrap();
        drop(writer);

        assert_eq!(cache_duration(&cache).unwrap(), Duration::from_secs(3));
        let out = dir.path().join("audio.opus");
        let duration = finalize(&cache, &out).unwrap();
        assert_eq!(duration, Duration::from_secs(3));
        assert!(file_len(&out) > 0);
    }

    #[test]
    fn new_writer_continues_after_existing_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(".cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("mic-00000.f32"), 0.5f32.to_le_bytes()).unwrap();

        let mut writer = ChunkWriter::new(&cache).unwrap();
        writer.push(Track::Mic, &[0.25]).unwrap();
        writer.push(Track::Sys, &[0.25]).unwrap();
        writer.flush().unwrap();

        assert_eq!(file_len(&cache.join("mic-00000.f32")), 4);
        assert_eq!(file_len(&cache.join("mic-00001.f32")), 4);
        assert_eq!(file_len(&cache.join("sys-00000.f32")), 4);
    }

    #[test]
    fn finalize_survives_non_finite_and_out_of_range_samples() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(".cache");
        let mut writer = ChunkWriter::new(&cache).unwrap();
        let mut samples = sine(440.0, 1);
        samples[100] = f32::NAN;
        samples[200] = f32::INFINITY;
        samples[300] = 7.0;
        writer.push(Track::Mic, &samples).unwrap();
        writer.flush().unwrap();
        drop(writer);

        assert_eq!(sanitize(f32::NAN), 0.0);
        assert_eq!(sanitize(f32::NEG_INFINITY), 0.0);
        assert_eq!(sanitize(7.0), 1.0);
        assert_eq!(sanitize(-7.0), -1.0);
        let out = dir.path().join("audio.opus");
        finalize(&cache, &out).unwrap();
        assert!(file_len(&out) > 0);
    }

    #[test]
    fn finalize_on_empty_cache_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(".cache");
        std::fs::create_dir_all(&cache).unwrap();
        let out = dir.path().join("audio.opus");
        assert!(finalize(&cache, &out).is_err());
        assert!(!out.exists());
    }
}
