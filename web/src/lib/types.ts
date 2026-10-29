// Mirrors of the recall-store types as the server serializes them.

export type Step = 'segment' | 'transcribe' | 'diarize' | 'name' | 'summarize' | 'write';
export const STEPS: Step[] = ['segment', 'transcribe', 'diarize', 'name', 'summarize', 'write'];

export type StepState =
  | 'pending'
  | 'running'
  | { done: { at: string } }
  | { failed: { at: string; error: string } };

export type Side = 'me' | 'them';

export interface Segment {
  start_ms: number;
  end_ms: number;
  side: Side;
  speaker: string | null;
  text: string;
}

export interface Cluster {
  label: string;
  speaker: string | null;
  /** How `speaker` was chosen; absent in older records. */
  named_by?: 'voiceprint' | 'meet' | 'user';
}

export type Priority = 'high' | 'medium' | 'low';

export interface ActionItem {
  text: string;
  owner: string | null;
  /** `YYYY-MM-DD` when the meeting gave a computable date, otherwise the
   *  words used. */
  due: string | null;
  priority: Priority | null;
  /** Milliseconds into the meeting where the item came up. */
  source_ms: number | null;
  done: boolean;
  done_at: string | null;
}

/** `GET /api/action-items`: one item with the meeting it came from. */
export interface ActionRow extends ActionItem {
  meeting: string;
  title: string;
  started: string;
  /** Position in the meeting's `summary.actions`, for the toggle. */
  index: number;
  /** The owner is the user. */
  mine: boolean;
}

export interface Chapter {
  start_ms: number;
  title: string;
}

export interface Summary {
  one_line: string;
  body: string;
  decisions: string[];
  actions: ActionItem[];
  chapters: Chapter[];
}

export interface Meeting {
  id: string;
  title: string;
  started: string;
  duration_ms: number;
  source_app: string | null;
  steps: Partial<Record<Step, StepState>>;
  segments: Segment[];
  clusters: Cluster[];
  summary: Summary | null;
  models: Record<string, string>;
  /** Names from the Meet roster, offered when renaming a speaker. */
  participants_hint: string[];
  /** Milliseconds from the audio origin to the first screen frame. */
  screen_offset_ms?: number;
  /** Why the screen recording failed, when it did. */
  screen_error?: string;
  /** Publish to the shared vault; absent means off. */
  sync?: boolean;
}

/** Where a meeting stands with the shared vault. */
export interface VaultStatus {
  /** The switch value asked for while a job was processing the meeting. */
  requested: boolean | null;
  /** A change is waiting to be pushed or is being pushed. */
  queued: boolean;
  /** The note's path in the vault as last pushed; null when it is not there. */
  path: string | null;
  synced_at: string | null;
  /** Why the last attempt failed; the daemon retries on its own. */
  error: string | null;
}

export interface MeetingDetail {
  meeting: Meeting;
  /** Display name per speaker id. */
  names: Record<string, string>;
  markdown: string;
  /** The meeting has `screen.mp4`, served at `/api/meetings/{id}/screen`. */
  has_screen: boolean;
  /** Null when publishing to a vault is off (no `vault.remote`). */
  vault: VaultStatus | null;
}

export interface MeetingRow {
  id: string;
  title: string;
  started: string;
  duration_ms: number;
  source_app: string | null;
  one_line: string | null;
  participants: string[];
  /** `recording`, `processing:<step>`, `failed:<step>` or `done`. */
  state: string;
}

export interface Hit {
  meeting: string;
  start_ms: number;
  speaker: string;
  /** Text around the match, with the match wrapped in `[` `]`. */
  snippet: string;
}

export interface Person {
  id: string;
  name: string;
  /** False for a voice recall registered that nobody has named yet. */
  named: boolean;
  aliases: string[];
  samples: number;
  meetings: number;
}

export type RecallEvent =
  | { kind: 'recording'; app: string }
  | { kind: 'stopped' }
  | { kind: 'progress'; meeting: string; step: Step; pct: number }
  | { kind: 'done'; meeting: string }
  | { kind: 'failed'; meeting: string; step: Step; error: string };

export interface ListFilter {
  q?: string;
  app?: string;
  participant?: string;
  /** RFC 3339, inclusive. */
  from?: string;
  /** RFC 3339, exclusive. */
  to?: string;
}

export interface SegmentPatch {
  index: number;
  text?: string;
  speaker?: string;
  delete?: boolean;
}

export interface MeetingPatch {
  title?: string;
  segments?: SegmentPatch[];
  sync?: boolean;
}

export interface LlmConfig {
  /** `ollama` or `claude`. */
  backend: string;
  model: string;
  endpoint: string;
  /** Context window in tokens; Ollama gets it as `num_ctx`. */
  context_tokens: number;
}

export interface SttConfig {
  /** `breeze`. */
  engine: string;
  threads: number;
}

export interface VaultConfig {
  /** The vault's remote URL; empty turns publishing off. */
  remote: string;
  /** recall's clone; empty means `<data_dir>/vault`. */
  dir: string;
  /** Folder in the vault for the meeting notes. */
  folder: string;
  branch: string;
}

/** `GET /api/config`: the config the daemon is running with. */
export interface Config {
  meetings_dir: string;
  data_dir: string;
  record_screen: boolean;
  allowlist: string[];
  debounce_secs: number;
  end_grace_secs: number;
  llm: LlmConfig;
  summary_lang: string;
  /** Absent when unset; `null` in a patch clears it. */
  ui_lang?: string | null;
  stt: SttConfig;
  port: number;
  vault: VaultConfig;
}

/** `PUT /api/config` body: any subset, sections may be partial. */
export type ConfigPatch = Partial<Omit<Config, 'llm' | 'stt' | 'vault'>> & {
  llm?: Partial<LlmConfig>;
  stt?: Partial<SttConfig>;
  vault?: Partial<VaultConfig>;
};

export interface SelfTest {
  /** 50 peak levels each, 0 to 1. */
  mic_peaks: number[];
  sys_peaks: number[];
}

export interface Status {
  phase: string;
  /** Unix milliseconds when `phase` began. */
  since_ms: number;
  source_app: string | null;
  mic_peak: number;
  sys_peak: number;
  /** The Meet extension's latest status during a recording, e.g. `signal_lost`. */
  meet: string | null;
  /** `recording`, `off` or `error` during a recording; `picker` outside one
   *  when the next recording will show the portal's screen picker. */
  screen: string | null;
}
