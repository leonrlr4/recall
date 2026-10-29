import { describe, expect, it } from 'vitest';
import { match, meetingPath } from './router';

describe('router', () => {
  it('matches_the_fixed_paths', () => {
    expect(match('/')).toEqual({ name: 'list' });
    expect(match('/todo')).toEqual({ name: 'not_found' });
    expect(match('/people')).toEqual({ name: 'people' });
    expect(match('/settings')).toEqual({ name: 'settings' });
    expect(match('/ask')).toEqual({ name: 'ask' });
    expect(match('/nope')).toEqual({ name: 'not_found' });
  });

  it('meeting_path_round_trips', () => {
    const id = '2026-10-27 14:30 週會 / review';
    const path = meetingPath(id, 61_500.4);
    const [p, s] = path.split('?');
    expect(match(p, `?${s}`)).toEqual({ name: 'meeting', id, t: 61_500 });
    expect(match(meetingPath(id))).toEqual({ name: 'meeting', id, t: null });
  });
});
