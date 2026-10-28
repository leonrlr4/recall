// Daemon state fed by /api/events and /api/status, shared by every page.
import { events, record, status } from './api';
import type { Snapshot, RecordAction } from './status';
import type { RecallEvent, Step } from './types';

/**
 * `armed` and `finishing` have no event of their own, so the status is also
 * polled at this interval while the tab is visible, and read again when the
 * tab becomes visible. It is a local request.
 */
const POLL_MS = 2000;

export const live = $state<{
  /** Recorder state; null until the first status answer or event. */
  snapshot: Snapshot | null;
  /** Latest progress per meeting id while it is processing. */
  progress: Record<string, { step: Step; pct: number }>;
  /** Bumped when a meeting appears, finishes or fails, so lists refetch. */
  version: number;
  /** Meeting id of the latest done/failed event. */
  changed: string | null;
}>({ snapshot: null, progress: {}, version: 0, changed: null });

/** Counts recorder events, so a status answer older than one is dropped. */
let recorderEvents = 0;
/** Numbers status requests; an answer older than the last applied one is dropped. */
let requestSeq = 0;
let appliedSeq = 0;

function apply(e: RecallEvent) {
  switch (e.kind) {
    case 'recording':
      recorderEvents++;
      // The event has no start time; the status answer brings the daemon's.
      live.snapshot = {
        phase: 'recording',
        since_ms: Date.now(),
        source_app: e.app,
        meet: null,
        screen: null,
      };
      void refreshStatus();
      break;
    case 'stopped':
      recorderEvents++;
      live.snapshot = {
        phase: 'idle',
        since_ms: Date.now(),
        source_app: null,
        meet: null,
        screen: null,
      };
      live.version++;
      void refreshStatus();
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

/** Fetches `/api/status` unless a recorder event lands while it is in flight. */
export async function refreshStatus(): Promise<void> {
  const seen = recorderEvents;
  const mine = ++requestSeq;
  try {
    const s = await status();
    if (recorderEvents !== seen || mine < appliedSeq) return;
    appliedSeq = mine;
    live.snapshot = {
      phase: s.phase,
      since_ms: s.since_ms,
      source_app: s.source_app,
      meet: s.meet,
      screen: s.screen,
    };
  } catch {
    // Keep the last known state; the next poll or event corrects it.
  }
}

/** Sends a record command, then reads back the state it led to. */
export async function sendRecord(action: RecordAction | 'screen_off'): Promise<void> {
  await record(action);
  await refreshStatus();
}

let close: (() => void) | null = null;

export function connectLive(): void {
  if (close || typeof EventSource === 'undefined') return;
  const closeEvents = events(apply);
  let timer: ReturnType<typeof setInterval> | undefined;
  const visibility = () => {
    clearInterval(timer);
    timer = undefined;
    if (document.hidden) return;
    void refreshStatus();
    timer = setInterval(() => void refreshStatus(), POLL_MS);
  };
  document.addEventListener('visibilitychange', visibility);
  visibility();
  close = () => {
    closeEvents();
    clearInterval(timer);
    document.removeEventListener('visibilitychange', visibility);
  };
}
