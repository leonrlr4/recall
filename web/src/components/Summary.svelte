<script lang="ts">
  import { clock } from '../lib/format';
  import { t } from '../lib/i18n';
  import type { Summary } from '../lib/types';

  let {
    summary,
    pending,
    currentMs,
    onseek,
  }: {
    summary: Summary | null;
    /** True while the summarize step has not finished. */
    pending: boolean;
    currentMs: number;
    onseek: (ms: number) => void;
  } = $props();

  // Chapter containing the playhead.
  const activeChapter = $derived.by(() => {
    if (!summary) return -1;
    let idx = -1;
    summary.chapters.forEach((c, i) => {
      if (c.start_ms <= currentMs) idx = i;
    });
    return idx;
  });
</script>

{#if !summary}
  <div class="panel pad">
    <p class="muted none">{pending ? t('summary.pending') : t('summary.none')}</p>
  </div>
{:else}
  <div class="stack">
    <section class="panel pad">
      <h2 class="panel-title">{t('summary.title')}</h2>
      {#if summary.one_line}
        <p class="lead">{summary.one_line}</p>
      {/if}
      {#if summary.body}
        <p class="body">{summary.body}</p>
      {/if}
    </section>

    {#if summary.decisions.length}
      <section class="panel pad">
        <h2 class="panel-title">{t('summary.decisions')}</h2>
        <ul class="list">
          {#each summary.decisions as d, i (i)}
            <li>{d}</li>
          {/each}
        </ul>
      </section>
    {/if}

    {#if summary.actions.length}
      <section class="panel pad">
        <h2 class="panel-title">{t('summary.actions')}</h2>
        <ul class="actions">
          {#each summary.actions as a, i (i)}
            <li>
              <span class="box" aria-hidden="true"></span>
              <div>
                <div>{a.text}</div>
                {#if a.owner || a.due}
                  <div class="who">
                    {#if a.owner}<span>{a.owner}</span>{/if}
                    {#if a.owner && a.due}<span aria-hidden="true"> · </span>{/if}
                    {#if a.due}<span>{t('summary.due', { due: a.due })}</span>{/if}
                  </div>
                {/if}
              </div>
            </li>
          {/each}
        </ul>
      </section>
    {/if}

    {#if summary.chapters.length}
      <section class="panel pad">
        <h2 class="panel-title">{t('summary.chapters')}</h2>
        <ol class="chapters">
          {#each summary.chapters as c, i (i)}
            <li>
              <button class:active={i === activeChapter} onclick={() => onseek(c.start_ms)}>
                <span class="mono tnum">{clock(c.start_ms)}</span>
                <span>{c.title}</span>
              </button>
            </li>
          {/each}
        </ol>
      </section>
    {/if}
  </div>
{/if}

<style>
  .stack {
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  .pad {
    padding: 16px;
  }

  .none {
    margin: 0;
  }

  .lead {
    margin: 0;
    font-size: 16px;
    font-weight: 500;
    line-height: 1.5;
  }

  .body {
    margin: 8px 0 0;
    color: var(--text-2);
    line-height: 1.6;
    white-space: pre-line;
  }

  .list {
    margin: 0;
    padding-left: 20px;
    line-height: 1.6;
  }

  .list li + li {
    margin-top: 4px;
  }

  .actions {
    margin: 0;
    padding: 0;
    list-style: none;
    line-height: 1.5;
  }

  .actions li {
    display: grid;
    grid-template-columns: 16px 1fr;
    gap: 8px;
  }

  .actions li + li {
    margin-top: 8px;
  }

  .box {
    width: 14px;
    height: 14px;
    margin-top: 4px;
    border: 1.5px solid var(--border-strong);
    border-radius: 4px;
  }

  .who {
    font-size: 13px;
    color: var(--text-2);
  }

  .chapters {
    margin: 0 -8px;
    padding: 0;
    list-style: none;
  }

  .chapters button {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 12px;
    width: 100%;
    padding: 4px 8px;
    border: 0;
    border-radius: var(--radius-sm);
    background: transparent;
    text-align: left;
    line-height: 1.5;
    transition: background 120ms;
  }

  .chapters button:hover {
    background: var(--hover);
  }

  .chapters button.active {
    background: var(--accent-soft);
  }

  .chapters .mono {
    color: var(--accent);
    padding-top: 1px;
  }
</style>
