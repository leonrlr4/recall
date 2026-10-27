import { describe, expect, it } from 'vitest';
import { audioUrl, listUrl, meetingUrl, parseSse } from './api';

describe('api', () => {
  it('api_list_builds_query_string', () => {
    expect(listUrl({})).toBe('/api/meetings');
    expect(listUrl({ q: '', app: undefined })).toBe('/api/meetings');
    expect(listUrl({ q: '預算 review', app: 'Google Meet' })).toBe(
      '/api/meetings?q=%E9%A0%90%E7%AE%97+review&app=Google+Meet',
    );
    expect(
      listUrl({ participant: 'Ann', from: '2026-10-01T00:00:00Z', to: '2026-11-01T00:00:00Z' }),
    ).toBe(
      '/api/meetings?participant=Ann&from=2026-10-01T00%3A00%3A00Z&to=2026-11-01T00%3A00%3A00Z',
    );
  });

  it('meeting_ids_are_path_encoded', () => {
    expect(meetingUrl('2026-10-27 14:30 Biobuild weekly')).toBe(
      '/api/meetings/2026-10-27%2014%3A30%20Biobuild%20weekly',
    );
    expect(audioUrl('a/b')).toBe('/api/meetings/a%2Fb/audio');
  });

  it('sse_parser_splits_data_and_events_across_chunks', () => {
    const p = parseSse();
    expect(p.push('data: Hel')).toEqual([]);
    expect(p.push('lo\n\ndata: wor')).toEqual([{ event: 'message', data: 'Hello' }]);
    expect(p.push('ld\r\n\r\nevent: done\ndata:\n\n')).toEqual([
      { event: 'message', data: 'world' },
      { event: 'done', data: '' },
    ]);
  });

  it('sse_parser_joins_multiline_data', () => {
    const p = parseSse();
    expect(p.push('data: a\ndata: b\n\n')).toEqual([{ event: 'message', data: 'a\nb' }]);
  });
});
