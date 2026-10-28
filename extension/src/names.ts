// DOM reading for the isolated-world content script. Meet's class names are
// obfuscated and change between releases; every selector lives in SELECTORS
// so a Meet update is a one-place fix.

import { SpeakingTracker, type SpeakingEvent } from './csrc';

export const SELECTORS = {
  /** A participant tile in the call grid. */
  tile: 'div[data-participant-id]',
  /** The participant's display name inside a tile or People panel row. */
  name: 'span.notranslate',
  /** Marks the user's own tile. */
  self: '[data-self-name]',
  /** Classes of the speaking glow; a tile glows when any element in it has one. */
  glow: ['Oaajhc', 'HX2H7', 'wEsLMd', 'OgVli'],
  /** A row of the People panel. It also carries data-participant-id, so it is not a tile. */
  panelItem: '[role="listitem"][data-participant-id]',
};

export interface Tile {
  /** The tile's `data-participant-id`. */
  id: string;
  name: string | null;
  self: boolean;
  glowing: boolean;
}

export interface NameEvent {
  t: number;
  kind: 'name';
  participant: string;
  name: string;
}

function nameIn(el: Element): string | null {
  const text = el.querySelector(SELECTORS.name)?.textContent?.trim();
  return text ? text : null;
}

const glowSelector = SELECTORS.glow.map((c) => `.${c}`).join(',');

/** The call grid's tiles in document order, outermost tile elements only. */
export function readTiles(root: ParentNode): Tile[] {
  const tiles: Tile[] = [];
  for (const el of root.querySelectorAll(SELECTORS.tile)) {
    if (el.matches(SELECTORS.panelItem)) continue;
    if (el.parentElement?.closest(SELECTORS.tile)) continue;
    tiles.push({
      id: el.getAttribute('data-participant-id') ?? '',
      name: nameIn(el),
      self: el.matches(SELECTORS.self) || el.querySelector(SELECTORS.self) !== null,
      glowing: el.matches(glowSelector) || el.querySelector(glowSelector) !== null,
    });
  }
  return tiles;
}

/**
 * Names of the remote participants from the grid and the People panel,
 * sorted and deduplicated. The user is left out: their speech comes from the
 * microphone, never from Meet, so their name is no naming choice.
 */
export function readRoster(root: ParentNode): string[] {
  const tiles = readTiles(root);
  const selfIds = new Set(tiles.filter((t) => t.self).map((t) => t.id));
  const names = new Set(tiles.filter((t) => !t.self && t.name).map((t) => t.name as string));
  for (const row of root.querySelectorAll(SELECTORS.panelItem)) {
    if (selfIds.has(row.getAttribute('data-participant-id') ?? '')) continue;
    const name = nameIn(row);
    if (name) names.add(name);
  }
  return [...names].sort();
}

/** Names of the glowing remote tiles, one entry per tile. */
export function glowingNames(tiles: Tile[]): string[] {
  return tiles.filter((t) => t.glowing && !t.self && t.name).map((t) => t.name as string);
}

/** Who was speaking (CSRC participants) and which remote tiles glowed at one instant. */
export interface Sample {
  speaking: string[];
  glowing: string[];
}

/**
 * Co-occurrence counts of (participant, tile name). A sample counts only when
 * exactly one participant speaks and exactly one remote tile glows; any other
 * sample cannot say who is who.
 */
class Counts {
  private counts = new Map<string, Map<string, number>>();

  add(sample: Sample): string | null {
    if (sample.speaking.length !== 1 || sample.glowing.length !== 1) return null;
    const [participant] = sample.speaking;
    const [name] = sample.glowing;
    const byName = this.counts.get(participant) ?? new Map<string, number>();
    byName.set(name, (byName.get(name) ?? 0) + 1);
    this.counts.set(participant, byName);
    return participant;
  }

  /** The most counted name; on a tie, `current` wins if it is among the leaders. */
  best(participant: string, current?: string): string | undefined {
    let best: string | undefined;
    let max = 0;
    for (const [name, n] of this.counts.get(participant) ?? []) {
      if (n > max || (n === max && name === current)) {
        best = name;
        max = n;
      }
    }
    return best;
  }

  participants(): string[] {
    return [...this.counts.keys()];
  }
}

/** The name each participant co-occurred with most across `samples`. */
export function correlate(samples: Sample[]): Map<string, string> {
  const counts = new Counts();
  for (const sample of samples) counts.add(sample);
  const names = new Map<string, string>();
  for (const participant of counts.participants()) {
    const name = counts.best(participant);
    if (name) names.set(participant, name);
  }
  return names;
}

/** Correlates samples as they arrive and emits a `name` event whenever a participant's name changes. */
export class Namer {
  private counts = new Counts();
  private names = new Map<string, string>();

  observe(t: number, speaking: string[], tiles: Tile[]): NameEvent[] {
    const participant = this.counts.add({ speaking, glowing: glowingNames(tiles) });
    if (participant === null) return [];
    const current = this.names.get(participant);
    const name = this.counts.best(participant, current);
    if (!name || name === current) return [];
    this.names.set(participant, name);
    return [{ t, kind: 'name', participant, name }];
  }
}

/**
 * Speaking detection from the glow alone, for when the CSRC hook could not
 * install. Each glowing remote tile speaks as `tile:<data-participant-id>`
 * and is named from its own label.
 */
export class GlowFallback {
  private tracker = new SpeakingTracker();
  private names = new Map<string, string>();

  observe(t: number, tiles: Tile[]): (SpeakingEvent | NameEvent)[] {
    const events: (SpeakingEvent | NameEvent)[] = [];
    const levels = new Map<string, number>();
    for (const tile of tiles) {
      if (tile.self || !tile.glowing) continue;
      const participant = `tile:${tile.id}`;
      levels.set(participant, 1);
      if (tile.name && this.names.get(participant) !== tile.name) {
        this.names.set(participant, tile.name);
        events.push({ t, kind: 'name', participant, name: tile.name });
      }
    }
    // Speaking events first, so a new speaker's start precedes its name.
    return [...this.tracker.update(t, levels), ...events];
  }

  get speakingCount(): number {
    return this.tracker.speakingCount;
  }
}
