//! Speech regions: voice activity detection, the echo rule and merging.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector};

use crate::decode::SAMPLE_RATE;
use crate::models::{self, ModelId};

/// A span of audio in milliseconds from the start of the recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub start_ms: u32,
    pub end_ms: u32,
}

impl Region {
    fn len_ms(&self) -> u32 {
        self.end_ms.saturating_sub(self.start_ms)
    }
}

/// Finds speech in 16 kHz mono audio.
pub trait Vad {
    /// Speech regions in `samples16k`, in time order.
    fn regions(&self, samples16k: &[f32]) -> Result<Vec<Region>>;
}

/// Samples per Silero inference window at 16 kHz.
const WINDOW: usize = 512;
/// Seconds of speech the detector can hold before [`SileroVad::regions`]
/// drains it; it is drained after every window, so this only bounds one
/// region, and `max_speech_duration` keeps regions shorter than this.
const BUFFER_SECS: f32 = 60.0;

/// Silero VAD through sherpa-onnx.
pub struct SileroVad {
    // The detector carries state between calls, so each `regions` call takes
    // the lock, resets it and runs the whole input.
    detector: Mutex<VoiceActivityDetector>,
}

impl SileroVad {
    /// Loads `silero_vad.onnx` from the `models` directory.
    pub fn load(models: &Path, threads: usize) -> Result<Self> {
        let model = models::path(models, ModelId::SileroVad);
        if !model.is_file() {
            bail!("Silero VAD model not found at {}", model.display());
        }
        let config = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: Some(model.to_string_lossy().into_owned()),
                threshold: 0.5,
                min_silence_duration: 0.25,
                min_speech_duration: 0.25,
                window_size: WINDOW as i32,
                // sherpa-onnx's own default; longer speech is cut here by
                // raising the threshold until a pause is found.
                max_speech_duration: 20.0,
            },
            sample_rate: SAMPLE_RATE as i32,
            num_threads: i32::try_from(threads.max(1)).context("thread count")?,
            ..Default::default()
        };
        let detector = VoiceActivityDetector::create(&config, BUFFER_SECS)
            .ok_or_else(|| anyhow!("loading Silero VAD from {}", model.display()))?;
        Ok(Self {
            detector: Mutex::new(detector),
        })
    }
}

impl Vad for SileroVad {
    fn regions(&self, samples16k: &[f32]) -> Result<Vec<Region>> {
        let detector = self
            .detector
            .lock()
            .map_err(|_| anyhow!("Silero VAD lock poisoned"))?;
        detector.reset();
        detector.clear();
        let mut out = Vec::new();
        let drain = |out: &mut Vec<Region>| {
            while let Some(segment) = detector.front() {
                out.push(Region {
                    start_ms: samples_to_ms(segment.start()),
                    end_ms: samples_to_ms(segment.start() + i64::from(segment.n())),
                });
                detector.pop();
            }
        };
        for chunk in samples16k.chunks(WINDOW) {
            detector.accept_waveform(chunk);
            drain(&mut out);
        }
        detector.flush();
        drain(&mut out);
        Ok(out)
    }
}

fn samples_to_ms(samples: i64) -> u32 {
    let ms = samples.max(0) * 1_000 / i64::from(SAMPLE_RATE);
    u32::try_from(ms).unwrap_or(u32::MAX)
}

/// Removes microphone regions that are the remote side leaking back in
/// through the speakers.
///
/// A mic region is dropped when at least 80 % of it overlaps the union of the
/// `sys` regions, each widened by `tolerance_ms` (150 in the pipeline) on both
/// sides to absorb playback latency. Everything else is kept unchanged.
/// `sys` may be unsorted and overlapping.
pub fn drop_echo(mic: &[Region], sys: &[Region], tolerance_ms: u32) -> Vec<Region> {
    let widened: Vec<Region> = sys
        .iter()
        .map(|r| Region {
            start_ms: r.start_ms.saturating_sub(tolerance_ms),
            end_ms: r.end_ms.saturating_add(tolerance_ms),
        })
        .collect();
    let covered = union(&widened);
    mic.iter()
        .filter(|m| {
            let overlap: u64 = covered
                .iter()
                .map(|c| {
                    let start = m.start_ms.max(c.start_ms);
                    let end = m.end_ms.min(c.end_ms);
                    u64::from(end.saturating_sub(start))
                })
                .sum();
            // overlap / len < 0.8, in integers.
            overlap * 5 < u64::from(m.len_ms()) * 4
        })
        .copied()
        .collect()
}

/// Sorted, non-overlapping cover of `regions`.
fn union(regions: &[Region]) -> Vec<Region> {
    let mut sorted = regions.to_vec();
    sorted.sort_by_key(|r| (r.start_ms, r.end_ms));
    let mut out: Vec<Region> = Vec::with_capacity(sorted.len());
    for r in sorted {
        match out.last_mut() {
            Some(last) if r.start_ms <= last.end_ms => last.end_ms = last.end_ms.max(r.end_ms),
            _ => out.push(r),
        }
    }
    out
}

