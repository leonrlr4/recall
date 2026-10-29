<script lang="ts">
  import { locale, t } from '../lib/i18n';
  import type { VaultStatus } from '../lib/types';
  import { vaultState } from '../lib/vault';

  let {
    on,
    processing,
    status,
    busy = false,
    onchange,
  }: {
    /** The switch position, a pending request included. */
    on: boolean;
    /** The meeting's processing state, e.g. `done` or `processing:transcribe`. */
    processing: string;
    status: VaultStatus;
    /** A switch PATCH is in flight. */
    busy?: boolean;
    onchange: (on: boolean) => void;
  } = $props();

  const state = $derived(vaultState(on, processing, status));
  const syncedAt = $derived(
    status.synced_at
      ? new Date(status.synced_at).toLocaleString(locale(), { dateStyle: 'medium', timeStyle: 'short' })
      : '',
  );
  const line = $derived.by(() => {
    switch (state) {
      case 'waiting':
        return t('vault.waiting');
      case 'syncing':
        return t('vault.syncing');
      case 'synced':
        return t('vault.synced', { time: syncedAt });
      case 'removing':
        return t('vault.removing');
      case 'failed':
        return t(on ? 'vault.failed' : 'vault.remove_failed', { error: status.error ?? '' });
      default:
        return '';
    }
  });

  function toggle(e: Event) {
    const next = (e.currentTarget as HTMLInputElement).checked;
    // Turning it off takes the note out of the vault, but every earlier
    // version stays in the vault's git history.
    if (!next && status.path && !confirm(t('vault.off_confirm'))) {
      (e.currentTarget as HTMLInputElement).checked = true;
      return;
    }
    onchange(next);
  }
</script>

<section class="vault panel" aria-label={t('vault.label')}>
  <label class="switch">
    <input type="checkbox" role="switch" checked={on} disabled={busy} onchange={toggle} aria-describedby="vault-help" />
    <span class="track" aria-hidden="true"><span class="thumb"></span></span>
    <span class="label">{t('vault.label')}</span>
  </label>
  <p id="vault-help" class="help">{on ? t('vault.help_on') : t('vault.help_off')}</p>
  <div aria-live="polite">
    {#if line}
      <p class="state {state}" title={status.path ?? undefined}>
        {#if state === 'syncing' || state === 'removing'}
          <span class="dot" aria-hidden="true"></span>
        {/if}
        {line}
      </p>
    {/if}
  </div>
</section>

<style>
  .vault {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 12px 16px;
  }

  .switch {
    position: relative;
    display: inline-flex;
    align-items: center;
    gap: 10px;
    cursor: pointer;
  }

  .switch input {
    position: absolute;
    opacity: 0;
    width: 1px;
    height: 1px;
  }

  .track {
    position: relative;
    flex: none;
    width: 32px;
    height: 18px;
    border-radius: 999px;
    background: var(--border-strong);
    transition: background 120ms;
  }

  .thumb {
    position: absolute;
    top: 2px;
    left: 2px;
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: var(--surface);
    box-shadow: var(--shadow);
    transition: transform 120ms;
  }

  .switch input:checked + .track {
    background: var(--accent);
  }

  .switch input:checked + .track .thumb {
    transform: translateX(14px);
  }

  .switch input:focus-visible + .track {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .switch input:disabled + .track {
    opacity: 0.6;
  }

  .label {
    font-size: 14px;
    font-weight: 500;
  }

  .help {
    margin: 0;
    color: var(--text-3);
    font-size: 12px;
  }

  .state {
    display: flex;
    align-items: center;
    gap: 6px;
    margin: 0;
    font-size: 13px;
    color: var(--text-2);
    overflow-wrap: anywhere;
  }

  .state.synced {
    color: var(--ok);
  }

  .state.failed {
    color: var(--danger);
  }

  .dot {
    flex: none;
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--accent);
    animation: pulse 1.2s ease-in-out infinite;
  }

  @keyframes pulse {
    50% {
      opacity: 0.3;
    }
  }
</style>
