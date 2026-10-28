import { describe, expect, it, vi } from 'vitest';
import { ENDPOINT, postBatch } from './worker';

describe('postBatch', () => {
  it('POSTs the batch as JSON with only known fields', async () => {
    const fetchFn = vi.fn().mockResolvedValue(new Response(null, { status: 204 }));
    const ok = await postBatch(
      {
        type: 'recall-batch',
        meeting_code: 'abc-defg-hij',
        events: [{ t: 1, kind: 'speaking_start', participant: 'csrc:1', source: 'recall' } as never],
      },
      fetchFn,
    );
    expect(ok).toBe(true);
    expect(fetchFn).toHaveBeenCalledWith(ENDPOINT, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        meeting_code: 'abc-defg-hij',
        events: [{ t: 1, kind: 'speaking_start', participant: 'csrc:1' }],
      }),
    });
  });

  it('reports a refused connection as a failure without throwing', async () => {
    const fetchFn = vi.fn().mockRejectedValue(new TypeError('Failed to fetch'));
    const batch = {
      type: 'recall-batch' as const,
      meeting_code: 'abc-defg-hij',
      events: [{ t: 1, kind: 'status' as const, status: 'ok' }],
    };
    expect(await postBatch(batch, fetchFn)).toBe(false);
  });
});
