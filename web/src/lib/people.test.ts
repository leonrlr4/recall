import { describe, expect, it } from 'vitest';
import { unnamedCount, visiblePeople } from './people';
import type { Person } from './types';

const person = (id: string, name: string, named = true): Person => ({
  id,
  name,
  named,
  aliases: [],
  samples: 1,
  meetings: 1,
});

const all = [
  person('b', 'Bob'),
  person('r1', 'Remote 1', false),
  person('me', 'Me'),
  person('a', 'Ann'),
];

describe('visiblePeople', () => {
  it('visible_people_hides_unnamed_voices_by_default', () => {
    expect(visiblePeople(all, false).map((p) => p.id)).toEqual(['me', 'a', 'b']);
    expect(unnamedCount(all)).toBe(1);
  });

  it('visible_people_shows_unnamed_voices_when_toggled', () => {
    expect(visiblePeople(all, true).map((p) => p.id)).toEqual(['me', 'a', 'b', 'r1']);
  });
});
