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
}

export interface ActionItem {
  text: string;
  owner: string | null;
  due: string | null;
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
}

export interface MeetingDetail {
  meeting: Meeting;
  /** Display name per speaker id. */
  names: Record<string, string>;
  markdown: string;
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
}

export interface LlmConfig {
  /** `ollama` or `claude`. */
  backend: string;
  model: string;
  endpoint: string;
}

export interface SttConfig {
  /** `breeze`. */
  engine: string;
  threads: number;
}

/** `GET /api/config`: the config the daemon is running with. */
export interface Config {
  meetings_dir: string;
  data_dir: string;
  record_screen: boolean;
  allowlist: string[];
  debounce_secs: number;
  end_grace_secs: number;
  min_length_secs: number;
  llm: LlmConfig;
  summary_lang: string;
  /** Absent when unset; `null` in a patch clears it. */
  ui_lang?: string | null;
  stt: SttConfig;
  port: number;
}

/** `PUT /api/config` body: any subset, sections may be partial. */
export type ConfigPatch = Partial<Omit<Config, 'llm' | 'stt'>> & {
  llm?: Partial<LlmConfig>;
  stt?: Partial<SttConfig>;
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
}
