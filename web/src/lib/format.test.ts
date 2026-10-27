import { describe, expect, it } from 'vitest';
import { clock, dateKey, initials, parseState, speakerColor, stateOfSteps } from './format';

describe('format', () => {
  it('clock_formats', () => {
    expect(clock(0)).toBe('00:00');
    expect(clock(999)).toBe('00:00');
    expect(clock(61_000)).toBe('01:01');
    expect(clock(59 * 60_000 + 59_999)).toBe('59:59');
    expect(clock(3_600_000)).toBe('1:00:00');
    expect(clock(3_725_000)).toBe('1:02:05');
    expect(clock(-5)).toBe('00:00');
  });

  it('date_key_uses_local_calendar_day', () => {
    const d = new Date(2026, 9, 27, 23, 59);
    expect(dateKey(d)).toBe('2026-10-27');
  });

  it('initials_take_first_letters', () => {
    expect(initials('Ann Lee')).toBe('AL');
    expect(initials('leon')).toBe('L');
    expect(initials('王小明')).toBe('王');
    expect(initials('  ')).toBe('?');
  });

  it('speaker_color_is_stable_and_in_palette', () => {
    expect(speakerColor('3f1c')).toBe(speakerColor('3f1c'));
    for (const id of ['me', 'a', 'b', 'c', 'uuid-1']) {
      expect(speakerColor(id)).toMatch(/^var\(--spk-[0-7]\)$/);
    }
  });

  it('state_parses_kind_and_step', () => {
    expect(parseState('done')).toEqual({ kind: 'done', step: null });
    expect(parseState('recording')).toEqual({ kind: 'recording', step: null });
    expect(parseState('processing:transcribe')).toEqual({ kind: 'processing', step: 'transcribe' });
    expect(parseState('failed:write')).toEqual({ kind: 'failed', step: 'write' });
  });

  it('state_of_steps_matches_the_store_rule', () => {
    const done = { done: { at: '2026-10-27T00:00:00Z' } };
    const all = { segment: done, transcribe: done, diarize: done, name: done, summarize: done, write: done };
    expect(stateOfSteps({})).toBe('processing:segment');
    expect(stateOfSteps(all)).toBe('done');
    expect(stateOfSteps({ ...all, name: 'pending' })).toBe('processing:name');
    expect(stateOfSteps({ ...all, name: 'pending', summarize: 'running' })).toBe('processing:summarize');
    expect(stateOfSteps({ ...all, write: { failed: { at: 'x', error: 'disk' } }, name: 'running' })).toBe(
      'failed:write',
    );
  });
});
