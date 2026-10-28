// Isolated-world content script. It receives speaking events from the
// MAIN-world hook, names CSRCs from the DOM, reads the roster, watches for a
// lost signal, and hands the last minute of events to the service worker.
// Content scripts never fetch: their requests would carry the
// meet.google.com origin and hit the browser's local-network prompt.

import { GlowFallback, Namer, readRoster, readTiles } from './names';
import { EventQueue, meetingCode, sanitize, type MeetEvent } from './queue';

/** DOM sampling period for name correlation and the glow fallback. */
export const SAMPLE_MS = 200;
export const ROSTER_MS = 10_000;
export const BATCH_MS = 2_000;
/** With remote audio present, this long without a speaking event means the CSRC signal is lost. */
export const SIGNAL_LOST_MS = 300_000;
/** Remote audio counts as present while its last heartbeat is this recent. */
const AUDIO_FRESH_MS = 2_000;

export interface BatchMessage {
  type: 'recall-batch';
  meeting_code: string;
  events: MeetEvent[];
}

export interface ContentDeps {
  win: Window;
  doc: Document;
  /** The current URL path; Meet changes it without a reload when joining from the landing page. */
  pathname: () => string;
  send: (message: BatchMessage) => void;
  now: () => number;
}

/** Starts the content script; the returned function stops its timers and listeners. */
export function startContent({ win, doc, pathname, send, now }: ContentDeps): () => void {
  const queue = new EventQueue();
  const namer = new Namer();
  const glow = new GlowFallback();
  const speaking = new Set<string>();
  let fallback = false;
  let roster: string[] = [];
  let lastSpeaking = now();
  let lastAudio = -Infinity;
  let signalLost = false;

  const sawSpeaking = (t: number) => {
    lastSpeaking = t;
    if (signalLost) {
      signalLost = false;
      queue.push({ t, kind: 'status', status: 'ok' });
    }
  };

  const enterFallback = (t: number) => {
    if (fallback) return;
    fallback = true;
    speaking.clear();
    queue.push({ t, kind: 'status', status: 'hook_failed' });
  };

  const onMessage = (event: MessageEvent) => {
    if (event.source !== win) return;
    const data = event.data as Record<string, unknown> | null;
    if (typeof data !== 'object' || data === null || data.source !== 'recall') return;
    if (data.kind === 'audio') {
      if (typeof data.t === 'number') lastAudio = data.t;
      return;
    }
    const e = sanitize(data);
    if (!e) return;
    if (e.kind === 'status' && e.status === 'hook_failed') {
      enterFallback(e.t);
      return;
    }
    if (fallback) return;
    if (e.kind === 'speaking_start') speaking.add(e.participant);
    if (e.kind === 'speaking_stop') speaking.delete(e.participant);
    if (e.kind === 'speaking_start' || e.kind === 'speaking_stop') sawSpeaking(e.t);
    queue.push(e);
  };

  const sample = () => {
    const t = now();
    const tiles = readTiles(doc);
    if (fallback) {
      for (const e of glow.observe(t, tiles)) {
        if (e.kind !== 'name') sawSpeaking(e.t);
        queue.push(e);
      }
    } else {
      for (const e of namer.observe(t, [...speaking], tiles)) queue.push(e);
    }

    const someoneSpeaking = fallback ? glow.speakingCount > 0 : speaking.size > 0;
    if (
      !signalLost &&
      !someoneSpeaking &&
      t - lastSpeaking >= SIGNAL_LOST_MS &&
      t - lastAudio <= AUDIO_FRESH_MS
    ) {
      signalLost = true;
      queue.push({ t, kind: 'status', status: 'signal_lost' });
    }
  };

  const readRosterNow = () => {
    const names = readRoster(doc);
    if (names.join('\n') === roster.join('\n')) return;
    roster = names;
    queue.push({ t: now(), kind: 'roster', names });
  };

  const flush = () => {
    const code = meetingCode(pathname());
    const events = queue.window(now());
    if (code && events.length > 0) send({ type: 'recall-batch', meeting_code: code, events });
  };

  win.addEventListener('message', onMessage);
  win.addEventListener('pagehide', flush);
  if (doc.documentElement.dataset.recallHook === 'failed') enterFallback(now());
  readRosterNow();
  const timers = [
    win.setInterval(sample, SAMPLE_MS),
    win.setInterval(readRosterNow, ROSTER_MS),
    win.setInterval(flush, BATCH_MS),
  ];
  return () => {
    win.removeEventListener('message', onMessage);
    win.removeEventListener('pagehide', flush);
    for (const timer of timers) win.clearInterval(timer);
  };
}

function sendToWorker(message: BatchMessage): void {
  try {
    // Rejects while the worker is restarting and throws once the extension
    // is reloaded under this page; the next batch resends the same window.
    chrome.runtime.sendMessage(message).catch(() => {});
  } catch {
    // Extension context invalidated.
  }
}

if (import.meta.env.MODE === 'content') {
  startContent({
    win: window,
    doc: document,
    pathname: () => location.pathname,
    send: sendToWorker,
    now: () => performance.timeOrigin + performance.now(),
  });
}
