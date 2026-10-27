import { describe, expect, it } from 'vitest';
import { peaksFrom } from './player';

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
