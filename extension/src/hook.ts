// MAIN-world content script, run at document_start before Meet's own code.
// It reads who is speaking from WebRTC: each remote participant arrives as a
// stable CSRC on Meet's rotating pool of audio receivers, so the CSRC, not
// the receiver, identifies a participant. Results go to the isolated-world
// content script with window.postMessage, marked `source: "recall"`.

import { SourceLevels, SPEAKING_LEVEL, SpeakingTracker, type RtpSource } from './csrc';

declare global {
  interface Window {
    __recallHooked?: boolean;
  }
}

export const POLL_MS = 100;

/** Minimum gap between two page-internal `audio` heartbeats. */
const AUDIO_HEARTBEAT_MS = 1000;

/** Unix ms, whole: the daemon reads `t` as an integer. */
function wallClock(): number {
  return Math.round(performance.timeOrigin + performance.now());
}

/**
 * Wraps `win.RTCPeerConnection` and starts polling its audio receivers.
 * Returns a function that stops polling (the wrap stays). A second call is a
 * no-op: wrapping twice would mirror every connection, and so every CSRC, twice.
 *
 * Besides speaking events the hook posts `{kind: "audio"}` at most once a
 * second while any remote source carries sound. The content script uses it to
 * tell "nobody spoke" from "speech is not reaching the CSRC reader", and never
 * forwards it.
 */
export function installHook(win: Window & typeof globalThis, now: () => number = wallClock): () => void {
  if (win.__recallHooked) return () => {};
  win.__recallHooked = true;

  const post = (message: Record<string, unknown>) =>
    win.postMessage({ source: 'recall', t: now(), ...message }, '*');
  const mark = (state: string) => {
    const root = win.document?.documentElement;
    // The content script loads later, at document_idle, and reads this to
    // learn about a failure whose message it missed.
    if (root) root.dataset.recallHook = state;
  };

  const connections = new Set<RTCPeerConnection>();
  try {
    const Native = win.RTCPeerConnection;
    if (typeof Native !== 'function') throw new Error('RTCPeerConnection is missing');
    win.RTCPeerConnection = new Proxy(Native, {
      construct(target, args, newTarget) {
        const pc = Reflect.construct(target, args, newTarget) as RTCPeerConnection;
        connections.add(pc);
        return pc;
      },
    });
  } catch {
    mark('failed');
    post({ kind: 'status', status: 'hook_failed' });
    return () => {};
  }
  mark('ok');

  const tracker = new SpeakingTracker();
  const csrcLevels = new SourceLevels();
  const ssrcLevels = new SourceLevels();
  let lastAudio = -Infinity;

  const poll = () => {
    const csrcs: RtpSource[] = [];
    const ssrcs: RtpSource[] = [];
    for (const pc of connections) {
      if (pc.connectionState === 'closed') {
        connections.delete(pc);
        continue;
      }
      try {
        for (const receiver of pc.getReceivers()) {
          if (receiver.track?.kind !== 'audio') continue;
          csrcs.push(...receiver.getContributingSources());
          ssrcs.push(...receiver.getSynchronizationSources());
        }
      } catch {
        // A connection torn down between the state check and the read.
      }
    }

    const t = now();
    const levels = new Map<string, number>();
    for (const [source, level] of csrcLevels.read(csrcs)) levels.set(`csrc:${source}`, level);
    for (const event of tracker.update(t, levels)) post({ ...event });

    const loud = (map: Map<unknown, number>) => [...map.values()].some((l) => l >= SPEAKING_LEVEL);
    const audible = loud(levels) || loud(ssrcLevels.read(ssrcs));
    if (audible && t - lastAudio >= AUDIO_HEARTBEAT_MS) {
      lastAudio = t;
      post({ kind: 'audio' });
    }
  };

  const timer = win.setInterval(poll, POLL_MS);
  return () => win.clearInterval(timer);
}

if (import.meta.env.MODE === 'hook') installHook(window);
