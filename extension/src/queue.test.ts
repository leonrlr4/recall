import { describe, expect, it } from 'vitest';
import { EventQueue, meetingCode, sanitize } from './queue';

describe('EventQueue', () => {
  it('queue resends only the last 60 s', () => {
    const queue = new EventQueue();
    queue.push({ t: 1_000, kind: 'speaking_start', participant: 'csrc:1' });
    queue.push({ t: 30_000, kind: 'speaking_stop', participant: 'csrc:1' });
    queue.push({ t: 61_000, kind: 'roster', names: ['Tom Lin'] });

    expect(queue.window(61_000).map((e) => e.t)).toEqual([1_000, 30_000, 61_000]);
    // Every batch resends the whole window, so a failed POST loses nothing younger than 60 s.
    expect(queue.window(61_000).map((e) => e.t)).toEqual([1_000, 30_000, 61_000]);
    expect(queue.window(61_001).map((e) => e.t)).toEqual([30_000, 61_000]);
    expect(queue.window(121_001)).toEqual([]);
  });
});

describe('meetingCode', () => {
  it('reads the code from the URL path', () => {
    expect(meetingCode('/abc-defg-hij')).toBe('abc-defg-hij');
    expect(meetingCode('/abc-defg-hij/')).toBe('abc-defg-hij');
    expect(meetingCode('/')).toBeNull();
    expect(meetingCode('/landing')).toBeNull();
  });
});

describe('sanitize', () => {
  it('keeps only the fields the daemon knows', () => {
    expect(
      sanitize({ source: 'recall', t: 5, kind: 'speaking_start', participant: 'csrc:1', extra: 1 }),
    ).toEqual({ t: 5, kind: 'speaking_start', participant: 'csrc:1' });
    expect(sanitize({ t: 5, kind: 'roster', names: ['A', 3] })).toBeNull();
    expect(sanitize({ t: 5, kind: 'audio' })).toBeNull();
    expect(sanitize({ kind: 'status', status: 'ok' })).toBeNull();
  });
});
