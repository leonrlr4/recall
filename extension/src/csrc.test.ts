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
});
