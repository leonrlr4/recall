// Background service worker: POSTs the content script's batches to the local
// daemon. It holds no state, because MV3 may stop it at any time; the content
// script resends the last minute of events with every batch instead.

import type { BatchMessage } from './content';
import { sanitize, type MeetEvent } from './queue';

export const ENDPOINT = 'http://127.0.0.1:7474/api/meet/events';

/** POSTs one batch; resolves to whether the daemon accepted it. Never rejects. */
export async function postBatch(batch: BatchMessage, fetchFn: typeof fetch = fetch): Promise<boolean> {
  const events = batch.events.map(sanitize).filter((e): e is MeetEvent => e !== null);
  if (events.length === 0) return true;
  try {
    const response = await fetchFn(ENDPOINT, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ meeting_code: batch.meeting_code, events }),
    });
    return response.ok;
  } catch {
    // The daemon is not running; the next batch carries the same events.
    return false;
  }
}

function isBatch(message: unknown): message is BatchMessage {
  const m = message as Partial<BatchMessage> | null;
  return m?.type === 'recall-batch' && typeof m.meeting_code === 'string' && Array.isArray(m.events);
}

if (import.meta.env.MODE === 'worker') {
  chrome.runtime.onMessage.addListener((message, sender) => {
    if (sender.id !== chrome.runtime.id || !isBatch(message)) return;
    void postBatch(message);
  });
}
