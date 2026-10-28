/** One event as the daemon's `POST /api/meet/events` accepts it. `t` is Unix ms. */
export type MeetEvent =
  | { t: number; kind: 'speaking_start' | 'speaking_stop'; participant: string }
  | { t: number; kind: 'name'; participant: string; name: string }
  | { t: number; kind: 'roster'; names: string[] }
  | { t: number; kind: 'status'; status: string };

/**
 * How long an event keeps being resent. The daemon starts recording 8 s
 * after the microphone opens and drops duplicates and events older than the
 * recording, so every batch carries this whole window.
 */
export const RESEND_WINDOW_MS = 60_000;

export class EventQueue {
  private events: MeetEvent[] = [];

  push(event: MeetEvent): void {
    this.events.push(event);
  }

  /** The events of the last `RESEND_WINDOW_MS`; older ones are dropped for good. */
  window(now: number): MeetEvent[] {
    this.events = this.events.filter((e) => now - e.t <= RESEND_WINDOW_MS);
    return [...this.events];
  }
}

/** The Meet code from a URL path such as `/abc-defg-hij`, or null off a call page. */
export function meetingCode(pathname: string): string | null {
  return /^\/([a-z]{3}-[a-z]{4}-[a-z]{3})(?:\/|$)/.exec(pathname)?.[1] ?? null;
}

/**
 * Rebuilds `value` as a `MeetEvent` from the fields the daemon knows, or
 * returns null, with `t` rounded to whole ms. This strips the hook's page-internal `source` marker and
 * anything else a page script could have added to a posted message.
 */
export function sanitize(value: unknown): MeetEvent | null {
  if (typeof value !== 'object' || value === null) return null;
  const v = value as Record<string, unknown>;
  if (typeof v.t !== 'number' || !Number.isFinite(v.t)) return null;
  // The daemon reads `t` as an integer and rejects the whole batch on a fraction.
  const t = Math.round(v.t);
  switch (v.kind) {
    case 'speaking_start':
    case 'speaking_stop':
      return typeof v.participant === 'string' ? { t, kind: v.kind, participant: v.participant } : null;
    case 'name':
      return typeof v.participant === 'string' && typeof v.name === 'string'
        ? { t, kind: 'name', participant: v.participant, name: v.name }
        : null;
    case 'roster':
      return Array.isArray(v.names) && v.names.every((n) => typeof n === 'string')
        ? { t, kind: 'roster', names: [...v.names] }
        : null;
    case 'status':
      return typeof v.status === 'string' ? { t, kind: 'status', status: v.status } : null;
    default:
      return null;
  }
}
