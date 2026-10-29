import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  STORAGE_KEY,
  langLabel,
  readStoredLang,
  storeLang,
  summaryLangs,
  viewSummary,
} from './summaryLang';
import type { Summary, SummaryText } from './types';

function summary(over: Partial<Summary> = {}): Summary {
  return {
    one_line: 'Weekly sync.',
    body: 'Talked about the release.',
    decisions: ['Ship Friday'],
    actions: [
      {
        text: 'Write release notes',
        owner: 'Leon',
        due: '2026-10-30',
        priority: 'high',
        source_ms: 2_500,
        done: true,
        done_at: '2026-10-29T05:00:00Z',
      },
    ],
    chapters: [{ start_ms: 0, title: 'Intro' }],
    lang: 'en',
    ...over,
  };
}

const zh: SummaryText = {
  one_line: '週會。',
  body: '討論了發版。',
  decisions: ['週五發版'],
  actions: ['寫發版說明'],
  chapters: ['開場'],
};

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('summaryLang', () => {
  it('shows_the_translation_with_shared_done_marks_and_times', () => {
    const s = summary({ translations: { 'zh-TW': zh } });
    const { summary: view, missing } = viewSummary(s, 'zh-TW');
    expect(missing).toBe(false);
    expect(view.one_line).toBe('週會。');
    expect(view.decisions).toEqual(['週五發版']);
    expect(view.actions[0]).toEqual({ ...s.actions[0], text: '寫發版說明' });
    expect(view.chapters).toEqual([{ start_ms: 0, title: '開場' }]);
    // The original is untouched, so a toggle still sends its text.
    expect(s.actions[0].text).toBe('Write release notes');
  });

  it('shows_the_original_language_as_written', () => {
    const s = summary({ translations: { 'zh-TW': zh } });
    expect(viewSummary(s, 'en')).toEqual({ summary: s, missing: false });
  });

  it('falls_back_with_a_note_when_the_translation_is_missing_or_misaligned', () => {
    const s = summary();
    expect(viewSummary(s, 'zh-TW')).toEqual({ summary: s, missing: true });
    const short = summary({ translations: { 'zh-TW': { ...zh, actions: [] } } });
    expect(viewSummary(short, 'zh-TW')).toEqual({ summary: short, missing: true });
  });

  it('leaves_summaries_without_a_language_alone', () => {
    const old = summary({ lang: undefined });
    expect(viewSummary(old, 'zh-TW')).toEqual({ summary: old, missing: false });
    expect(summaryLangs(old, 'zh-TW')).toEqual([]);
  });

  it('offers_the_written_language_translations_and_the_wanted_one', () => {
    expect(summaryLangs(summary({ translations: { 'zh-TW': zh } }), 'en')).toEqual(['en', 'zh-TW']);
    expect(summaryLangs(summary(), 'zh-TW')).toEqual(['en', 'zh-TW']);
    expect(summaryLangs(summary(), 'en')).toEqual([]);
    expect(summaryLangs(summary({ lang: 'zh-TW', translations: { ja: zh } }), 'en')).toEqual([
      'en',
      'zh-TW',
      'ja',
    ]);
    expect(langLabel('en')).toBe('EN');
    expect(langLabel('zh-TW')).toBe('中文');
    expect(langLabel('ja')).toBe('ja');
  });

  it('remembers_the_choice_and_survives_refused_storage', () => {
    const store = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => store.set(k, v),
    });
    expect(readStoredLang()).toBe(null);
    storeLang('zh-TW');
    expect(store.get(STORAGE_KEY)).toBe('zh-TW');
    expect(readStoredLang()).toBe('zh-TW');

    const refuse = () => {
      throw new Error('SecurityError');
    };
    vi.stubGlobal('localStorage', { getItem: refuse, setItem: refuse });
    expect(readStoredLang()).toBe(null);
    expect(() => storeLang('en')).not.toThrow();
  });
});
