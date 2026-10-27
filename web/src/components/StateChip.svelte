<script lang="ts">
  import { parseState } from '../lib/format';
  import { t } from '../lib/i18n';

  let { state, pct = null }: { state: string; pct?: number | null } = $props();

  const parsed = $derived(parseState(state));
  const label = $derived.by(() => {
    const step = parsed.step ? t(`step.${parsed.step}`) : '';
    switch (parsed.kind) {
      case 'recording':
        return t('state.recording');
      case 'processing':
        return pct !== null
          ? t('state.processing_pct', { step, pct: Math.round(pct * 100) })
          : t('state.processing', { step });
      case 'failed':
        return t('state.failed', { step });
      default:
        return t('state.done');
    }
  });
</script>

{#if parsed.kind !== 'done'}
  <span class="chip {parsed.kind}" title={label}>
    {#if parsed.kind === 'recording' || parsed.kind === 'processing'}
      <span class="dot" aria-hidden="true"></span>
    {/if}
    {label}
  </span>
{/if}

<style>
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 22px;
    padding: 0 8px;
    border-radius: 999px;
    font-size: 12px;
    font-weight: 500;
    white-space: nowrap;
  }

  .recording {
    background: var(--danger-soft);
    color: var(--live);
  }

  .processing {
    background: var(--accent-soft);
    color: var(--accent);
  }

  .failed {
    background: var(--danger-soft);
    color: var(--danger);
  }

  .dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: currentColor;
    animation: pulse 1.4s ease-in-out infinite;
  }

  @keyframes pulse {
    50% {
      opacity: 0.3;
    }
  }
</style>
