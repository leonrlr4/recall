import { describe, expect, it } from 'vitest';
import { renameSuggestions, unnamedCount, visiblePeople } from './people';
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

describe('renameSuggestions', () => {
  it('rename_suggestions_exclude_used_names', () => {
    const hints = ['Ann Lee', 'Bob', 'Carol', 'bob', 'Dan '];
    const names = { me: 'Dan', s1: 'Remote 1', s2: 'ann lee', s3: 'Carol' };
    // Ann is taken by s2 and Dan by me; Carol is s3's own name.
    expect(renameSuggestions(hints, names, 's3')).toEqual(['Bob', 'Carol']);
    expect(renameSuggestions(hints, names, 's1')).toEqual(['Bob']);
    expect(renameSuggestions([], names, 's1')).toEqual([]);
  });
});
