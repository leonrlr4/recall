import { describe, expect, it } from 'vitest';
import type { VaultStatus } from './types';
import { syncOn, vaultBusy, vaultState } from './vault';

const none: VaultStatus = { requested: null, queued: false, path: null, synced_at: null, error: null };
const published: VaultStatus = {
  ...none,
  path: 'meetings/2026-10-29 12.37 Weekly sync.md',
  synced_at: '2026-10-29T05:00:00Z',
};

describe('vault', () => {
  it('sync_on_prefers_a_pending_request', () => {
    expect(syncOn(undefined, none)).toBe(false);
    expect(syncOn(true, none)).toBe(true);
    expect(syncOn(false, { ...none, requested: true })).toBe(true);
    expect(syncOn(true, { ...none, requested: false })).toBe(false);
  });

  it('vault_state_when_switched_on', () => {
    expect(vaultState(true, 'processing:transcribe', none)).toBe('waiting');
    expect(vaultState(true, 'done', { ...none, queued: true })).toBe('syncing');
    expect(vaultState(true, 'done', none)).toBe('syncing');
    expect(vaultState(true, 'done', published)).toBe('synced');
    expect(vaultState(true, 'done', { ...published, error: 'push rejected' })).toBe('failed');
    // A rerun keeps the published note until it finishes.
    expect(vaultState(true, 'processing:summarize', published)).toBe('waiting');
  });

  it('vault_state_when_switched_off', () => {
    expect(vaultState(false, 'done', none)).toBe('off');
    expect(vaultState(false, 'done', { ...published, queued: true })).toBe('removing');
    expect(vaultState(false, 'done', { ...published, error: 'offline' })).toBe('failed');
  });

  it('vault_busy_states_poll', () => {
    expect(vaultBusy('syncing')).toBe(true);
    expect(vaultBusy('removing')).toBe(true);
    expect(vaultBusy('synced')).toBe(false);
    expect(vaultBusy('failed')).toBe(false);
  });
});
