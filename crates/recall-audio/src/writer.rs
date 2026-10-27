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
    file: Option<BufWriter<ChunkFile>>,
    /// Samples pushed into the open chunk, written out or still buffered.
    in_chunk: usize,
    /// Samples a failed write lost, written as silence before the next
    /// samples so this track stays aligned with the other one.
    owed: u64,
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
            owed: 0,
        })
    }

    /// Writes the silence owed, then `samples`. On a failure, the samples
    /// that did not reach the disk are owed and the error is returned.
    fn push(&mut self, cache: &Path, disk: &Disk, samples: &[f32]) -> Result<()> {
        while self.owed > 0 {
            let take = self.owed.min(SILENCE.len() as u64) as usize;
            self.owed -= take as u64;
            self.write(cache, disk, &SILENCE[..take])
                .inspect_err(|_| self.owed += samples.len() as u64)?;
        }
        self.write(cache, disk, samples)
    }

    fn write(&mut self, cache: &Path, disk: &Disk, mut samples: &[f32]) -> Result<()> {
        while !samples.is_empty() {
            let file = match &mut self.file {
                Some(file) => file,
                None => {
                    let path = chunk_path(cache, self.track, self.index);
                    match ChunkFile::create(&path, disk) {
                        Ok(file) => self.file.insert(BufWriter::new(file)),
                        Err(err) => {
                            self.owed += samples.len() as u64;
                            return Err(err);
                        }
                    }
                }
            };
            let take = samples.len().min(CHUNK_SAMPLES - self.in_chunk);
            let mut bytes = Vec::with_capacity(take * 4);
            for sample in &samples[..take] {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            let written = file.write_all(&bytes);
            self.in_chunk += take;
            samples = &samples[take..];
            let result = match written {
                Ok(()) if self.in_chunk == CHUNK_SAMPLES => {
                    let flushed = file.flush();
                    if flushed.is_ok() {
                        self.file = None;
                        self.index += 1;
                        self.in_chunk = 0;
                    }
                    flushed.context("flushing a full chunk")
                }
                Ok(()) => Ok(()),
                Err(err) => Err(err).context("writing a chunk"),
            };
            if let Err(err) = result {
                self.abandon_chunk();
                self.owed += samples.len() as u64;
                return Err(err);
            }
        }
        Ok(())
    }

    /// Writes the open chunk's buffered samples to the OS.
    fn flush(&mut self) -> Result<()> {
        let Some(file) = &mut self.file else {
            return Ok(());
        };
        let flushed = file.flush().context("flushing a chunk");
        if flushed.is_err() {
            self.abandon_chunk();
        }
        flushed
    }

    /// After a failed write: closes the open chunk without writing its
    /// buffer, cuts it back to whole samples, owes the samples pushed into
    /// it that are not on disk, and moves on, so the next push opens a new
    /// chunk instead of appending after a partial sample.
    fn abandon_chunk(&mut self) {
        let Some(file) = self.file.take() else {
            return;
        };
        let (file, _unwritten) = file.into_parts();
        let path = chunk_path(&PathBuf::new(), self.track, self.index);
        let kept = match file.cut_to_whole_samples() {
            Ok(kept) => kept,
            Err(err) => {
                tracing::error!(
                    err = format!("{err:#}"),
                    chunk = %path.display(),
                    "cutting a chunk after a failed write"
                );
                0
            }
        };
        self.owed += (self.in_chunk as u64).saturating_sub(kept);
        self.index += 1;
        self.in_chunk = 0;
    }
}

/// Silence written in place of samples a failed write lost.
static SILENCE: [f32; 4_800] = [0.0; 4_800];

/// Test hook standing in for the free space on the disk: `None` is the real
/// disk, `Some(bytes)` lets that many more bytes be written across all chunk
/// files, after which writes fail as on a full disk.
#[derive(Clone, Default)]
struct Disk(Option<std::sync::Arc<std::sync::atomic::AtomicU64>>);

/// One chunk file, writing through [`Disk`].
struct ChunkFile {
    file: File,
    disk: Disk,
}