/// Joins regions separated by less than `gap_ms` (300 in the pipeline), as
/// long as the joined region stays within `max_len_ms` (30 000, the longest
/// chunk the transcriber takes), then cuts any region still longer than
/// `max_len_ms` into consecutive `max_len_ms` pieces. Overlapping regions are
/// always joined. Input may be unsorted; output is sorted and non-overlapping.
pub fn merge_close(regions: &[Region], gap_ms: u32, max_len_ms: u32) -> Vec<Region> {
    let mut sorted = regions.to_vec();
    sorted.sort_by_key(|r| (r.start_ms, r.end_ms));
    let mut joined: Vec<Region> = Vec::with_capacity(sorted.len());
    for r in sorted {
        match joined.last_mut() {
            Some(last)
                if r.start_ms < last.end_ms
                    || (r.start_ms - last.end_ms < gap_ms
                        && r.end_ms.saturating_sub(last.start_ms) <= max_len_ms) =>
            {
                last.end_ms = last.end_ms.max(r.end_ms);
            }
            _ => joined.push(r),
        }
    }
    if max_len_ms == 0 {
        return joined;
    }
    let mut out = Vec::with_capacity(joined.len());
    for r in joined {
        let mut start = r.start_ms;
        while r.end_ms.saturating_sub(start) > max_len_ms {
            out.push(Region {
                start_ms: start,
                end_ms: start + max_len_ms,
            });
            start += max_len_ms;
        }
        out.push(Region {
            start_ms: start,
            end_ms: r.end_ms,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn r(start_ms: u32, end_ms: u32) -> Region {
        Region { start_ms, end_ms }
    }

    #[test]
    fn drop_echo_removes_region_fully_inside_sys() {
        let sys = [r(1_000, 5_000)];
        // Inside sys, and inside only once the 150 ms tolerance is applied.
        let mic = [r(2_000, 3_000), r(900, 5_100)];
        assert_eq!(drop_echo(&mic, &sys, 150), vec![]);
    }

    #[test]
    fn drop_echo_keeps_region_mostly_alone() {
        let sys = [r(1_000, 2_000)];
        // Sys widened by the tolerance is 850..2150: 1150 of 4000 ms overlap.
        let alone = r(1_000, 5_000);
        // 1300 of 1700 ms overlap: 76 %, just under the 80 % rule.
        let partial = r(600, 2_300);
        // Each widened sys region alone covers at most 55 %, their union 95 %.
        let covered = r(10_000, 11_000);
        let sys_split = [r(9_000, 10_400), r(10_650, 10_800)];
        let all_sys: Vec<Region> = sys.iter().chain(&sys_split).copied().collect();
        assert_eq!(
            drop_echo(&[alone, partial, covered], &all_sys, 150),
            vec![alone, partial]
        );
        assert_eq!(drop_echo(&[alone], &[], 150), vec![alone]);
    }

    #[test]
    fn merge_close_joins_gaps_under_300ms_and_splits_at_30s() {
        // 299 ms gap joins, 300 ms gap does not.
        assert_eq!(
            merge_close(
                &[r(0, 1_000), r(1_299, 2_000), r(2_300, 3_000)],
                300,
                30_000
            ),
            vec![r(0, 2_000), r(2_300, 3_000)]
        );
        // Unsorted and overlapping input is handled.
        assert_eq!(
            merge_close(&[r(500, 1_500), r(0, 1_000)], 300, 30_000),
            vec![r(0, 1_500)]
        );
        // A join that would pass 30 s is not made.
        assert_eq!(
            merge_close(&[r(0, 20_000), r(20_100, 40_000)], 300, 30_000),
            vec![r(0, 20_000), r(20_100, 40_000)]
        );
        // Overlapping regions always join, then the result is cut.
        assert_eq!(
            merge_close(&[r(0, 20_000), r(10_000, 40_000)], 300, 30_000),
            vec![r(0, 30_000), r(30_000, 40_000)]
        );
        // A single long region is cut into 30 s pieces.
        assert_eq!(
            merge_close(&[r(1_000, 71_000)], 300, 30_000),
            vec![r(1_000, 31_000), r(31_000, 61_000), r(61_000, 71_000)]
        );
    }

    /// `models/silero_vad.onnx` under `$RECALL_HOME` or `~/.local/share/recall`.
    fn installed_models_dir() -> Option<PathBuf> {
        let candidates = [
            std::env::var_os("RECALL_HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join("models")),
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/recall/models")),
        ];
        candidates
            .into_iter()
            .flatten()
            .find(|dir| dir.join("silero_vad.onnx").is_file())
    }

    #[test]
    fn silero_finds_speech_in_fixture() {
        let Some(models) = installed_models_dir() else {
            eprintln!(
                "skipping silero_finds_speech_in_fixture: silero_vad.onnx not found under \
                 $RECALL_HOME/models or ~/.local/share/recall/models"
            );
            return;
        };
        // 1 s silence, 6.6 s of LibriSpeech 1089-134686-0000 (CC BY 4.0), 1 s silence.
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/speech.opus");
        let (samples, _) = crate::decode_stereo(&fixture).unwrap();

        let vad = SileroVad::load(&models, 1).unwrap();
        let regions = vad.regions(&samples).unwrap();

        assert!(!regions.is_empty());
        let first = regions.first().unwrap();
        let last = regions.last().unwrap();
        assert!(
            (700..=1_500).contains(&first.start_ms),
            "first region {first:?}"
        );
        assert!(
            (6_500..=8_000).contains(&last.end_ms),
            "last region {last:?}"
        );
        let speech_ms: u32 = regions.iter().map(|r| r.end_ms - r.start_ms).sum();
        assert!(
            speech_ms >= 4_000,
            "{speech_ms} ms of speech in {regions:?}"
        );
        // A second call starts from a reset detector, not where the first ended.
        assert_eq!(vad.regions(&samples).unwrap(), regions);
    }
}
