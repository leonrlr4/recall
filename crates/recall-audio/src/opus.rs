//! Ogg Opus file writer (RFC 7845).

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ::opus::{Application, Bitrate, Channels, Encoder};
use anyhow::{Context, Result, bail};
use ogg::{PacketWriteEndInfo, PacketWriter};

/// Samples at 48 kHz the decoder discards from the start of the stream. 312
/// is libopus's encoder lookahead at 48 kHz, the value `opusenc` writes too.
const PRE_SKIP: u16 = 312;

/// Large enough for any single Opus packet (RFC 6716 caps a frame at 1275
/// bytes; a packet carries at most three of those here).
const MAX_PACKET: usize = 4000;

/// Encodes interleaved f32 PCM into an Ogg Opus file in 20 ms packets.
pub struct OggOpusWriter {
    packets: PacketWriter<'static, BufWriter<File>>,
    encoder: Encoder,
    serial: u32,
    channels: usize,
    /// Samples per channel in one 20 ms frame at `sample_rate`.
    frame_len: usize,
    /// Granule positions count 48 kHz samples whatever the input rate.
    granule_scale: u64,
    /// Interleaved samples not yet making up a whole frame.
    buffer: Vec<f32>,
    /// Samples per channel handed to `write_interleaved`, padding excluded.
    samples_written: u64,
    /// Frames passed to the encoder, padding included.
    frames_encoded: u64,
    /// The last encoded packet, held back so `finish` can flag it as the end
    /// of the stream.
    pending: Option<(Vec<u8>, u64)>,
}

