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
 * its last packet for up to 10 s after packets stop, and when Meet moves a
 * participant to another receiver both receivers list the same source until
 * the old entry expires. So only the entry with the newest `timestamp` for a
 * source counts, and only if that timestamp advanced past the newest one of
 * the previous read; otherwise the source reads as silent. Comparing with
 * the source's own earlier timestamps keeps the timestamp's clock irrelevant.
 * An entry without a timestamp always counts.
 */
export class SourceLevels {
  private newest = new Map<number, number>();

  read(sources: Iterable<RtpSource>): Map<number, number> {
    const entries = [...sources];
    const newest = new Map<number, number>();
    for (const { source, timestamp } of entries) {
      if (timestamp !== undefined) newest.set(source, Math.max(timestamp, newest.get(source) ?? -Infinity));
    }
    const levels = new Map<number, number>();
    for (const { source, timestamp, audioLevel } of entries) {
      const previous = this.newest.get(source);
      const fresh =
        timestamp === undefined ||
        (timestamp === newest.get(source) && (previous === undefined || timestamp > previous));
      const level = fresh ? (audioLevel ?? 0) : 0;
      levels.set(source, Math.max(level, levels.get(source) ?? 0));
    }
    this.newest = newest;
    return levels;
  }
}