impl ChunkFile {
    fn create(path: &Path, disk: &Disk) -> Result<Self> {
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        Ok(Self {
            file,
            disk: disk.clone(),
        })
    }

    /// Truncates the file to a multiple of 4 bytes and returns the whole
    /// samples it holds.
    fn cut_to_whole_samples(&self) -> std::io::Result<u64> {
        let len = self.file.metadata()?.len();
        let whole = len / 4 * 4;
        if whole != len {
            self.file.set_len(whole)?;
        }
        Ok(whole / 4)
    }
}

impl Write for ChunkFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let Some(free) = &self.disk.0 else {
            return self.file.write(buf);
        };
        let n = (free.load(std::sync::atomic::Ordering::SeqCst) as usize).min(buf.len());
        if n == 0 && !buf.is_empty() {
            return Err(std::io::Error::from_raw_os_error(28)); // ENOSPC
        }
        free.fetch_sub(n as u64, std::sync::atomic::Ordering::SeqCst);
        self.file.write(&buf[..n])
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// Appends captured samples to the chunk cache.
///
/// A failed write (a full disk) closes that track's chunk, cuts it back to
/// whole samples and returns the error; the next push opens a new chunk and
/// first writes silence for every sample that did not reach the disk, so
/// both tracks keep one timeline.
pub struct ChunkWriter {
    cache: PathBuf,
    mic: TrackFiles,
    sys: TrackFiles,
    disk: Disk,
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
            disk: Disk::default(),
        })
    }

    /// Lets only `free` more bytes reach the chunk files; see [`Disk`].
    #[cfg(test)]
    pub(crate) fn with_free_space(
        mut self,
        free: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        self.disk = Disk(Some(free));
        self
    }

    pub fn push(&mut self, track: Track, samples: &[f32]) -> Result<()> {
        let files = match track {
            Track::Mic => &mut self.mic,
            Track::Sys => &mut self.sys,
        };
        files.push(&self.cache, &self.disk, samples)
    }

    /// Writes buffered samples of both open chunks to the OS. Returns the
    /// first error; both tracks are flushed regardless.
    pub fn flush(&mut self) -> Result<()> {
        let mic = self.mic.flush();
        let sys = self.sys.flush();
        mic.and(sys)
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

    /// Every chunk of `track` in `cache`, read back in order.
    fn read_track(cache: &Path, track: Track) -> Vec<f32> {
        chunk_paths(cache, track)
            .unwrap()
            .iter()
            .flat_map(|path| {
                let bytes = std::fs::read(path).unwrap();
                assert_eq!(
                    bytes.len() % 4,
                    0,
                    "{} is not whole samples",
                    path.display()
                );
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn full_disk_cuts_chunks_to_whole_samples_and_keeps_tracks_aligned() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(".cache");
        // Room for 10 000 samples and half of one more, shared by the tracks.
        let free = Arc::new(AtomicU64::new(40_002));
        let mut writer = ChunkWriter::new(&cache)
            .unwrap()
            .with_free_space(free.clone());
        let block = 3_000;
        let mut errors = 0;
        // Blocks 0..8 alternate between the tracks; the disk fills during
        // block 2 or 3, and space comes back before block 6.
        for i in 0..8 {
            if i == 6 {
                free.store(u64::MAX / 2, Ordering::SeqCst);
            }
            let value = i as f32 + 1.0;
            for track in [Track::Mic, Track::Sys] {
                if writer
                    .push(track, &vec![value / 10.0; block])
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    errors += 1;
                }
            }
        }
        writer.flush().unwrap();
        assert!(errors > 0, "the disk never filled");

        let mic = read_track(&cache, Track::Mic);
        let sys = read_track(&cache, Track::Sys);
        assert_eq!(mic.len(), 8 * block);
        assert_eq!(sys.len(), 8 * block);
        // What was written after the disk freed up sits at its own time.
        for samples in [&mic, &sys] {
            assert!(samples[7 * block..].iter().all(|s| *s == 0.8));
            assert!(samples[..block].iter().all(|s| *s == 0.1));
        }
        // More than one chunk per track: each failure rolled to a new file.
        assert!(chunk_paths(&cache, Track::Mic).unwrap().len() > 1);
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
