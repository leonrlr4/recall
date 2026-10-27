import type { Person } from './types';

/**
 * The people the People page lists: `me` first, then by name. Voices recall
 * registered that nobody has named yet are left out unless `showUnnamed`.
 */
export function visiblePeople(all: Person[], showUnnamed: boolean): Person[] {
  return all
    .filter((p) => showUnnamed || p.named)
    .sort((a, b) => (a.id === 'me' ? -1 : b.id === 'me' ? 1 : a.name.localeCompare(b.name)));
}

/** How many people are unnamed voices. */
export function unnamedCount(all: Person[]): number {
  return all.filter((p) => !p.named).length;
}
