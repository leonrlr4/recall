// Daemon state fed by /api/events, shared by every page.
import { events } from './api';
import type { RecallEvent, Step } from './types';

export const live = $state<{
  /** Source app being recorded, or null when idle. */
  recording: string | null;
  /** Latest progress per meeting id while it is processing. */
  progress: Record<string, { step: Step; pct: number }>;
  /** Bumped when a meeting appears, finishes or fails, so lists refetch. */
  version: number;
  /** Meeting id of the latest done/failed event. */
  changed: string | null;
}>({ recording: null, progress: {}, version: 0, changed: null });

function apply(e: RecallEvent) {
  switch (e.kind) {
    case 'recording':
      live.recording = e.app;
      break;
    case 'stopped':
      live.recording = null;
      live.version++;
      break;
    case 'progress':
      live.progress[e.meeting] = { step: e.step, pct: e.pct };
      break;
    case 'done':
    case 'failed':
      delete live.progress[e.meeting];
      live.changed = e.meeting;
      live.version++;
      break;
  }
}

let close: (() => void) | null = null;

export function connectLive(): void {
  if (close || typeof EventSource === 'undefined') return;
  close = events(apply);
}
