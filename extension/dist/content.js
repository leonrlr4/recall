(function() {
  "use strict";
  function wallClock() {
    return Math.round(Date.now());
  }
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
  const SELECTORS = {
    /** A participant tile in the call grid. */
    tile: "div[data-participant-id]",
    /** The participant's display name inside a tile or People panel row. */
    name: "span.notranslate",
    /** Marks the user's own tile. */
    self: "[data-self-name]",
    /** Classes of the speaking glow; a tile glows when any element in it has one. */
    glow: ["Oaajhc", "HX2H7", "wEsLMd", "OgVli"],
    /** A row of the People panel. It also carries data-participant-id, so it is not a tile. */
    panelItem: '[role="listitem"][data-participant-id]'
  };
  function nameIn(el) {
    const text = el.querySelector(SELECTORS.name)?.textContent?.trim();
    return text ? text : null;
  }
  const glowSelector = SELECTORS.glow.map((c) => `.${c}`).join(",");
  function readTiles(root) {
    const tiles = [];
    for (const el of root.querySelectorAll(SELECTORS.tile)) {
      if (el.matches(SELECTORS.panelItem)) continue;
      if (el.parentElement?.closest(SELECTORS.tile)) continue;
      tiles.push({
        id: el.getAttribute("data-participant-id") ?? "",
        name: nameIn(el),
        self: el.matches(SELECTORS.self) || el.querySelector(SELECTORS.self) !== null,
        glowing: el.matches(glowSelector) || el.querySelector(glowSelector) !== null
      });
    }
    return tiles;
  }
  function readRoster(root) {
    const tiles = readTiles(root);
    const selfIds = new Set(tiles.filter((t) => t.self).map((t) => t.id));
    const names = new Set(tiles.filter((t) => !t.self && t.name).map((t) => t.name));
    for (const row of root.querySelectorAll(SELECTORS.panelItem)) {
      if (selfIds.has(row.getAttribute("data-participant-id") ?? "")) continue;
      const name = nameIn(row);
      if (name) names.add(name);
    }
    return [...names].sort();
  }
  function glowingNames(tiles) {
    return tiles.filter((t) => t.glowing && !t.self && t.name).map((t) => t.name);
  }
  class Counts {
    counts = /* @__PURE__ */ new Map();
    add(sample) {
      if (sample.speaking.length !== 1 || sample.glowing.length !== 1) return null;
      const [participant] = sample.speaking;
      const [name] = sample.glowing;
      const byName = this.counts.get(participant) ?? /* @__PURE__ */ new Map();
      byName.set(name, (byName.get(name) ?? 0) + 1);
      this.counts.set(participant, byName);
      return participant;
    }
    /** The most counted name; on a tie, `current` wins if it is among the leaders. */
    best(participant, current) {
      let best;
      let max = 0;
      for (const [name, n] of this.counts.get(participant) ?? []) {
        if (n > max || n === max && name === current) {
          best = name;
          max = n;
        }
      }
      return best;
    }
    participants() {
      return [...this.counts.keys()];
    }
  }
  class Namer {
    counts = new Counts();
    names = /* @__PURE__ */ new Map();
    observe(t, speaking, tiles) {
      const participant = this.counts.add({ speaking, glowing: glowingNames(tiles) });
      if (participant === null) return [];
      const current = this.names.get(participant);
      const name = this.counts.best(participant, current);
      if (!name || name === current) return [];
      this.names.set(participant, name);
      return [{ t, kind: "name", participant, name }];
    }
    /** A `name` event at `t` for every participant that has a name. */
    current(t) {
      return [...this.names].map(([participant, name]) => ({ t, kind: "name", participant, name }));
    }
  }
  class GlowFallback {
    tracker = new SpeakingTracker();
    names = /* @__PURE__ */ new Map();
    observe(t, tiles) {
      const events = [];
      const levels = /* @__PURE__ */ new Map();
      for (const tile of tiles) {
        if (tile.self || !tile.glowing) continue;
        const participant = `tile:${tile.id}`;
        levels.set(participant, 1);
        if (tile.name && this.names.get(participant) !== tile.name) {
          this.names.set(participant, tile.name);
          events.push({ t, kind: "name", participant, name: tile.name });
        }
      }
      return [...this.tracker.update(t, levels), ...events];
    }
    /** A `name` event at `t` for every tile participant seen so far. */
    current(t) {
      return [...this.names].map(([participant, name]) => ({ t, kind: "name", participant, name }));
    }
    get speakingCount() {
      return this.tracker.speakingCount;
    }
  }
  const RESEND_WINDOW_MS = 6e4;
  class EventQueue {
    events = [];
    push(event) {
      this.events.push(event);
    }
    /** The events of the last `RESEND_WINDOW_MS`; older ones are dropped for good. */
    window(now) {
      this.events = this.events.filter((e) => now - e.t <= RESEND_WINDOW_MS);
      return [...this.events];
    }
  }
  function meetingCode(pathname) {
    return /^\/([a-z]{3}-[a-z]{4}-[a-z]{3})(?:\/|$)/.exec(pathname)?.[1] ?? null;
  }
  function sanitize(value) {
    if (typeof value !== "object" || value === null) return null;
    const v = value;
    if (typeof v.t !== "number" || !Number.isFinite(v.t)) return null;
    const t = Math.round(v.t);
    switch (v.kind) {
      case "speaking_start":
      case "speaking_stop":
        return typeof v.participant === "string" ? { t, kind: v.kind, participant: v.participant } : null;
      case "name":
        return typeof v.participant === "string" && typeof v.name === "string" ? { t, kind: "name", participant: v.participant, name: v.name } : null;
      case "roster":
        return Array.isArray(v.names) && v.names.every((n) => typeof n === "string") ? { t, kind: "roster", names: [...v.names] } : null;
      case "status":
        return typeof v.status === "string" ? { t, kind: "status", status: v.status } : null;
      default:
        return null;
    }
  }
  const SAMPLE_MS = 200;
  const ROSTER_MS = 1e4;
  const BATCH_MS = 2e3;
  const REFRESH_MS = 3e4;
  const SIGNAL_LOST_MS = 3e5;
  const AUDIO_FRESH_MS = 2e3;
  function startContent({ win, doc, pathname, send, now: clock }) {
    const now = () => Math.round(clock());
    const queue = new EventQueue();
    const namer = new Namer();
    const glow = new GlowFallback();
    const speaking = /* @__PURE__ */ new Set();
    let fallback = false;
    let roster = [];
    let lastSpeaking = now();
    let lastAudio = -Infinity;
    let signalLost = false;
    const sawSpeaking = (t) => {
      lastSpeaking = t;
      if (signalLost) {
        signalLost = false;
        queue.push({ t, kind: "status", status: "ok" });
      }
    };
    const enterFallback = (t) => {
      if (fallback) return;
      fallback = true;
      speaking.clear();
      queue.push({ t, kind: "status", status: "hook_failed" });
    };
    const onMessage = (event) => {
      if (event.source !== win) return;
      const data = event.data;
      if (typeof data !== "object" || data === null || data.source !== "recall") return;
      if (data.kind === "audio") {
        if (typeof data.t === "number") lastAudio = data.t;
        return;
      }
      const e = sanitize(data);
      if (!e) return;
      if (e.kind === "status" && e.status === "hook_failed") {
        enterFallback(e.t);
        return;
      }
      if (fallback) return;
      if (e.kind === "speaking_start") speaking.add(e.participant);
      if (e.kind === "speaking_stop") speaking.delete(e.participant);
      if (e.kind === "speaking_start" || e.kind === "speaking_stop") sawSpeaking(e.t);
      queue.push(e);
    };
    const sample = () => {
      const t = now();
      const tiles = readTiles(doc);
      if (fallback) {
        for (const e of glow.observe(t, tiles)) {
          if (e.kind !== "name") sawSpeaking(e.t);
          queue.push(e);
        }
      } else {
        for (const e of namer.observe(t, [...speaking], tiles)) queue.push(e);
      }
      const someoneSpeaking = fallback ? glow.speakingCount > 0 : speaking.size > 0;
      if (!signalLost && !someoneSpeaking && t - lastSpeaking >= SIGNAL_LOST_MS && t - lastAudio <= AUDIO_FRESH_MS) {
        signalLost = true;
        queue.push({ t, kind: "status", status: "signal_lost" });
      }
    };
    const readRosterNow = () => {
      const names = readRoster(doc);
      if (names.join("\n") === roster.join("\n")) return;
      roster = names;
      queue.push({ t: now(), kind: "roster", names });
    };
    const refresh = () => {
      const t = now();
      for (const e of fallback ? glow.current(t) : namer.current(t)) queue.push(e);
      if (roster.length > 0) queue.push({ t, kind: "roster", names: roster });
      if (fallback) queue.push({ t, kind: "status", status: "hook_failed" });
      else if (signalLost) queue.push({ t, kind: "status", status: "signal_lost" });
    };
    const flush = () => {
      const code = meetingCode(pathname());
      const events = queue.window(now());
      if (code && events.length > 0) send({ type: "recall-batch", meeting_code: code, events });
    };
    win.addEventListener("message", onMessage);
    win.addEventListener("pagehide", flush);
    if (doc.documentElement.dataset.recallHook === "failed") enterFallback(now());
    readRosterNow();
    const timers = [
      win.setInterval(sample, SAMPLE_MS),
      win.setInterval(readRosterNow, ROSTER_MS),
      win.setInterval(flush, BATCH_MS),
      win.setInterval(refresh, REFRESH_MS)
    ];
    return () => {
      win.removeEventListener("message", onMessage);
      win.removeEventListener("pagehide", flush);
      for (const timer of timers) win.clearInterval(timer);
    };
  }
  function sendToWorker(message) {
    try {
      chrome.runtime.sendMessage(message).catch(() => {
      });
    } catch {
    }
  }
  {
    startContent({
      win: window,
      doc: document,
      pathname: () => location.pathname,
      send: sendToWorker,
      now: wallClock
    });
  }
})();
