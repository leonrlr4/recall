import { describe, expect, it } from 'vitest';
import { audioMsAt, catchUpRate, peaksFrom, videoCorrection, videoSeconds } from './player';

describe('player', () => {
  it('peaks_take_max_abs_per_bin_per_channel', () => {
    const left = new Float32Array([0.1, -0.5, 0.2, 0.3, 0, -0.9]);
    const right = new Float32Array([0, 0, 0.4, 0, 0.1, 0]);
    const p = peaksFrom([left, right], 3);
    expect(Array.from(p.me)).toEqual([0.5, 0.3, 0.9].map(Math.fround));
    expect(Array.from(p.them)).toEqual([0, 0.4, 0.1].map(Math.fround));
  });

  it('mono_audio_leaves_them_silent', () => {
    const p = peaksFrom([new Float32Array([1, 1])], 4);
    expect(p.me.length).toBe(4);
    expect(Array.from(p.them)).toEqual([0, 0, 0, 0]);
  });
});

describe('screen video sync', () => {
  it('player_maps_audio_time_to_video_time_with_offset', () => {
    // The screen started 2 s after the audio.
    expect(videoSeconds(5_000, 2_000)).toBe(3);
    expect(videoSeconds(1_000, 2_000)).toBe(0);
    // The screen started before the audio.
    expect(videoSeconds(5_000, -500)).toBe(5.5);
    expect(videoSeconds(5_000, 0)).toBe(5);
    expect(audioMsAt(3, 2_000)).toBe(5_000);
    expect(audioMsAt(0.2, -500)).toBe(0);

    // Within 300 ms nothing moves; beyond it the video jumps to the audio.
    expect(videoCorrection(3.25, 5_000, 2_000, 60)).toBeNull();
    expect(videoCorrection(3.4, 5_000, 2_000, 60)).toBe(3);
    expect(videoCorrection(0, 1_000, 2_000, 60)).toBeNull();
    // Past the end of the video it stays on its last frame.
    expect(videoCorrection(60, 90_000, 2_000, 60)).toBeNull();
    expect(videoCorrection(10, 90_000, 2_000, 60)).toBe(60);
    expect(videoCorrection(10, 90_000, 2_000, NaN)).toBe(88);

    // A small drift is closed by playing faster or slower, not by seeking.
    expect(catchUpRate(3.02, 5_000, 2_000)).toBe(1);
    expect(catchUpRate(2.8, 5_000, 2_000)).toBeCloseTo(1.2);
    expect(catchUpRate(3.1, 5_000, 2_000)).toBeCloseTo(0.9);
  });
});
