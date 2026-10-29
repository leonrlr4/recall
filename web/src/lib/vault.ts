import type { VaultStatus } from './types';

/**
 * What the meeting page says about the vault:
 * - `off`: not published and not asked to be.
 * - `waiting`: switched on; it publishes when processing finishes.
 * - `syncing`: a publish or update is on its way.
 * - `synced`: the vault has the current note.
 * - `removing`: switched off (or deleted) while the note is still in the vault.
 * - `failed`: the last attempt failed; the daemon retries on its own.
 */
export type VaultState = 'off' | 'waiting' | 'syncing' | 'synced' | 'removing' | 'failed';

/** The switch position: a value asked for during processing wins. */
export function syncOn(sync: boolean | undefined, status: VaultStatus): boolean {
  return status.requested ?? sync ?? false;
}

/**
 * The vault state for a meeting whose switch is `on`, given its processing
 * state (`done`, `processing:<step>`, `failed:<step>` or `recording`).
 */
export function vaultState(on: boolean, processing: string, status: VaultStatus): VaultState {
  if (!on) {
    if (!status.path) return 'off';
    return status.error && !status.queued ? 'failed' : 'removing';
  }
  if (status.queued) return 'syncing';
  if (status.error) return 'failed';
  if (processing !== 'done') return 'waiting';
  return status.path ? 'synced' : 'syncing';
}

/** States that change without a page event, so the page polls while in them. */
export function vaultBusy(state: VaultState): boolean {
  return state === 'syncing' || state === 'removing';
}
