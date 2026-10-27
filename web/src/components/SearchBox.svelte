<script lang="ts">
  import { t } from '../lib/i18n';

  let {
    value = $bindable(''),
    input = $bindable<HTMLInputElement | undefined>(),
    onsubmit,
  }: { value?: string; input?: HTMLInputElement; onsubmit?: () => void } = $props();

  function keydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      if (value) value = '';
      else input?.blur();
    } else if (e.key === 'Enter') {
      onsubmit?.();
    }
  }
</script>

<div class="search">
  <svg class="glass" viewBox="0 0 16 16" aria-hidden="true">
    <circle cx="7" cy="7" r="4.5" fill="none" stroke="currentColor" stroke-width="1.5" />
    <path d="m10.5 10.5 3 3" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" />
  </svg>
  <input
    bind:this={input}
    bind:value
    class="input"
    type="search"
    placeholder={t('list.search_placeholder')}
    aria-label={t('list.search_label')}
    onkeydown={keydown}
  />
  {#if !value}
    <kbd class="hint" title={t('list.search_hint')}>/</kbd>
  {/if}
</div>

<style>
  .search {
    position: relative;
  }

  .glass {
    position: absolute;
    left: 12px;
    top: 50%;
    width: 16px;
    height: 16px;
    transform: translateY(-50%);
    color: var(--text-3);
    pointer-events: none;
  }

  .input {
    padding-left: 36px;
    padding-right: 36px;
  }

  .input::-webkit-search-cancel-button {
    display: none;
  }

  .hint {
    position: absolute;
    right: 8px;
    top: 50%;
    transform: translateY(-50%);
    pointer-events: none;
  }
</style>
