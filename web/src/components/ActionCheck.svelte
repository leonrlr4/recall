<script lang="ts">
  import type { Snippet } from 'svelte';
  import { dateKey } from '../lib/format';
  import { locale, t } from '../lib/i18n';
  import { dueLabel, isOverdue } from '../lib/due';
  import type { ActionItem } from '../lib/types';

  let {
    item,
    disabled = false,
    ontoggle,
    extra,
  }: {
    item: ActionItem;
    disabled?: boolean;
    ontoggle: (done: boolean) => void;
    /** More of the meta line after owner, due and priority. */
    extra?: Snippet;
  } = $props();

  const overdue = $derived(isOverdue(item, dateKey(new Date())));
  const due = $derived(item.due ? dueLabel(item.due, locale()) : null);
</script>

<div class="item" class:done={item.done}>
  <input
    type="checkbox"
    checked={item.done}
    {disabled}
    aria-label={t('action.mark_done', { text: item.text })}
    onchange={(e) => ontoggle(e.currentTarget.checked)}
  />
  <div class="main">
    <div class="text">{item.text}</div>
    <div class="meta">
      {#if item.priority}
        <span class="prio {item.priority}">{t(`action.${item.priority}`)}</span>
      {/if}
      {#if item.owner}<span>{item.owner}</span>{/if}
      {#if due}
        <span class:overdue>{t(overdue ? 'action.overdue' : 'action.due', { due })}</span>
      {/if}
      {@render extra?.()}
    </div>
  </div>
</div>

<style>
  .item {
    display: grid;
    grid-template-columns: 16px 1fr;
    gap: 8px;
    line-height: 1.5;
  }

  input {
    width: 15px;
    height: 15px;
    margin: 4px 0 0;
    accent-color: var(--accent);
    cursor: pointer;
  }

  input:disabled {
    cursor: default;
  }

  .done .text {
    color: var(--text-3);
    text-decoration: line-through;
  }

  .meta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 2px 10px;
    font-size: 13px;
    color: var(--text-2);
  }

  .prio {
    padding: 0 6px;
    border-radius: 4px;
    font-size: 12px;
    font-weight: 500;
  }

  .prio.high {
    background: var(--danger-soft);
    color: var(--danger);
  }

  .prio.medium {
    background: var(--orange-soft);
    color: var(--orange);
  }

  .prio.low {
    background: var(--surface-2);
    color: var(--text-2);
  }

  .overdue {
    color: var(--danger);
    font-weight: 500;
  }
</style>
