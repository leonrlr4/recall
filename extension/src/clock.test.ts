import { afterEach, expect, it, vi } from 'vitest';
import { wallClock } from './clock';

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

it('reads wall-clock time, not the monotonic page clock', () => {
  // performance.now() stops while the machine sleeps, so after a resume
  // timeOrigin + now() lags the wall clock the daemon compares `t` against.
  vi.useFakeTimers();
  vi.setSystemTime(1_000_000);
  vi.spyOn(performance, 'now').mockReturnValue(0);
  expect(wallClock()).toBe(1_000_000);
});
