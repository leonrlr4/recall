// Due-date display for the meeting page's action items.

const ISO_DATE = /^\d{4}-\d{2}-\d{2}$/;

export function isIsoDate(s: string | null): s is string {
  return s !== null && ISO_DATE.test(s);
}

/** An open item due before `today` (`YYYY-MM-DD`) is overdue. */
export function isOverdue(item: { due: string | null; done: boolean }, today: string): boolean {
  return !item.done && isIsoDate(item.due) && item.due < today;
}

/** A calendar due date as a short local date (`Nov 3`, `11月3日`), with the
 *  year when it is not this year's; words pass through. */
export function dueLabel(due: string, locale: string, now = new Date()): string {
  if (!isIsoDate(due)) return due;
  const [y, m, d] = due.split('-').map(Number);
  const date = new Date(y, m - 1, d);
  const opts: Intl.DateTimeFormatOptions = { month: 'short', day: 'numeric' };
  if (y !== now.getFullYear()) opts.year = 'numeric';
  return date.toLocaleDateString(locale, opts);
}
