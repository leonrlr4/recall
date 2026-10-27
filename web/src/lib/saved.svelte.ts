// What this browser saved to the config file since the page loaded.
// `GET /api/config` keeps returning the running config until the daemon
// restarts, so the settings page lays this over it; kept at module level so
// it survives navigating away from the page.
import type { ConfigPatch } from './types';

export const savedConfig = $state<{ patch: ConfigPatch; restartNeeded: boolean }>({
  patch: {},
  restartNeeded: false,
});
