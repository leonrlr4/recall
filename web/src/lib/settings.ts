import type { Config, ConfigPatch } from './types';

/** A track whose loudest block stays at or below this is reported silent. */
export const SILENCE_PEAK = 0.02;

export const VERDICTS = ['ok', 'mic_silent', 'sys_silent', 'both_silent'] as const;
export type Verdict = (typeof VERDICTS)[number];

/** Judges a self-test from the peak levels of the two tracks. */
export function selftestVerdict(mic: number[], sys: number[]): Verdict {
  const heard = (peaks: number[]) => peaks.some((p) => p > SILENCE_PEAK);
  const m = heard(mic);
  const s = heard(sys);
  if (m && s) return 'ok';
  if (m) return 'sys_silent';
  if (s) return 'mic_silent';
  return 'both_silent';
}

export type UiLang = 'auto' | 'zh-TW' | 'en';

// Choices the settings form offers; each has a `settings.backend.*` or
// `settings.lang.*` string.
export const STT_ENGINES = ['breeze'];
export const BACKENDS = ['ollama', 'claude'];
export const SUMMARY_LANGS = ['zh-TW', 'en'];
export const UI_LANGS: UiLang[] = ['auto', 'zh-TW', 'en'];

/** The settings select value for a config `ui_lang`; unset means follow the browser. */
export function uiLangOf(v: string | null | undefined): UiLang {
  return v === 'zh-TW' || v === 'en' ? v : 'auto';
}

const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);

/**
 * The fields of `next` that differ from `prev`, shaped as a `PUT /api/config`
 * body. Sections (`llm`, `stt`) carry only their changed keys. An unset
 * `ui_lang` (absent or null) on both sides is no change; clearing a set one
 * sends `null`.
 */
export function configDiff(prev: Config, next: Config): ConfigPatch {
  const out: Record<string, unknown> = {};
  for (const key of Object.keys(next) as (keyof Config)[]) {
    const a = prev[key];
    const b = next[key];
    if (key === 'llm' || key === 'stt') {
      const section: Record<string, unknown> = {};
      const pa = a as unknown as Record<string, unknown>;
      const pb = b as unknown as Record<string, unknown>;
      for (const k of Object.keys(pb)) if (!same(pa[k], pb[k])) section[k] = pb[k];
      if (Object.keys(section).length > 0) out[key] = section;
    } else if (key === 'ui_lang') {
      if ((a ?? null) !== (b ?? null)) out[key] = b ?? null;
    } else if (!same(a, b)) {
      out[key] = b;
    }
  }
  return out as ConfigPatch;
}

/**
 * `config` with `patch` written over it, the way `PUT /api/config` merges:
 * sections key by key, `ui_lang: null` removes the language.
 */
export function applyPatch(config: Config, patch: ConfigPatch): Config {
  const out = structuredClone(config);
  const { llm, stt, ...top } = patch;
  Object.assign(out, top);
  if (llm) Object.assign(out.llm, llm);
  if (stt) Object.assign(out.stt, stt);
  if (out.ui_lang === null) delete out.ui_lang;
  return out;
}

/** One patch with the effect of `a` then `b`. */
export function mergePatch(a: ConfigPatch, b: ConfigPatch): ConfigPatch {
  const out: ConfigPatch = { ...a, ...b };
  if (a.llm || b.llm) out.llm = { ...a.llm, ...b.llm };
  if (a.stt || b.stt) out.stt = { ...a.stt, ...b.stt };
  return out;
}

/** Form fields, dotted for section keys, in the order the form shows them. */
export const FIELDS = [
  'meetings_dir',
  'data_dir',
  'record_screen',
  'allowlist',
  'debounce_secs',
  'end_grace_secs',
  'stt.engine',
  'stt.threads',
  'llm.backend',
  'llm.model',
  'llm.endpoint',
  'llm.context_tokens',
  'summary_lang',
  'ui_lang',
  'port',
] as const;

export type Field = (typeof FIELDS)[number];

/**
 * The `error` event text the server sends when a meeting's transcript does
 * not fit `llm.context_tokens`; see `recall-server/src/ask.rs`.
 */
export const ASK_TOO_LONG =
  'this meeting is too long for the current context; raise llm.context_tokens';

/** The i18n key for an ask failure the UI explains itself, else null. */
export function askErrorKey(message: string): string | null {
  return message === ASK_TOO_LONG ? 'ask.too_long' : null;
}

/** The form field a server error message names, so it can show under that field. */
export function fieldOfError(message: string): Field | null {
  for (const f of FIELDS) {
    const re = new RegExp(`(^|[^\\w.])${f.replace('.', '\\.')}($|[^\\w.])`);
    if (re.test(message)) return f;
  }
  return null;
}
