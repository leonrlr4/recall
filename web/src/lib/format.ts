/** Formats milliseconds as `mm:ss`, or `h:mm:ss` from one hour up. */
export function clock(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => String(n).padStart(2, '0');
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${pad(m)}:${pad(s)}`;
}

/** `YYYY-MM-DD` of the local calendar day, used to group meetings by date. */
export function dateKey(d: Date): string {
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/**
 * Up to two initials for a display name. CJK names give their first
 * character only, since two characters of a Chinese name read as a word.
 */
export function initials(name: string): string {
  const words = name.trim().split(/\s+/).filter(Boolean);
  if (words.length === 0) return '?';
  const first = Array.from(words[0])[0];
  if (/\p{Script=Han}/u.test(first)) return first;
  const second = words.length > 1 ? Array.from(words[words.length - 1])[0] : '';
  return (first + second).toUpperCase();
}

/** Size of the speaker palette; `app.css` defines `--spk-0` to `--spk-7`. */
const PALETTE = 8;

/** A stable palette colour for a speaker id (FNV-1a hash of the id). */
export function speakerColor(id: string): string {
  let h = 0x811c9dc5;
  for (let i = 0; i < id.length; i++) {
    h ^= id.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return `var(--spk-${(h >>> 0) % PALETTE})`;
}

export type StateKind = 'recording' | 'processing' | 'failed' | 'done';

/** Splits a `MeetingRow.state` such as `"processing:transcribe"`. */
export function parseState(state: string): { kind: StateKind; step: string | null } {
  const [kind, step] = state.split(':', 2);
  return { kind: kind as StateKind, step: step ?? null };
}

type StepMap = Partial<Record<string, unknown>>;
const STEP_ORDER = ['segment', 'transcribe', 'diarize', 'name', 'summarize', 'write'];

/**
 * The list state for a loaded meeting, following `state_of` in
 * recall-store's index.rs (the audio file is assumed to exist).
 */
export function stateOfSteps(steps: StepMap): string {
  const kind = (s: string) => {
    const v = steps[s];
    if (v === undefined || v === 'pending') return 'pending';
    if (v === 'running') return 'running';
    return typeof v === 'object' && v !== null && 'failed' in v ? 'failed' : 'done';
  };
  const failed = STEP_ORDER.find((s) => kind(s) === 'failed');
  if (failed) return `failed:${failed}`;
  const running = STEP_ORDER.find((s) => kind(s) === 'running');
  if (running) return `processing:${running}`;
  const pending = STEP_ORDER.find((s) => kind(s) !== 'done');
  return pending ? `processing:${pending}` : 'done';
}
