import { describe, expect, it } from 'vitest';
import { dueLabel, isOverdue } from './due';

describe('due', () => {
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
