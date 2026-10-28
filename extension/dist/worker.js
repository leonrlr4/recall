(function() {
  "use strict";
  function sanitize(value) {
    if (typeof value !== "object" || value === null) return null;
    const v = value;
    const t = v.t;
    if (typeof t !== "number" || !Number.isFinite(t)) return null;
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
  const ENDPOINT = "http://127.0.0.1:7474/api/meet/events";
  async function postBatch(batch, fetchFn = fetch) {
    const events = batch.events.map(sanitize).filter((e) => e !== null);
    if (events.length === 0) return true;
    try {
      const response = await fetchFn(ENDPOINT, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ meeting_code: batch.meeting_code, events })
      });
      return response.ok;
    } catch {
      return false;
    }
  }
  function isBatch(message) {
    const m = message;
    return m?.type === "recall-batch" && typeof m.meeting_code === "string" && Array.isArray(m.events);
  }
  {
    chrome.runtime.onMessage.addListener((message, sender) => {
      if (sender.id !== chrome.runtime.id || !isBatch(message)) return;
      void postBatch(message);
    });
  }
})();
