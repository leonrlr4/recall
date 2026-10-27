<script lang="ts">
  import { t } from '../lib/i18n';

  let {
    id,
    tags = $bindable<string[]>([]),
    describedby,
  }: { id: string; tags?: string[]; describedby?: string } = $props();

  let draft = $state('');

  function add() {
    const v = draft.trim().replace(/,$/, '').trim();
    draft = '';
    if (v && !tags.includes(v)) tags = [...tags, v];
  }

  function remove(i: number) {
    tags = tags.filter((_, j) => j !== i);
  }

  function key(e: KeyboardEvent) {
    if (e.isComposing) return;
    if (e.key === 'Enter' || e.key === ',') {
      e.preventDefault();
      add();
    } else if (e.key === 'Backspace' && draft === '' && tags.length > 0) {
      remove(tags.length - 1);
    }
  }
</script>

<div class="tags">
  {#each tags as tag, i (tag)}
    <span class="tag">
      {tag}
      <button
        type="button"
        class="x"
        aria-label={t('settings.tag_remove', { tag })}
        onclick={() => remove(i)}
      >
        <svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
          <path d="M4 4l8 8M12 4l-8 8" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" />
        </svg>
      </button>
    </span>
  {/each}
  <input
    {id}
    class="entry"
    bind:value={draft}
    onkeydown={key}
    onblur={add}
    placeholder={t('settings.tag_placeholder')}
    aria-describedby={describedby}
  />
</div>

<style>
  .tags {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 4px;
    min-height: 36px;
    padding: 4px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--surface);
    transition: border-color 120ms;
  }

  .tags:hover {
    border-color: var(--border-strong);
  }

  .tags:focus-within {
    border-color: var(--accent);
    box-shadow: 0 0 0 3px var(--accent-soft);
  }

  .tag {
    display: inline-flex;
    align-items: center;
    gap: 2px;
    height: 24px;
    padding: 0 4px 0 8px;
    border-radius: 4px;
    background: var(--surface-2);
    color: var(--text);
    font-size: 13px;
  }

  .x {
    display: inline-grid;
    place-items: center;
    width: 20px;
    height: 20px;
    padding: 0;
    border: 0;
    border-radius: 4px;
    background: none;
    color: var(--text-3);
  }

  .x:hover {
    background: var(--hover);
    color: var(--text);
  }

  .entry {
    flex: 1;
    min-width: 120px;
    height: 24px;
    padding: 0 4px;
    border: 0;
    background: none;
    font-size: 14px;
  }

  .entry:focus {
    outline: none;
  }
</style>
