import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { startContent, type BatchMessage } from './content';
import { loadFixture } from './fixture';
import type { MeetEvent } from './queue';

let sent: BatchMessage[];
let stop: () => void;

function start(): void {
  stop = startContent({
    win: window,
    doc: document,
    pathname: () => '/abc-defg-hij',
    send: (message) => sent.push(message),
    now: () => Date.now(),
  });
}

function fromHook(data: Record<string, unknown>): void {
  window.dispatchEvent(
    new MessageEvent('message', { data: { source: 'recall', t: Date.now(), ...data }, source: window }),
  );
}

/** Every distinct event the worker has been handed so far. */
function delivered(): MeetEvent[] {
  const seen = new Map<string, MeetEvent>();
  for (const batch of sent) for (const e of batch.events) seen.set(JSON.stringify(e), e);
  return [...seen.values()];
}

function statuses(): string[] {
  return delivered().flatMap((e) => (e.kind === 'status' ? [e.status] : []));
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(1_000_000);
  sent = [];
  delete document.documentElement.dataset.recallHook;
});

afterEach(() => {
  stop();
  vi.useRealTimers();
});

describe('startContent', () => {
  it('signal_lost after 300 s of silence with remote audio', () => {
    loadFixture('grid');
    start();
    for (let i = 0; i < 299; i++) {
      fromHook({ kind: 'audio' });
      vi.advanceTimersByTime(1000);
    }
    expect(statuses()).toEqual([]);

    fromHook({ kind: 'audio' });
    vi.advanceTimersByTime(3000);
    expect(statuses()).toEqual(['signal_lost']);

    // A speaking event clears it.
    fromHook({ kind: 'speaking_start', participant: 'csrc:1' });
    vi.advanceTimersByTime(2000);
    expect(statuses()).toEqual(['signal_lost', 'ok']);
  });

  it('stays quiet through 300 s of silence without remote audio', () => {
    loadFixture('grid');
    start();
    vi.advanceTimersByTime(400_000);
    expect(statuses()).toEqual([]);
  });

  it('forwards hook events without the page-internal marker and names the speaker', () => {
    loadFixture('speaking');
    start();
    fromHook({ kind: 'speaking_start', participant: 'csrc:12345' });
    vi.advanceTimersByTime(2000);
    const events = delivered();
    expect(events).toContainEqual({ t: 1_000_000, kind: 'speaking_start', participant: 'csrc:12345' });
    expect(events).toContainEqual(
      expect.objectContaining({ kind: 'name', participant: 'csrc:12345', name: 'Amy Chen' }),
    );
    expect(events).toContainEqual({ t: 1_000_000, kind: 'roster', names: ['Amy Chen', 'Tom Lin'] });
    expect(events.some((e) => 'source' in e)).toBe(false);
    expect(sent.every((batch) => batch.meeting_code === 'abc-defg-hij')).toBe(true);
  });

  it('glow fallback after hook_failed', () => {
    loadFixture('grid');
    start();
    fromHook({ kind: 'status', status: 'hook_failed' });
    const ring = document.querySelector('[data-participant-id="spaces/AAA/devices/2"] .ring')!;
    ring.classList.add('wEsLMd');
    vi.advanceTimersByTime(1000);
    ring.classList.remove('wEsLMd');
    vi.advanceTimersByTime(1000);

    const events = delivered().filter((e) => e.kind !== 'roster');
    expect(events.map((e) => e.kind)).toEqual(['status', 'speaking_start', 'name', 'speaking_stop']);
    expect(events[0]).toMatchObject({ status: 'hook_failed' });
    expect(events[1]).toMatchObject({ participant: 'tile:spaces/AAA/devices/2' });
    expect(events[2]).toMatchObject({ participant: 'tile:spaces/AAA/devices/2', name: 'Tom Lin' });
    expect(events[3]).toMatchObject({ participant: 'tile:spaces/AAA/devices/2' });
  });

  it('falls back to the glow when the hook failed before the content script loaded', () => {
    loadFixture('grid');
    document.documentElement.dataset.recallHook = 'failed';
    start();
    vi.advanceTimersByTime(2000);
    expect(statuses()).toEqual(['hook_failed']);
  });

  it('flushes on page hide', () => {
    loadFixture('grid');
    start();
    fromHook({ kind: 'speaking_start', participant: 'csrc:1' });
    window.dispatchEvent(new Event('pagehide'));
    expect(sent).toHaveLength(1);
  });
});
