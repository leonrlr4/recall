// Waveform peaks and drawing for the meeting player. The recording is
// stereo: left is the microphone (me), right is the remote side (them).

export interface Peaks {
  me: Float32Array;
  them: Float32Array;
}

export const BINS = 600;

/**
 * Sample rate the audio is decoded at. Decoding resamples to the context
 * rate, and an hour of stereo at 48 kHz is ~1.4 GB of floats; at 8 kHz it is
 * ~230 MB, while the amplitude envelope at 600 bins looks the same. Do not go
 * lower: Firefox rejects OfflineAudioContext rates below 8000.
 */
const DECODE_RATE = 8000;

/** Max absolute sample per bin, for each channel. */
export function peaksFrom(channels: Float32Array[], bins = BINS): Peaks {
  const bin = (data: Float32Array | undefined): Float32Array => {
    const out = new Float32Array(bins);
    if (!data || data.length === 0) return out;
    const size = data.length / bins;
    for (let b = 0; b < bins; b++) {
      const start = Math.floor(b * size);
      const end = Math.max(start + 1, Math.floor((b + 1) * size));
      let max = 0;
      for (let i = start; i < end && i < data.length; i++) {
        const v = Math.abs(data[i]);
        if (v > max) max = v;
      }
      out[b] = max;
    }
    return out;
  };
  return { me: bin(channels[0]), them: bin(channels[1]) };
}

/** Fetches and decodes the audio at `url`, then bins it. Throws on decode errors. */
export async function loadPeaks(url: string, bins = BINS, signal?: AbortSignal): Promise<Peaks> {
  const res = await fetch(url, { signal });
  if (!res.ok) throw new Error(`audio ${res.status}`);
  const bytes = await res.arrayBuffer();
  const ctx = new OfflineAudioContext(2, 1, DECODE_RATE);
  const buf = await ctx.decodeAudioData(bytes);
  const channels = Array.from({ length: buf.numberOfChannels }, (_, i) => buf.getChannelData(i));
  return peaksFrom(channels, bins);
}

export interface DrawOptions {
  durationMs: number;
  currentMs: number;
  chapters: number[];
  colors: { me: string; them: string; playhead: string; tick: string };
}

/** Draws me above the midline and them below; the played part is opaque. */
export function draw(canvas: HTMLCanvasElement, peaks: Peaks, o: DrawOptions): void {
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth;
  const h = canvas.clientHeight;
  if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
  }
  const ctx = canvas.getContext('2d');
  if (!ctx) return;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);

  const n = peaks.me.length;
  let max = 0;
  for (let i = 0; i < n; i++) max = Math.max(max, peaks.me[i], peaks.them[i]);
  const scale = max > 0 ? 1 / max : 0;

  const tickZone = 8;
  const mid = tickZone + (h - tickZone) / 2;
  const half = (h - tickZone) / 2 - 2;
  const step = w / n;
  const barW = Math.max(1, step * 0.7);
  const playedX = o.durationMs > 0 ? (o.currentMs / o.durationMs) * w : 0;

  for (let i = 0; i < n; i++) {
    const x = i * step;
    const played = x + barW / 2 <= playedX;
    const up = Math.max(1, Math.sqrt(peaks.me[i] * scale) * half);
    const down = Math.max(1, Math.sqrt(peaks.them[i] * scale) * half);
    ctx.globalAlpha = played ? 1 : 0.35;
    ctx.fillStyle = o.colors.me;
    ctx.fillRect(x, mid - up, barW, up);
    ctx.fillStyle = o.colors.them;
    ctx.fillRect(x, mid, barW, down);
  }
  ctx.globalAlpha = 1;

  ctx.fillStyle = o.colors.tick;
  for (const ms of o.chapters) {
    if (o.durationMs <= 0) break;
    const x = Math.round((ms / o.durationMs) * w);
    ctx.fillRect(x - 1, 0, 2, tickZone + 2);
  }

  if (o.durationMs > 0) {
    ctx.fillStyle = o.colors.playhead;
    ctx.fillRect(Math.round(playedX) - 1, tickZone, 2, h - tickZone);
  }
}

// Screen video sync. `offsetMs` is `screen_offset_ms`: how long after the
// audio origin the first screen frame was captured (negative when the screen
// started first). The audio element is the clock; the video follows it.

/** How far the video may drift from the audio before it is moved. */
export const SYNC_TOLERANCE_MS = 300;

/** Video position in seconds for an audio position; 0 before the screen started. */
export function videoSeconds(audioMs: number, offsetMs: number): number {
  return Math.max(0, (audioMs - offsetMs) / 1000);
}

/** Audio position in milliseconds for a video position; 0 for video
 *  recorded before the audio started. */
export function audioMsAt(videoSec: number, offsetMs: number): number {
  return Math.max(0, videoSec * 1000 + offsetMs);
}

/**
 * Where to move the video so it matches the audio, or null when it is within
 * [`SYNC_TOLERANCE_MS`]. Past the video's end (`durationSec`, NaN while
 * unknown) the target is the last frame.
 */
export function videoCorrection(
  videoSec: number,
  audioMs: number,
  offsetMs: number,
  durationSec: number,
): number | null {
  let target = videoSeconds(audioMs, offsetMs);
  if (Number.isFinite(durationSec)) target = Math.min(target, durationSec);
  return Math.abs(videoSec - target) * 1000 > SYNC_TOLERANCE_MS ? target : null;
}

/**
 * Playback rate that closes a drift smaller than [`SYNC_TOLERANCE_MS`]
 * without a seek: a seek while playing costs a decode, which leaves the video
 * behind by about as much as it fixed. Within 50 ms the rate is 1.
 */
export function catchUpRate(videoSec: number, audioMs: number, offsetMs: number): number {
  const drift = videoSeconds(audioMs, offsetMs) - videoSec;
  if (Math.abs(drift) < 0.05) return 1;
  return 1 + Math.max(-0.25, Math.min(0.25, drift));
}
