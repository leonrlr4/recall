import { describe, expect, it } from 'vitest';
import { parseTime, splitCitations } from './citations';

const WEEKLY = '2026-10-27 09:00 Weekly sync';

describe('citations', () => {
  it('parses_transcript_times', () => {
    expect(parseTime('12:34')).toBe(754_000);
    expect(parseTime('1:02:03')).toBe(3_723_000);
    expect(parseTime('12:60')).toBeNull();
    expect(parseTime('12')).toBeNull();
  });

  it('links_only_known_meetings', () => {
    const text = `週五發布 [[${WEEKLY} @ 00:03]][[${WEEKLY} @ 1:00:00]]，版面 [[2026-10-27 11:00 Design review @ 00:00]]。`;
    const pieces = splitCitations(text, new Set([WEEKLY]));
    expect(pieces).toEqual([
      { kind: 'text', text: '週五發布 ' },
      { kind: 'cite', id: WEEKLY, ms: 3_000, time: '00:03', raw: `[[${WEEKLY} @ 00:03]]` },
      { kind: 'cite', id: WEEKLY, ms: 3_600_000, time: '1:00:00', raw: `[[${WEEKLY} @ 1:00:00]]` },
      { kind: 'text', text: '，版面 [[2026-10-27 11:00 Design review @ 00:00]]。' },
    ]);
  });

  it('leaves_text_without_citations_alone', () => {
    expect(splitCitations('no cites [[x]]', new Set())).toEqual([
      { kind: 'text', text: 'no cites [[x]]' },
    ]);
    expect(splitCitations('', new Set())).toEqual([]);
  });
});
