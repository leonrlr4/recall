// Which language the meeting page shows a summary in: the language it was
// written in (`summary.lang`) or one of its translations.

import type { Summary, SummaryText } from './types';

export const STORAGE_KEY = 'recall_summary_lang';

/** Languages first in the switch, in this order; any other follows by tag. */
const KNOWN = ['en', 'zh-TW'];
const LABELS: Record<string, string> = { en: 'EN', 'zh-TW': '中文' };

export function langLabel(lang: string): string {
  return LABELS[lang] ?? lang;
}

/** The translation lines up item for item with the summary. The server
 *  checks this before saving one; a mismatch here is treated as missing. */
export function fits(summary: Summary, text: SummaryText): boolean {
  return (
    text.decisions.length === summary.decisions.length &&
    text.actions.length === summary.actions.length &&
    text.chapters.length === summary.chapters.length
  );
}

/**
 * The switch's choices: the summary's own language, its translations and
 * `wanted`, so a meeting without that translation still shows the choice
 * (with a note). Empty for a summary from before languages were recorded,
 * and for one with nothing to switch to.
 */
export function summaryLangs(summary: Summary, wanted: string): string[] {
  if (!summary.lang) return [];
  const all = new Set([summary.lang, ...Object.keys(summary.translations ?? {}), wanted]);
  if (all.size < 2) return [];
  const rank = (l: string) => {
    const i = KNOWN.indexOf(l);
    return i === -1 ? KNOWN.length : i;
  };
  return [...all].sort((a, b) => rank(a) - rank(b) || a.localeCompare(b));
}

export interface SummaryView {
  /** The summary with its texts in the shown language. Action items keep
   *  their done marks, owners, dues and times, which are shared. */
  summary: Summary;
  /** `wanted` has no usable translation, so the summary shows as written. */
  missing: boolean;
}

/** `summary` in `wanted`, or as written when that has no translation. */
export function viewSummary(summary: Summary, wanted: string): SummaryView {
  if (!summary.lang || wanted === summary.lang) return { summary, missing: false };
  const text = summary.translations?.[wanted];
  if (!text || !fits(summary, text)) return { summary, missing: true };
  return {
    summary: {
      ...summary,
      one_line: text.one_line,
      body: text.body,
      decisions: text.decisions,
      actions: summary.actions.map((a, i) => ({ ...a, text: text.actions[i] })),
      chapters: summary.chapters.map((c, i) => ({ ...c, title: text.chapters[i] })),
    },
    missing: false,
  };
}

export function readStoredLang(): string | null {
  try {
    return globalThis.localStorage?.getItem(STORAGE_KEY) ?? null;
  } catch {
    return null;
  }
}

export function storeLang(lang: string): void {
  try {
    globalThis.localStorage?.setItem(STORAGE_KEY, lang);
  } catch {
    // Private windows may refuse storage; the choice then lasts this page load.
  }
}
