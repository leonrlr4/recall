// Filtering, ordering and due-date display for the To-do page and the
// meeting page's action items.

import type { ActionRow, Priority } from './types';

/** `me` is the user's own items (`mine`), `all` everyone's, anything else
 *  one owner's name. */
export interface TodoFilter {
  owner: string;
  status: 'open' | 'done' | 'all';
  /** Meeting id; empty for every meeting. */
  meeting: string;
}

export const DEFAULT_FILTER: TodoFilter = { owner: 'me', status: 'open', meeting: '' };

const ISO_DATE = /^\d{4}-\d{2}-\d{2}$/;

export function isIsoDate(s: string | null): s is string {
  return s !== null && ISO_DATE.test(s);
}

const PRIORITY_RANK: Record<Priority, number> = { high: 0, medium: 1, low: 2 };

function priorityRank(p: Priority | null): number {
  return p ? PRIORITY_RANK[p] : 3;
}

/** Calendar dates first, earliest first; then dues written as words; then
 *  none. */
function dueRank(due: string | null): [number, string] {
  if (isIsoDate(due)) return [0, due];
  return due ? [1, ''] : [2, ''];
}

/**
 * Sorted by due date, then priority (high first), then newest meeting, then
 * the meeting's own order. Does not modify `rows`.
 */
export function sortTodos(rows: ActionRow[]): ActionRow[] {
  return [...rows].sort((a, b) => {
    const [ra, da] = dueRank(a.due);
    const [rb, db] = dueRank(b.due);
    return (
      ra - rb ||
      da.localeCompare(db) ||
      priorityRank(a.priority) - priorityRank(b.priority) ||
      b.started.localeCompare(a.started) ||
      a.index - b.index
    );
  });
}

export function filterTodos(rows: ActionRow[], f: TodoFilter): ActionRow[] {
  const owner = f.owner.toLowerCase();
  return rows.filter(
    (r) =>
      (f.owner === 'me'
        ? r.mine
        : f.owner === 'all' || (r.owner ?? '').toLowerCase() === owner) &&
      (f.status === 'all' || r.done === (f.status === 'done')) &&
      (!f.meeting || r.meeting === f.meeting),
  );
}

/** Distinct owners other than the user, as first spelled, by name. */
export function owners(rows: ActionRow[]): string[] {
  const seen = new Map<string, string>();
  for (const r of rows) {
    if (!r.owner || r.mine) continue;
    const key = r.owner.toLowerCase();
    if (!seen.has(key)) seen.set(key, r.owner);
  }
  return [...seen.values()].sort((a, b) => a.localeCompare(b));
}

/** Distinct meetings in `rows`, in the order they first appear. */
export function meetings(rows: ActionRow[]): { id: string; title: string }[] {
  const seen = new Map<string, string>();
  for (const r of rows) if (!seen.has(r.meeting)) seen.set(r.meeting, r.title);
  return [...seen].map(([id, title]) => ({ id, title }));
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
