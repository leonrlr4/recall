<script lang="ts">
  import { tick, untrack } from 'svelte';
  import { t } from '../lib/i18n';
  import { live, sendRecord } from '../lib/live.svelte';
  import {
    canStopScreen,
    recordActions,
    statusAnnouncement,
    statusChipText,
    statusNotes,
    type RecordAction,
  } from '../lib/status';

  let now = $state(Date.now());
  let busy = $state(false);
  let confirming = $state(false);
  let error = $state<string | null>(null);
  let group: HTMLDivElement;
  let cancelButton = $state<HTMLButtonElement>();
  const buttons: Partial<Record<RecordAction, HTMLButtonElement>> = $state({});

  const phase = $derived(live.snapshot?.phase ?? 'idle');
  const chip = $derived(statusChipText(live.snapshot, live.progress, now, t));
  const actions = $derived(recordActions(phase));
  const announcement = $derived(statusAnnouncement(live.snapshot, live.progress, t));
  const notes = $derived(statusNotes(live.snapshot, t));
  const stopScreen = $derived(canStopScreen(live.snapshot));

  // Tick the elapsed time only while recording.
  $effect(() => {
    if (phase !== 'recording') return;
    now = Date.now();
    const timer = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(timer);
  });

  // A discard prompt left open must not apply to a later recording, and an
  // error from one phase says nothing about the next.
  let lastPhase = untrack(() => phase);
  $effect(() => {
    if (phase === lastPhase) return;
    lastPhase = phase;
    error = null;
    if (confirming) void closeConfirm();
  });

  $effect(() => {
    if (confirming) cancelButton?.focus();
  });

  /** Closes the discard prompt and puts focus back on a record button. */
  async function closeConfirm() {
    const hadFocus = group.contains(document.activeElement);
    confirming = false;
    await tick();
    // The prompt's button was removed with it, so focus fell to the body.
    const lost = hadFocus || document.activeElement === document.body;
    if (lost) (buttons.discard ?? buttons.stop ?? buttons.start)?.focus();
  }

  function confirmKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      void closeConfirm();
    }
  }

  async function run(action: RecordAction | 'screen_off') {
    busy = true;
    error = null;
    try {
      await sendRecord(action);
      if (confirming) await closeConfirm();
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

<div class="status" role="group" aria-label={t('status.label')} bind:this={group}>
  <!-- The visible chip ticks every second; this region holds only the phase. -->
  <span class="sr-only" aria-live="polite">{announcement}</span>
  <span class="chip {chip.tone}" aria-hidden="true">
    <span class="dot" aria-hidden="true"></span>
    {chip.text}
  </span>
  {#each notes as note (note.text)}
    <span class="note {note.tone}" role="status" title={note.text}>{note.text}</span>
  {/each}
  {#if confirming}
    <!-- Escape from either button cancels; the listener sits on their container. -->
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <span class="confirm" role="alert" onkeydown={confirmKeydown}>
      {t('status.discard_confirm')}
      <button class="btn danger" disabled={busy} onclick={() => run('discard')}>
        {t('status.discard_yes')}
      </button>
      <button class="btn" disabled={busy} bind:this={cancelButton} onclick={closeConfirm}>
        {t('common.cancel')}
      </button>
    </span>
  {:else}
    {#each actions as action (action)}
      <button
        class="btn"
        class:primary={action === 'start'}
        disabled={busy}
        bind:this={buttons[action]}
        onclick={() => (action === 'discard' ? (confirming = true) : run(action))}
      >
        {label[action]}
      </button>
    {/each}
    {#if stopScreen}
      <button
        class="btn"
        disabled={busy}
        title={t('status.screen_off_help')}
        onclick={() => run('screen_off')}
      >
        {t('status.screen_off')}
      </button>
    {/if}
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

  .note {
    flex: 0 1 auto;
    min-width: 0;
    max-width: 280px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .note.warn {
    color: var(--orange);
  }

  .note.info {
    color: var(--text-2);
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
