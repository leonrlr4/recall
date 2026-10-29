<script lang="ts">
  import ActionCheck from './ActionCheck.svelte';
  import { clock } from '../lib/format';
  import { t } from '../lib/i18n';
  import type { Summary } from '../lib/types';

  let {
    summary,
    pending,
    currentMs,
    onseek,
    ontoggle,
  }: {
    summary: Summary | null;
    /** True while the summarize step has not finished. */
    pending: boolean;
    currentMs: number;
    onseek: (ms: number) => void;
    /** Checks off or reopens `summary.actions[index]`. */
    ontoggle: (index: number, done: boolean) => void;
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
              <ActionCheck
                item={a}
                disabled={pending}
                ontoggle={(done) => ontoggle(i, done)}
              >
                {#snippet extra()}
                  {#if a.source_ms !== null}
                    <button
                      class="at mono tnum"
                      title={t('action.play_from', { time: clock(a.source_ms) })}
                      onclick={() => onseek(a.source_ms ?? 0)}
                    >
                      {clock(a.source_ms)}
                    </button>
                  {/if}
                {/snippet}
              </ActionCheck>
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

  .actions li + li {
    margin-top: 8px;
  }

  .at {
    padding: 0;
    border: 0;
    background: transparent;
    color: var(--accent);
    font-size: 12px;
  }

  .at:hover {
    text-decoration: underline;
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
