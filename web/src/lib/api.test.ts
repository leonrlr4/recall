import { describe, expect, it } from 'vitest';
import { ApiError, ask, audioUrl, listUrl, meetingUrl, parseSse, putConfig } from './api';

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

function sseResponse(body: string): Response {
  return new Response(body, { headers: { 'content-type': 'text/event-stream' } });
}

describe('ask', () => {
  it('ask_streams_tokens_until_done', async () => {
    const orig = globalThis.fetch;
    globalThis.fetch = async () => sseResponse('data: Hel\n\ndata: lo\n\nevent: done\ndata:\n\n');
    try {
      const tokens: string[] = [];
      await ask('m', 'q', (t) => tokens.push(t));
      expect(tokens.join('')).toBe('Hello');
    } finally {
      globalThis.fetch = orig;
    }
  });

  it('ask_throws_on_error_event', async () => {
    const orig = globalThis.fetch;
    globalThis.fetch = async () =>
      sseResponse('data: par\n\nevent: error\ndata: model not found\n\nevent: done\ndata:\n\n');
    try {
      const tokens: string[] = [];
      const err = await ask('m', 'q', (t) => tokens.push(t)).catch((e) => e);
      expect(err).toBeInstanceOf(ApiError);
      expect(err.message).toBe('model not found');
      expect(tokens).toEqual(['par']);
    } finally {
      globalThis.fetch = orig;
    }
  });

  it('sse_parser_reports_error_events', () => {
    const p = parseSse();
    expect(p.push('event: error\ndata: boom\n\n')).toEqual([{ event: 'error', data: 'boom' }]);
  });
});

describe('config', () => {
  it('api_put_config_returns_restart_flag', async () => {
    const orig = globalThis.fetch;
    const calls: { url: string; init?: RequestInit }[] = [];
    globalThis.fetch = async (url, init) => {
      calls.push({ url: String(url), init });
      return Response.json({ restart_required: true });
    };
    try {
      const res = await putConfig({ port: 8000, llm: { model: 'llama' } });
      expect(res).toEqual({ restart_required: true });
      expect(calls).toHaveLength(1);
      expect(calls[0].url).toBe('/api/config');
      expect(calls[0].init?.method).toBe('PUT');
      expect(JSON.parse(String(calls[0].init?.body))).toEqual({
        port: 8000,
        llm: { model: 'llama' },
      });
    } finally {
      globalThis.fetch = orig;
    }
  });

  it('api_errors_carry_the_server_error_message', async () => {
    const orig = globalThis.fetch;
    globalThis.fetch = async () => Response.json({ error: 'port must not be 0' }, { status: 400 });
    try {
      const err = await putConfig({ port: 0 }).catch((e) => e);
      expect(err).toBeInstanceOf(ApiError);
      expect(err.status).toBe(400);
      expect(err.message).toBe('port must not be 0');
    } finally {
      globalThis.fetch = orig;
    }
  });
});