impl OggOpusWriter {
    pub fn create(path: &Path, sample_rate: u32, channels: u8, bitrate: i32) -> Result<Self> {
        let opus_channels = match channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            n => bail!("Opus mapping family 0 supports 1 or 2 channels, got {n}"),
        };
        if !matches!(sample_rate, 8_000 | 12_000 | 16_000 | 24_000 | 48_000) {
            bail!("Opus does not support a {sample_rate} Hz input rate");
        }
        let mut encoder = Encoder::new(sample_rate, opus_channels, Application::Voip)
            .context("creating the Opus encoder")?;
        encoder
            .set_bitrate(Bitrate::Bits(bitrate))
            .context("setting the Opus bitrate")?;

        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let mut packets = PacketWriter::new(BufWriter::new(file));
        // Any serial works for a single-stream file; vary it so concatenated
        // files stay distinguishable.
        let serial = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);

        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(channels);
        head.extend_from_slice(&PRE_SKIP.to_le_bytes());
        head.extend_from_slice(&sample_rate.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes()); // output gain
        head.push(0); // channel mapping family
        packets
            .write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)
            .context("writing OpusHead")?;

        let vendor = b"recall";
        let mut tags = Vec::new();
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes()); // no user comments
        packets
            .write_packet(tags, serial, PacketWriteEndInfo::EndPage, 0)
            .context("writing OpusTags")?;

        Ok(Self {
            packets,
            encoder,
            serial,
            channels: channels as usize,
            frame_len: sample_rate as usize / 50,
            granule_scale: u64::from(48_000 / sample_rate),
            buffer: Vec::new(),
            samples_written: 0,
            frames_encoded: 0,
            pending: None,
        })
    }

    /// Buffers `pcm` (interleaved, one sample per channel per frame) and
    /// encodes every whole 20 ms frame it completes.
    pub fn write_interleaved(&mut self, pcm: &[f32]) -> Result<()> {
        if !pcm.len().is_multiple_of(self.channels) {
            bail!(
                "{} samples do not divide into {} channels",
                pcm.len(),
                self.channels
            );
        }
        self.samples_written += (pcm.len() / self.channels) as u64;
        self.buffer.extend_from_slice(pcm);
        let frame = self.frame_len * self.channels;
        let whole = self.buffer.len() / frame * frame;
        let buffer = std::mem::take(&mut self.buffer);
        for chunk in buffer[..whole].chunks_exact(frame) {
            self.encode(chunk)?;
        }
        self.buffer = buffer[whole..].to_vec();
        Ok(())
    }

    /// Pads the input with silence until the encoder has emitted every real
    /// sample past its lookahead, marks the last packet as the end of the
    /// stream and flushes the file.
    pub fn finish(mut self) -> Result<()> {
        let frame = self.frame_len * self.channels;
        // The decoder drops the first PRE_SKIP samples, so the encoder must see
        // that much input beyond the real samples, rounded up to whole frames.
        let pre_skip = u64::from(PRE_SKIP).div_ceil(self.granule_scale);
        let needed = (self.samples_written + pre_skip).div_ceil(self.frame_len as u64);
        let mut tail = std::mem::take(&mut self.buffer);
        tail.resize((needed - self.frames_encoded) as usize * frame, 0.0);
        for chunk in tail.chunks_exact(frame) {
            self.encode(chunk)?;
        }
        let (packet, _) = self.pending.take().expect("encode leaves a pending packet");
        // The final granule counts only real samples, so decoders trim the
        // padding (RFC 7845 section 4.5).
        let granule = self.samples_written * self.granule_scale + u64::from(PRE_SKIP);
        self.packets
            .write_packet(packet, self.serial, PacketWriteEndInfo::EndStream, granule)
            .context("writing the last Opus packet")?;
        let mut inner = self.packets.into_inner();
        std::io::Write::flush(&mut inner).context("flushing the Opus file")?;
        inner
            .into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()
            .context("syncing the Opus file")?;
        Ok(())
    }

    fn encode(&mut self, frame: &[f32]) -> Result<()> {
        let mut out = vec![0u8; MAX_PACKET];
        let len = self
            .encoder
            .encode_float(frame, &mut out)
            .context("encoding an Opus frame")?;
        out.truncate(len);
        self.frames_encoded += 1;
        // A page's granule position counts every sample decoded through its
        // last packet, the pre-skip included (RFC 7845 section 4).
        let end = self.frames_encoded * self.frame_len as u64 * self.granule_scale;
        if let Some((packet, granule)) = self.pending.replace((out, end)) {
            self.packets
                .write_packet(
                    packet,
                    self.serial,
                    PacketWriteEndInfo::NormalPacket,
                    granule,
                )
                .context("writing an Opus packet")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn opus_file_decodes_with_ffprobe_as_stereo_48k() {
        if Command::new("ffprobe").arg("-version").output().is_err() {
            eprintln!("skipping: ffprobe is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.opus");
        let mut writer = OggOpusWriter::create(&path, 48_000, 2, 64_000).unwrap();
        // One second of a 440 Hz tone on the left and 660 Hz on the right,
        // written in uneven slices so frames straddle calls.
        let pcm: Vec<f32> = (0..48_000)
            .flat_map(|i| {
                let t = i as f32 / 48_000.0;
                [
                    0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin(),
                    0.3 * (2.0 * std::f32::consts::PI * 660.0 * t).sin(),
                ]
            })
            .collect();
        for slice in pcm.chunks(1234) {
            writer.write_interleaved(slice).unwrap();
        }
        writer.finish().unwrap();

        let output = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=channels,sample_rate,codec_name",
                "-of",
                "default=nw=1",
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("codec_name=opus"), "{stdout}");
        assert!(stdout.contains("channels=2"), "{stdout}");
        assert!(stdout.contains("sample_rate=48000"), "{stdout}");
    }

    /// Decoders must get back exactly the samples written: the pre-skip
    /// trimmed at the start, the padding trimmed at the end.
    #[test]
    fn opus_file_decodes_to_the_samples_written() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping: ffmpeg is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        for frames in [0usize, 960 * 50, 48_123] {
            let path = dir.path().join(format!("{frames}.opus"));
            let mut writer = OggOpusWriter::create(&path, 48_000, 2, 64_000).unwrap();
            writer.write_interleaved(&vec![0.1; frames * 2]).unwrap();
            writer.finish().unwrap();

            let output = Command::new("ffmpeg")
                .args(["-nostdin", "-v", "error", "-i"])
                .arg(&path)
                .args(["-f", "s16le", "-"])
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert_eq!(output.stdout.len(), frames * 2 * 2, "{frames} frames");
        }
    }
}
