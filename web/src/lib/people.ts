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

/**
 * Names from the Meet roster (`participants_hint`) offered when renaming
 * `speaker`: those no other speaker in the meeting already goes by, compared
 * without case or surrounding space, each once. `names` maps every speaker
 * id the meeting mentions to its display name.
 */
export function renameSuggestions(
  hints: string[],
  names: Record<string, string>,
  speaker: string,
): string[] {
  const key = (s: string) => s.trim().toLocaleLowerCase();
  const taken = new Set(
    Object.entries(names)
      .filter(([id]) => id !== speaker)
      .map(([, name]) => key(name)),
  );
  const out: string[] = [];
  const seen = new Set<string>();
  for (const hint of hints) {
    const k = key(hint);
    if (!k || taken.has(k) || seen.has(k)) continue;
    seen.add(k);
    out.push(hint.trim());
  }
  return out;
}
