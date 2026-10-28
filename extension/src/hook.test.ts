import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { installHook } from './hook';

interface FakeSource {
  source: number;
  audioLevel: number;
  timestamp: number;
}

class FakeReceiver {
  csrcs: FakeSource[] = [];
  track = { kind: 'audio' };
  getContributingSources(): FakeSource[] {
    // Each poll sees fresh packets, as during a live call.
    for (const s of this.csrcs) s.timestamp += 100;
    return this.csrcs.map((s) => ({ ...s }));
  }
  getSynchronizationSources(): FakeSource[] {
    return [];
  }
}

class FakePeerConnection {
  connectionState = 'connected';
  receivers: FakeReceiver[] = [];
  getReceivers(): FakeReceiver[] {
    return this.receivers;
  }
}

type Posted = Record<string, unknown>;

let posted: Posted[];
let stops: (() => void)[];

function install(): void {
  stops.push(installHook(window, () => Date.now()));
}

function speakingEvents(): Posted[] {
  return posted.filter((m) => m.kind === 'speaking_start' || m.kind === 'speaking_stop');
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(1_000_000);
  posted = [];
  stops = [];
  vi.spyOn(window, 'postMessage').mockImplementation((message: unknown) => {
    posted.push(message as Posted);
  });
  (window as unknown as { RTCPeerConnection: unknown }).RTCPeerConnection = FakePeerConnection;
  delete window.__recallHooked;
  delete document.documentElement.dataset.recallHook;
});

afterEach(() => {
  for (const stop of stops) stop();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

function connectWith(level: number): FakeSource {
  const pc = new window.RTCPeerConnection() as unknown as FakePeerConnection;
  const receiver = new FakeReceiver();
  const source = { source: 12345, audioLevel: level, timestamp: 0 };
  receiver.csrcs.push(source);
  pc.receivers.push(receiver);
  return source;
}

describe('installHook', () => {
  it('hook polls csrc and emits start/stop with 500 ms hangover', () => {
    install();
    const source = connectWith(0.3);

    vi.advanceTimersByTime(100);
    expect(speakingEvents()).toEqual([
      { source: 'recall', t: 1_000_100, kind: 'speaking_start', participant: 'csrc:12345' },
    ]);

    // Last loud poll at 1_000_300; a quiet stretch shorter than 500 ms keeps it speaking.
    vi.advanceTimersByTime(200);
    source.audioLevel = 0.01;
    vi.advanceTimersByTime(400);
    expect(speakingEvents()).toHaveLength(1);

    vi.advanceTimersByTime(100);
    expect(speakingEvents()).toEqual([
      { source: 'recall', t: 1_000_100, kind: 'speaking_start', participant: 'csrc:12345' },
      { source: 'recall', t: 1_000_300, kind: 'speaking_stop', participant: 'csrc:12345' },
    ]);
  });

  it('hook wraps only once', () => {
    install();
    const wrapped = window.RTCPeerConnection;
    install();
    expect(window.RTCPeerConnection).toBe(wrapped);

    connectWith(0.3);
    vi.advanceTimersByTime(100);
    expect(speakingEvents()).toHaveLength(1);
  });

  it('reports remote audio separately from speaking events', () => {
    install();
    connectWith(0.3);
    vi.advanceTimersByTime(1000);
    const audio = posted.filter((m) => m.kind === 'audio');
    expect(audio.length).toBeGreaterThan(0);
    expect(audio.length).toBeLessThanOrEqual(2);
  });

  it('reports hook_failed when RTCPeerConnection cannot be wrapped', () => {
    delete (window as unknown as { RTCPeerConnection?: unknown }).RTCPeerConnection;
    install();
    expect(posted).toEqual([{ source: 'recall', t: 1_000_000, kind: 'status', status: 'hook_failed' }]);
    expect(document.documentElement.dataset.recallHook).toBe('failed');
  });
});
