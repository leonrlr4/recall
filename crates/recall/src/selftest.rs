//! The audio self-test: a short recording of both tracks, reduced to peak
//! levels, to check that the mic and the system audio are both captured.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use recall_audio::capture::{Capture, Levels};
use recall_audio::writer::{ChunkWriter, finalize};
use recall_pipeline::decode_stereo;
use recall_server::SelfTestResult;

/// Length of the test recording.
pub const DURATION: Duration = Duration::from_secs(5);
/// Peak values per track: one per 100 ms.
pub const WINDOWS: usize = 50;

/// Records [`DURATION`] from the default source and the default sink's
/// monitor into a temporary cache, encodes it as a recording would be, and
/// returns [`WINDOWS`] peaks per track from the decoded audio. Blocks for
/// the whole recording.
pub fn run() -> Result<SelfTestResult> {
    let tmp = tempfile::tempdir().context("creating a temporary directory")?;
    let cache = tmp.path().join("cache");
    let writer = ChunkWriter::new(&cache)?;
    let capture = Capture::start(Arc::new(Mutex::new(writer)), Arc::new(Levels::default()))
        .context("starting the capture")?;
    std::thread::sleep(DURATION);
    capture.stop().context("capturing")?;
    let audio = tmp.path().join("selftest.opus");
    finalize(&cache, &audio)?;
    let (mic, sys) = decode_stereo(&audio)?;
    Ok(SelfTestResult {
        mic_peaks: peaks(&mic, WINDOWS),
        sys_peaks: peaks(&sys, WINDOWS),
    })
}

/// The peak absolute sample, clamped to 0..=1, of each of `n` equal windows
/// over `samples`; windows past the end are 0.
pub fn peaks(samples: &[f32], n: usize) -> Vec<f32> {
    let window = samples.len().div_ceil(n).max(1);
    let mut out: Vec<f32> = samples
        .chunks(window)
        .map(|w| w.iter().fold(0.0f32, |m, s| m.max(s.abs())).min(1.0))
        .collect();
    out.resize(n, 0.0);
    out
}

/// One character per peak, from a space (silence) to a full block.
pub fn bar(peaks: &[f32]) -> String {
    const LEVELS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    peaks
        .iter()
        .map(|p| {
            let i = (p.clamp(0.0, 1.0) * 8.0).round() as usize;
            LEVELS[i]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_cover_equal_windows() {
        let mut samples = vec![0.0f32; 100];
        samples[5] = -0.5;
        samples[99] = 2.0;
        assert_eq!(peaks(&samples, 4), [0.5, 0.0, 0.0, 1.0]);
        assert_eq!(peaks(&[], 3), [0.0, 0.0, 0.0]);
        assert_eq!(peaks(&[0.25], 2), [0.25, 0.0]);
    }

    #[test]
    fn bar_has_one_char_per_peak() {
        assert_eq!(bar(&[0.0, 0.5, 1.0, 3.0]), " ▄██");
        assert_eq!(bar(&[0.0; WINDOWS]).chars().count(), WINDOWS);
    }
}
