/** A participant counts as speaking while its audio level is at least this. */
export const SPEAKING_LEVEL = 0.02;

/** A speaker stops only after this long below `SPEAKING_LEVEL`. */
export const HANGOVER_MS = 500;

export interface SpeakingEvent {
  t: number;
  kind: 'speaking_start' | 'speaking_stop';
  participant: string;
}

/** Turns sampled audio levels per participant into start/stop events. */
export class SpeakingTracker {
  /** Speaking participants and the last time each was loud. */
  private loudAt = new Map<string, number>();

  update(t: number, levels: Map<string, number>): SpeakingEvent[] {
    const events: SpeakingEvent[] = [];
    for (const [participant, level] of levels) {
      if (level < SPEAKING_LEVEL) continue;
      if (!this.loudAt.has(participant)) events.push({ t, kind: 'speaking_start', participant });
      this.loudAt.set(participant, t);
    }
    for (const [participant, last] of this.loudAt) {
      if (t - last < HANGOVER_MS) continue;
      // The speech ended at the last loud sample, not when the hangover ran out.
      events.push({ t: last, kind: 'speaking_stop', participant });
      this.loudAt.delete(participant);
    }
    return events;
  }

  get speakingCount(): number {
    return this.loudAt.size;
  }
}

/** The fields of `RTCRtpContributingSource` / `RTCRtpSynchronizationSource` used here. */
export interface RtpSource {
  source: number;
  audioLevel?: number;
  timestamp?: number;
}

/**
 * Reads audio levels from RTP sources. A source keeps reporting the level of
 * its last packet after packets stop, so a source whose `timestamp` has not
 * advanced since the previous read counts as silent. The comparison is with
 * the source's own previous value, so the timestamp's clock does not matter.
 */
export class SourceLevels {
  private seen = new Map<number, number>();

  read(sources: Iterable<RtpSource>): Map<number, number> {
    const levels = new Map<number, number>();
    const seen = new Map<number, number>();
    for (const s of sources) {
      const ts = s.timestamp;
      const stale = ts !== undefined && this.seen.get(s.source) === ts;
      if (ts !== undefined) seen.set(s.source, ts);
      const level = stale ? 0 : (s.audioLevel ?? 0);
      levels.set(s.source, Math.max(level, levels.get(s.source) ?? 0));
    }
    this.seen = seen;
    return levels;
  }
}
