(function() {
  "use strict";
  const SPEAKING_LEVEL = 0.02;
  const HANGOVER_MS = 500;
  class SpeakingTracker {
    /** Speaking participants and the last time each was loud. */
    loudAt = /* @__PURE__ */ new Map();
    update(t, levels) {
      const events = [];
      for (const [participant, level] of levels) {
        if (level < SPEAKING_LEVEL) continue;
        if (!this.loudAt.has(participant)) events.push({ t, kind: "speaking_start", participant });
        this.loudAt.set(participant, t);
      }
      for (const [participant, last] of this.loudAt) {
        if (t - last < HANGOVER_MS) continue;
        events.push({ t: last, kind: "speaking_stop", participant });
        this.loudAt.delete(participant);
      }
      return events;
    }
    get speakingCount() {
      return this.loudAt.size;
    }
  }
  class SourceLevels {
    newest = /* @__PURE__ */ new Map();
    read(sources) {
      const entries = [...sources];
      const newest = /* @__PURE__ */ new Map();
      for (const { source, timestamp } of entries) {
        if (timestamp !== void 0) newest.set(source, Math.max(timestamp, newest.get(source) ?? -Infinity));
      }
      const levels = /* @__PURE__ */ new Map();
      for (const { source, timestamp, audioLevel } of entries) {
        const previous = this.newest.get(source);
        const fresh = timestamp === void 0 || timestamp === newest.get(source) && (previous === void 0 || timestamp > previous);
        const level = fresh ? audioLevel ?? 0 : 0;
        levels.set(source, Math.max(level, levels.get(source) ?? 0));
      }
      this.newest = newest;
      return levels;
    }
  }
  const POLL_MS = 100;
  const AUDIO_HEARTBEAT_MS = 1e3;
  function wallClock() {
    return Math.round(performance.timeOrigin + performance.now());
  }
  function installHook(win, now = wallClock) {
    if (win.__recallHooked) return () => {
    };
    win.__recallHooked = true;
    const post = (message) => win.postMessage({ source: "recall", t: now(), ...message }, "*");
    const mark = (state) => {
      const root = win.document?.documentElement;
      if (root) root.dataset.recallHook = state;
    };
    const connections = /* @__PURE__ */ new Set();
    try {
      const Native = win.RTCPeerConnection;
      if (typeof Native !== "function") throw new Error("RTCPeerConnection is missing");
      win.RTCPeerConnection = new Proxy(Native, {
        construct(target, args, newTarget) {
          const pc = Reflect.construct(target, args, newTarget);
          connections.add(pc);
          return pc;
        }
      });
    } catch {
      mark("failed");
      post({ kind: "status", status: "hook_failed" });
      return () => {
      };
    }
    mark("ok");
    const tracker = new SpeakingTracker();
    const csrcLevels = new SourceLevels();
    const ssrcLevels = new SourceLevels();
    let lastAudio = -Infinity;
    const poll = () => {
      const csrcs = [];
      const ssrcs = [];
      for (const pc of connections) {
        if (pc.connectionState === "closed") {
          connections.delete(pc);
          continue;
        }
        try {
          for (const receiver of pc.getReceivers()) {
            if (receiver.track?.kind !== "audio") continue;
            csrcs.push(...receiver.getContributingSources());
            ssrcs.push(...receiver.getSynchronizationSources());
          }
        } catch {
        }
      }
      const t = now();
      const levels = /* @__PURE__ */ new Map();
      for (const [source, level] of csrcLevels.read(csrcs)) levels.set(`csrc:${source}`, level);
      for (const event of tracker.update(t, levels)) post({ ...event });
      const loud = (map) => [...map.values()].some((l) => l >= SPEAKING_LEVEL);
      const audible = loud(levels) || loud(ssrcLevels.read(ssrcs));
      if (audible && t - lastAudio >= AUDIO_HEARTBEAT_MS) {
        lastAudio = t;
        post({ kind: "audio" });
      }
    };
    const timer = win.setInterval(poll, POLL_MS);
    return () => win.clearInterval(timer);
  }
  installHook(window);
})();
