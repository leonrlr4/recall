<script lang="ts">
  import { t } from '../lib/i18n';
  import { live, sendRecord } from '../lib/live.svelte';
  import { recordActions, statusChipText, type RecordAction } from '../lib/status';

  let now = $state(Date.now());
  let busy = $state(false);
  let confirming = $state(false);
  let error = $state<string | null>(null);

  const phase = $derived(live.snapshot?.phase ?? 'idle');
  const chip = $derived(statusChipText(live.snapshot, live.progress, now, t));
  const actions = $derived(recordActions(phase));

  // Tick the elapsed time only while recording.
  $effect(() => {
    if (phase !== 'recording') return;
    now = Date.now();
    const timer = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(timer);
  });

  // A discard prompt left open must not apply to a later recording.
  $effect(() => {
    if (phase !== 'recording') confirming = false;
  });

  async function run(action: RecordAction) {
    busy = true;
    error = null;
    try {
      await sendRecord(action);
      confirming = false;
    } catch (e) {
      error = t('status.failed', { error: e instanceof Error ? e.message : String(e) });
    } finally {
      busy = false;
    }
  }

  const label: Record<RecordAction, string> = $derived({
    start: t('status.start'),
    stop: t('status.stop'),
    discard: t('status.discard'),
  });
</script>

<div class="status" role="group" aria-label={t('status.label')}>
  <span class="chip {chip.tone}" aria-live="polite">
    <span class="dot" aria-hidden="true"></span>
    {chip.text}
  </span>
  {#if confirming}
    <span class="confirm" role="alert">
      {t('status.discard_confirm')}
      <button class="btn danger" disabled={busy} onclick={() => run('discard')}>
        {t('status.discard_yes')}
      </button>
      <button class="btn" disabled={busy} onclick={() => (confirming = false)}>
        {t('common.cancel')}
      </button>
    </span>
  {:else}
    {#each actions as action (action)}
      <button
        class="btn"
        class:primary={action === 'start'}
        disabled={busy}
        onclick={() => (action === 'discard' ? (confirming = true) : run(action))}
      >
        {label[action]}
      </button>
    {/each}
  {/if}
  {#if error}
    <span class="error" role="alert" title={error}>{error}</span>
  {/if}
</div>

<style>
  .status {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-left: auto;
    min-width: 0;
    font-size: 13px;
  }

  .chip {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 2px 12px;
    border-radius: 999px;
    font-weight: 500;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }

  .dot {
    flex: none;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: currentColor;
  }

  .idle {
    color: var(--text-2);
  }

  .idle .dot {
    width: 6px;
    height: 6px;
    opacity: 0.6;
  }

  .armed {
    background: var(--orange-soft);
    color: var(--orange);
  }

  .recording {
    background: var(--danger-soft);
    color: var(--live);
  }

  .recording .dot {
    animation: pulse 1.6s ease-in-out infinite;
  }

  .finishing {
    background: var(--surface-2);
    color: var(--text-2);
  }

  .processing {
    background: var(--accent-soft);
    color: var(--accent);
  }

  .confirm {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    white-space: nowrap;
  }

  .btn {
    flex: none;
    height: 26px;
    padding: 0 10px;
    font-size: 13px;
  }

  .error {
    max-width: 240px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--danger);
  }

  @keyframes pulse {
    50% {
      opacity: 0.35;
    }
  }
</style>
