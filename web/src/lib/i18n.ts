import en from '../i18n/en.json';
import zhTW from '../i18n/zh-TW.json';
import { lang } from './locale.svelte';

export type Locale = 'zh-TW' | 'en';
export const LOCALES: Locale[] = ['zh-TW', 'en'];

type Dict = { [key: string]: string | Dict };
const DICTS: Record<Locale, Dict> = { 'zh-TW': zhTW, en };
const STORAGE_KEY = 'recall_lang';

/** A stored choice wins; otherwise any Chinese browser locale gets zh-TW. */
export function detectLocale(stored: string | null, browser: string | undefined): Locale {
  if (stored === 'zh-TW' || stored === 'en') return stored;
  return browser?.toLowerCase().startsWith('zh') ? 'zh-TW' : 'en';
}

/** Replaces `{name}` placeholders; unknown names stay as written. */
export function format(template: string, params: Record<string, string | number>): string {
  return template.replace(/\{(\w+)\}/g, (m, k: string) => (k in params ? String(params[k]) : m));
}

function storedLocale(): string | null {
  try {
    return globalThis.localStorage?.getItem(STORAGE_KEY) ?? null;
  } catch {
    return null;
  }
}

lang.current = detectLocale(storedLocale(), globalThis.navigator?.language);

export function locale(): Locale {
  return lang.current;
}

export function setLocale(l: Locale): void {
  lang.current = l;
  try {
    globalThis.localStorage?.setItem(STORAGE_KEY, l);
  } catch {
    // Private windows may refuse storage; the choice then lasts this session.
  }
  if (globalThis.document) document.documentElement.lang = l === 'zh-TW' ? 'zh-Hant-TW' : 'en';
}

function lookup(dict: Dict, key: string): string | undefined {
  let node: string | Dict | undefined = dict;
  for (const part of key.split('.')) {
    if (node === undefined || typeof node === 'string') return undefined;
    node = node[part];
  }
  return typeof node === 'string' ? node : undefined;
}

/** Translates `key` in the current locale, falling back to English, then the key. */
export function t(key: string, params?: Record<string, string | number>): string {
  const s = lookup(DICTS[lang.current], key) ?? lookup(DICTS.en, key) ?? key;
  return params ? format(s, params) : s;
}
