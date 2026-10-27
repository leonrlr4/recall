//! Audio decoding through `ffmpeg`.

use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Sample rate of everything [`decode_stereo`] returns.
pub const SAMPLE_RATE: u32 = 16_000;

/// Decodes `path` into two 16 kHz mono channels: `(left, right)`, which in a
/// recording are the microphone and the system audio. A mono file comes back
/// as two identical channels.
///
/// Runs `ffmpeg`, which must be on `PATH`, and holds the whole recording in
/// memory: an hour is 2 x 57.6 M samples, about 460 MB.
pub fn decode_stereo(path: &Path) -> Result<(Vec<f32>, Vec<f32>)> {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "f32le", "-acodec", "pcm_f32le", "-ac", "2", "-ar"])
        .arg(SAMPLE_RATE.to_string())
        .arg("-")
        .output()
        .map_err(|e| match e.kind() {
            ErrorKind::NotFound => anyhow::anyhow!("ffmpeg not found on PATH; install ffmpeg"),
            _ => anyhow::Error::new(e).context("running ffmpeg"),
        })?;
    if !output.status.success() {
        bail!(
            "ffmpeg failed to decode {} ({}): {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    deinterleave(&output.stdout).with_context(|| format!("decoding {}", path.display()))
}

/// Splits interleaved little-endian f32 stereo frames into two channels.
fn deinterleave(bytes: &[u8]) -> Result<(Vec<f32>, Vec<f32>)> {
    let (frames, rest) = bytes.as_chunks::<8>();
    if !rest.is_empty() {
        bail!(
            "ffmpeg output is {} bytes, not whole stereo f32 frames",
            bytes.len()
        );
    }
    let mut left = Vec::with_capacity(frames.len());
    let mut right = Vec::with_capacity(frames.len());
    for [l0, l1, l2, l3, r0, r1, r2, r3] in frames {
        left.push(f32::from_le_bytes([*l0, *l1, *l2, *l3]));
        right.push(f32::from_le_bytes([*r0, *r1, *r2, *r3]));
    }
    Ok((left, right))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg").arg("-version").output().is_ok()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn decode_stereo_splits_channels() {
        if !ffmpeg_available() {
            eprintln!("skipping decode_stereo_splits_channels: ffmpeg not found");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audio.opus");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg("aevalsrc=0.5*sin(440*2*PI*t)|0:s=48000:d=1")
            .args(["-c:a", "libopus"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());

        let (left, right) = decode_stereo(&path).unwrap();
        assert_eq!(left.len(), right.len());
        assert!(
            (15_000..=17_000).contains(&left.len()),
            "{} samples",
            left.len()
        );
        assert!(rms(&left) > 0.1, "left rms {}", rms(&left));
        assert!(rms(&right) < 0.01, "right rms {}", rms(&right));
    }

    #[test]
    fn decode_stereo_reports_missing_file() {
        if !ffmpeg_available() {
            eprintln!("skipping decode_stereo_reports_missing_file: ffmpeg not found");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let err = decode_stereo(&dir.path().join("absent.opus")).unwrap_err();
        assert!(format!("{err:#}").contains("absent.opus"), "{err:#}");
    }
}
