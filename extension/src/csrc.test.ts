import { describe, expect, it } from 'vitest';
import { SourceLevels, SpeakingTracker } from './csrc';

describe('SpeakingTracker', () => {
  it('bridges dips shorter than the hangover', () => {
    const tracker = new SpeakingTracker();
    expect(tracker.update(0, new Map([['csrc:1', 0.3]]))).toEqual([
      { t: 0, kind: 'speaking_start', participant: 'csrc:1' },
    ]);
    expect(tracker.update(100, new Map([['csrc:1', 0.01]]))).toEqual([]);
    expect(tracker.update(400, new Map())).toEqual([]);
    expect(tracker.update(500, new Map([['csrc:1', 0.3]]))).toEqual([]);
    expect(tracker.update(1000, new Map())).toEqual([
      { t: 500, kind: 'speaking_stop', participant: 'csrc:1' },
    ]);
  });
});

describe('SourceLevels', () => {
  it('reads a source as silent when its timestamp stops advancing', () => {
    const levels = new SourceLevels();
    expect(levels.read([{ source: 7, audioLevel: 0.5, timestamp: 10 }])).toEqual(new Map([[7, 0.5]]));
    expect(levels.read([{ source: 7, audioLevel: 0.5, timestamp: 10 }])).toEqual(new Map([[7, 0]]));
    expect(levels.read([{ source: 7, audioLevel: 0.4, timestamp: 30 }])).toEqual(new Map([[7, 0.4]]));
  });

  it('reads a frozen entry on a second receiver as silent', () => {
    // Meet moved the participant to another receiver; the old one keeps its
    // last entry, loud and frozen, for up to 10 s.
    const levels = new SourceLevels();
    const frozen = { source: 7, audioLevel: 0.5, timestamp: 100 };
    expect(levels.read([frozen])).toEqual(new Map([[7, 0.5]]));
    expect(levels.read([frozen, { source: 7, audioLevel: 0.01, timestamp: 120 }])).toEqual(new Map([[7, 0.01]]));
    expect(levels.read([{ source: 7, audioLevel: 0.01, timestamp: 140 }, frozen])).toEqual(new Map([[7, 0.01]]));
    // The frozen entry alone, after the new receiver went quiet, is still stale.
    expect(levels.read([frozen])).toEqual(new Map([[7, 0]]));

    // On first sight, only the newer of two entries counts.
    const first = new SourceLevels();
    expect(first.read([frozen, { source: 7, audioLevel: 0.01, timestamp: 120 }])).toEqual(new Map([[7, 0.01]]));
  });
});
