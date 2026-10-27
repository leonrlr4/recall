import { describe, expect, it } from 'vitest';
import en from '../i18n/en.json';
import zhTW from '../i18n/zh-TW.json';
import { detectLocale, format } from './i18n';

function keys(obj: Record<string, unknown>, prefix = ''): string[] {
  return Object.entries(obj).flatMap(([k, v]) =>
    v !== null && typeof v === 'object'
      ? keys(v as Record<string, unknown>, `${prefix}${k}.`)
      : [`${prefix}${k}`],
  );
}

describe('i18n', () => {
  it('i18n_files_have_the_same_keys', () => {
    const a = keys(en).sort();
    const b = keys(zhTW).sort();
    expect(a.length).toBeGreaterThan(0);
    expect(b).toEqual(a);
  });

  it('no_translation_is_empty', () => {
    for (const dict of [en, zhTW]) {
      for (const k of keys(dict)) {
        const v = k.split('.').reduce<unknown>((o, p) => (o as Record<string, unknown>)[p], dict);
        expect(typeof v === 'string' && v.trim().length > 0, k).toBe(true);
      }
    }
  });

  it('placeholders_match_across_locales', () => {
    const params = (s: string) => [...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort();
    for (const k of keys(en)) {
      const get = (d: object) =>
        k.split('.').reduce<unknown>((o, p) => (o as Record<string, unknown>)[p], d) as string;
      expect(params(get(zhTW)), k).toEqual(params(get(en)));
    }
  });

  it('every_literal_t_key_exists', () => {
    // Literal keys only; template keys such as `step.${s}` are built from
    // enums whose members the parity test above already covers.
    const sources = import.meta.glob('../**/*.svelte', {
      query: '?raw',
      import: 'default',
      eager: true,
    }) as Record<string, string>;
    expect(Object.keys(sources).length).toBeGreaterThan(0);
    const known = new Set(keys(en));
    const missing: string[] = [];
    for (const [file, src] of Object.entries(sources)) {
      for (const m of src.matchAll(/\bt\(\s*'([^']+)'/g)) {
        if (!known.has(m[1])) missing.push(`${file}: ${m[1]}`);
      }
    }
    expect(missing).toEqual([]);
  });

  it('format_fills_named_params', () => {
    expect(format('{n} of {total}', { n: 2, total: 5 })).toBe('2 of 5');
    expect(format('missing {x}', {})).toBe('missing {x}');
  });

  it('detect_locale_prefers_stored_then_browser', () => {
    expect(detectLocale('en', 'zh-TW')).toBe('en');
    expect(detectLocale(null, 'zh-CN')).toBe('zh-TW');
    expect(detectLocale(null, 'zh')).toBe('zh-TW');
    expect(detectLocale(null, 'en-US')).toBe('en');
    expect(detectLocale('fr', 'de-DE')).toBe('en');
  });
});
