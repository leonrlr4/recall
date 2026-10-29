import { describe, expect, it } from 'vitest';
import {
  DEFAULT_FILTER,
  dueLabel,
  filterTodos,
  isOverdue,
  meetings,
  owners,
  sortTodos,
} from './todo';
import type { ActionRow } from './types';

function row(text: string, over: Partial<ActionRow> = {}): ActionRow {
  return {
    meeting: 'm1',
    title: 'Weekly',
    started: '2026-10-27T01:00:00Z',
    index: 0,
    text,
    owner: null,
    due: null,
    priority: null,
    source_ms: null,
    done: false,
    done_at: null,
    mine: false,
    ...over,
  };
}

const texts = (rows: ActionRow[]) => rows.map((r) => r.text);

describe('todo', () => {
  it('sorts_by_due_then_priority', () => {
    const rows = [
      row('no due'),
      row('words', { due: 'next sprint' }),
      row('late low', { due: '2026-11-05', priority: 'low' }),
      row('late high', { due: '2026-11-05', priority: 'high' }),
      row('early', { due: '2026-11-01' }),
      row('no due high', { priority: 'high' }),
    ];
    expect(texts(sortTodos(rows))).toEqual([
      'early',
      'late high',
      'late low',
      'words',
      'no due high',
      'no due',
    ]);
    expect(rows[0].text).toBe('no due');
  });

  it('sorts_ties_by_newest_meeting_then_order', () => {
    const rows = [
      row('old 1', { index: 1 }),
      row('old 0', { index: 0 }),
      row('new', { meeting: 'm2', started: '2026-10-28T01:00:00Z' }),
    ];
    expect(texts(sortTodos(rows))).toEqual(['new', 'old 0', 'old 1']);
  });

  it('filters_by_owner_status_and_meeting', () => {
    const rows = [
      row('mine', { owner: 'Leon', mine: true }),
      row('amy', { owner: 'Amy' }),
      row('amy done', { owner: 'amy', done: true }),
      row('other meeting', { owner: 'Leon', mine: true, meeting: 'm2' }),
    ];
    expect(texts(filterTodos(rows, DEFAULT_FILTER))).toEqual(['mine', 'other meeting']);
    expect(texts(filterTodos(rows, { owner: 'AMY', status: 'all', meeting: '' }))).toEqual([
      'amy',
      'amy done',
    ]);
    expect(texts(filterTodos(rows, { owner: 'all', status: 'done', meeting: '' }))).toEqual([
      'amy done',
    ]);
    expect(texts(filterTodos(rows, { owner: 'all', status: 'open', meeting: 'm2' }))).toEqual([
      'other meeting',
    ]);
  });

  it('lists_other_owners_and_meetings_once', () => {
    const rows = [
      row('a', { owner: 'Tom' }),
      row('b', { owner: 'amy' }),
      row('c', { owner: 'Amy', meeting: 'm2', title: 'Review' }),
      row('d', { owner: 'Leon', mine: true }),
      row('e'),
    ];
    expect(owners(rows)).toEqual(['amy', 'Tom']);
    expect(meetings(rows)).toEqual([
      { id: 'm1', title: 'Weekly' },
      { id: 'm2', title: 'Review' },
    ]);
  });

  it('marks_open_past_due_items_overdue', () => {
    expect(isOverdue({ due: '2026-10-28', done: false }, '2026-10-29')).toBe(true);
    expect(isOverdue({ due: '2026-10-29', done: false }, '2026-10-29')).toBe(false);
    expect(isOverdue({ due: '2026-10-28', done: true }, '2026-10-29')).toBe(false);
    expect(isOverdue({ due: 'last week', done: false }, '2026-10-29')).toBe(false);
  });

  it('labels_due_dates', () => {
    const now = new Date(2026, 9, 29);
    expect(dueLabel('2026-11-03', 'en', now)).toBe('Nov 3');
    expect(dueLabel('2027-01-05', 'en', now)).toBe('Jan 5, 2027');
    expect(dueLabel('next sprint', 'en', now)).toBe('next sprint');
  });
});
