// Header chip text and record buttons for the daemon's recorder state.
import { clock } from './format';
import type { Status, Step } from './types';

/** The fields of `GET /api/status` the chip needs. */
export type Snapshot = Pick<Status, 'phase' | 'since_ms' | 'source_app'>;

export type Tone = 'idle' | 'armed' | 'recording' | 'finishing' | 'processing';

export type RecordAction = 'start' | 'stop' | 'discard';

type T = (key: string, params?: Record<string, string | number>) => string;

/**
 * Text and tone for the header chip. The recorder phase wins; a processing
 * job shows only while nothing is being recorded. With several jobs the one
 * that started last is shown (`progress` keeps insertion order, and meeting
 * ids are never integer-like keys). `snapshot` is null until the first
 * `/api/status` answer arrives.
 */
export function statusChipText(
  snapshot: Snapshot | null,
  progress: Record<string, { step: Step; pct: number }>,
  now: number,
  t: T,
): { text: string; tone: Tone } {
  const app = snapshot?.source_app || null;
  switch (snapshot?.phase) {
    case 'armed':
      return {
        text: app ? t('status.armed', { app }) : t('status.armed_any'),
        tone: 'armed',
      };
    case 'recording': {
      const elapsed = clock(now - snapshot.since_ms);
      return {
        text: app ? t('status.recording', { app, elapsed }) : t('status.recording_any', { elapsed }),
        tone: 'recording',
      };
    }
    case 'finishing':
      return { text: t('status.finishing'), tone: 'finishing' };
  }
  const jobs = Object.values(progress);
  const job = jobs[jobs.length - 1];
  if (job) {
    return {
      text: t('status.processing', { step: t(`step.${job.step}`), pct: Math.round(job.pct * 100) }),
      tone: 'processing',
    };
  }
  return { text: t('status.idle'), tone: 'idle' };
}

/**
 * What a screen reader hears: the chip's phase without elapsed time or
 * percent, so a polite live region speaks only when the phase, app or step
 * changes, not on every tick.
 */
export function statusAnnouncement(
  snapshot: Snapshot | null,
  progress: Record<string, { step: Step; pct: number }>,
  t: T,
): string {
  const chip = statusChipText(snapshot, progress, 0, t);
  switch (chip.tone) {
    case 'recording': {
      const app = snapshot?.source_app || null;
      return app ? t('status.live_recording', { app }) : t('status.live_recording_any');
    }
    case 'processing': {
      const jobs = Object.values(progress);
      return t('status.live_processing', { step: t(`step.${jobs[jobs.length - 1].step}`) });
    }
    default:
      return chip.text;
  }
}

/** Buttons offered next to the chip, in display order. */
export function recordActions(phase: string): RecordAction[] {
  switch (phase) {
    case 'recording':
      return ['stop', 'discard'];
    case 'finishing':
      return [];
    default:
      return ['start'];
  }
}
