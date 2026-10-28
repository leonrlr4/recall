/**
 * Unix ms, whole: the daemon reads `t` as an integer and compares it against
 * its own wall clock. `performance.timeOrigin + performance.now()` would lag
 * after a suspend, because Chrome's monotonic clock stops while the machine
 * sleeps.
 */
export function wallClock(): number {
  return Math.round(Date.now());
}
