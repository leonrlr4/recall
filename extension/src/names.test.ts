import { describe, expect, it } from 'vitest';
import { loadFixture } from './fixture';
import { correlate, glowingNames, Namer, readRoster, readTiles } from './names';

describe('correlate', () => {
  it('correlate ignores ambiguous samples and picks majority', () => {
    const names = correlate([
      { speaking: ['csrc:1'], glowing: ['Tom Lin'] },
      { speaking: ['csrc:1'], glowing: ['Tom Lin'] },
      { speaking: ['csrc:1'], glowing: ['Amy Chen'] },
      // Ambiguous: two speakers, or two glowing tiles. Neither counts.
      { speaking: ['csrc:1', 'csrc:2'], glowing: ['Amy Chen'] },
      { speaking: ['csrc:1'], glowing: ['Amy Chen', 'Tom Lin'] },
      { speaking: ['csrc:1'], glowing: ['Amy Chen', 'Tom Lin'] },
      { speaking: ['csrc:2'], glowing: ['Amy Chen'] },
      { speaking: [], glowing: ['Tom Lin'] },
    ]);
    expect(names).toEqual(
      new Map([
        ['csrc:1', 'Tom Lin'],
        ['csrc:2', 'Amy Chen'],
      ]),
    );
  });
});

describe('DOM reading', () => {
  it('self tile excluded', () => {
    loadFixture('speaking');
    const tiles = readTiles(document);
    expect(tiles.map((tile) => [tile.name, tile.self, tile.glowing])).toEqual([
      ['Leon Wu', true, true],
      ['Tom Lin', false, false],
      ['Amy Chen', false, true],
    ]);
    // The user's own glowing tile does not make the sample ambiguous.
    expect(glowingNames(tiles)).toEqual(['Amy Chen']);

    const namer = new Namer();
    expect(namer.observe(5, ['csrc:9'], tiles)).toEqual([
      { t: 5, kind: 'name', participant: 'csrc:9', name: 'Amy Chen' },
    ]);
    expect(namer.observe(6, ['csrc:9'], tiles)).toEqual([]);
  });

  it('roster merges tiles and the People panel without the user', () => {
    loadFixture('people');
    expect(readTiles(document).map((tile) => tile.name)).toEqual(['Leon Wu', 'Tom Lin']);
    expect(readRoster(document)).toEqual(['Bob Ho', 'Tom Lin']);
  });
});
