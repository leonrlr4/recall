import { describe, expect, it } from 'vitest';
import {
  canRenameInMeeting,
  renameRequest,
  renameSuggestions,
  unnamedCount,
  visiblePeople,
} from './people';
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

describe('meeting rename', () => {
  const registry = [
    person('me', 'Leon'),
    { ...person('a', 'Ann Lee'), aliases: ['Annie'] },
    person('r1', 'Remote 1', false),
    person('r2', 'Remote 2', false),
  ];

  it('rename_offered_only_for_unnamed_voices', () => {
    expect(canRenameInMeeting('r1', registry)).toBe(true);
    expect(canRenameInMeeting('a', registry)).toBe(false);
    expect(canRenameInMeeting('me', registry)).toBe(false);
    // Not in the registry (yet): nothing to rename.
    expect(canRenameInMeeting('zz', registry)).toBe(false);
  });

  it('rename_merges_into_a_named_person_with_that_name', () => {
    expect(renameRequest('r1', ' ann lee ', registry)).toEqual({ merge_into: 'a' });
    expect(renameRequest('r1', 'ANNIE', registry)).toEqual({ merge_into: 'a' });
    expect(renameRequest('r1', 'leon', registry)).toEqual({ merge_into: 'me' });
    // Another unnamed voice's placeholder name is not a person to merge into.
    expect(renameRequest('r1', 'Remote 2', registry)).toEqual({ name: 'Remote 2' });
    expect(renameRequest('r1', ' Bob Chen ', registry)).toEqual({ name: 'Bob Chen' });
  });
});
