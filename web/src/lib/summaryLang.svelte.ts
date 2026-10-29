// The summary language this browser chose, shared by the parts of the
// meeting page that show summary text. Until a choice is made it follows
// the UI language.
import { locale } from './i18n';
import { readStoredLang, storeLang } from './summaryLang';

const chosen = $state<{ lang: string | null }>({ lang: readStoredLang() });

export function wantedSummaryLang(): string {
  return chosen.lang ?? locale();
}

export function chooseSummaryLang(lang: string): void {
  chosen.lang = lang;
  storeLang(lang);
}
