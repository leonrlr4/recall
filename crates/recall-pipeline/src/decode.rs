//! Audio decoding through `ffmpeg`.

use std::io::{ErrorKind, Read};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Sample rate of everything [`decode_stereo`] returns.
pub const SAMPLE_RATE: u32 = 16_000;
/// Bytes of ffmpeg output read at a time.
const READ_CHUNK: usize = 64 * 1024;

/// Decodes `path` into two 16 kHz mono channels: `(left, right)`, which in a
/// recording are the microphone and the system audio. A mono file comes back
/// as two identical channels.
///
/// Runs `ffmpeg`, which must be on `PATH`, and splits its output into the
/// two channels as it arrives, so memory holds the decoded channels and one
/// 64 KiB read buffer. An hour is 57.6 M samples per channel, 2 x 230 MB =
/// 461 MB of `f32` (more for a moment while a vector grows).
pub fn decode_stereo(path: &Path) -> Result<(Vec<f32>, Vec<f32>)> {
    let mut child = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "f32le", "-acodec", "pcm_f32le", "-ac", "2", "-ar"])
        .arg(SAMPLE_RATE.to_string())
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            ErrorKind::NotFound => anyhow::anyhow!("ffmpeg not found on PATH; install ffmpeg"),
            _ => anyhow::Error::new(e).context("running ffmpeg"),
        })?;
    // Drained on its own thread so a chatty ffmpeg never blocks on a full
    // stderr pipe while this thread waits on stdout.
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut split = Deinterleaver::default();
    let mut buf = vec![0u8; READ_CHUNK];
    let read = loop {
        match stdout.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => split.push(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => break Err(e),
        }
    };
    drop(stdout);
    let status = child.wait().context("waiting for ffmpeg")?;
    let stderr = errors.join().unwrap_or_default();
    if !status.success() {
        bail!(
            "ffmpeg failed to decode {} ({status}): {}",
            path.display(),
            stderr.trim()
        );
    }
    read.with_context(|| format!("reading ffmpeg output for {}", path.display()))?;
    split
        .finish()
        .with_context(|| format!("decoding {}", path.display()))
}

/// Splits interleaved little-endian f32 stereo frames into two channels,
/// fed in pieces that need not end on a frame boundary.
#[derive(Default)]
struct Deinterleaver {
    left: Vec<f32>,
    right: Vec<f32>,
    /// The start of a frame cut off by the end of the last piece.
    partial: Vec<u8>,
}

impl Deinterleaver {
    fn push(&mut self, mut bytes: &[u8]) {
        if !self.partial.is_empty() {
            let take = (8 - self.partial.len()).min(bytes.len());
            self.partial.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.partial.len() < 8 {
                return;
            }
            let frame: [u8; 8] = self.partial[..].try_into().expect("8 bytes");
            self.frame(&frame);
            self.partial.clear();
        }
        let (frames, rest) = bytes.as_chunks::<8>();
        for frame in frames {
            self.frame(frame);
        }
        self.partial.extend_from_slice(rest);
    }

    fn frame(&mut self, [l0, l1, l2, l3, r0, r1, r2, r3]: &[u8; 8]) {
        self.left.push(f32::from_le_bytes([*l0, *l1, *l2, *l3]));
        self.right.push(f32::from_le_bytes([*r0, *r1, *r2, *r3]));
    }

    fn finish(self) -> Result<(Vec<f32>, Vec<f32>)> {
        if !self.partial.is_empty() {
            bail!(
                "ffmpeg output ends {} bytes into a stereo f32 frame",
                self.partial.len()
            );
        }
        Ok((self.left, self.right))
    }
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
    fn deinterleaver_handles_pieces_cut_mid_frame() {
        let frames: Vec<(f32, f32)> = (0..100).map(|i| (i as f32, -(i as f32))).collect();
        let bytes: Vec<u8> = frames
            .iter()
            .flat_map(|(l, r)| l.to_le_bytes().into_iter().chain(r.to_le_bytes()))
            .collect();
        for piece in [1, 3, 7, 8, 13, 800] {
            let mut split = Deinterleaver::default();
            for chunk in bytes.chunks(piece) {
                split.push(chunk);
            }
            let (left, right) = split.finish().unwrap();
            assert_eq!(
                left,
                frames.iter().map(|f| f.0).collect::<Vec<_>>(),
                "{piece}"
            );
            assert_eq!(
                right,
                frames.iter().map(|f| f.1).collect::<Vec<_>>(),
                "{piece}"
            );
        }
        let mut split = Deinterleaver::default();
        split.push(&bytes[..12]);
        assert!(split.finish().is_err());
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
