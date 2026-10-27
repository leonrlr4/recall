<script lang="ts">
  import { clock, initials, speakerColor } from '../lib/format';
  import { locale, t } from '../lib/i18n';
  import { live } from '../lib/live.svelte';
  import { link, meetingPath } from '../lib/router';
  import type { Hit, MeetingRow } from '../lib/types';
  import StateChip from './StateChip.svelte';

  let {
    row,
    hits = [],
    selected = false,
  }: { row: MeetingRow; hits?: Hit[]; selected?: boolean } = $props();

  const MAX_FACES = 5;
  const time = $derived(
    new Date(row.started).toLocaleTimeString(locale(), { hour: '2-digit', minute: '2-digit' }),
  );
  const faces = $derived(row.participants.slice(0, MAX_FACES));
  const more = $derived(row.participants.length - faces.length);
  const progress = $derived(live.progress[row.id]);

  /** Splits a hit snippet on its `[match]` markers. */
  function parts(snippet: string): { text: string; hit: boolean }[] {
    return snippet
      .split(/(\[[^\]]*\])/)
      .filter(Boolean)
      .map((p) =>
        p.startsWith('[') && p.endsWith(']')
          ? { text: p.slice(1, -1), hit: true }
          : { text: p, hit: false },
      );
  }

  let el: HTMLAnchorElement;
  $effect(() => {
    if (selected) el.scrollIntoView({ block: 'nearest' });
  });
</script>

<article class="card" class:selected>
  <a class="main" href={meetingPath(row.id)} onclick={link} bind:this={el} data-card>
    <div class="top">
      <h3>{row.title || t('meeting.untitled')}</h3>
      <StateChip state={row.state} pct={progress?.pct ?? null} />
    </div>
    <div class="meta">
      <span class="tnum">{time}</span>
      <span class="sep" aria-hidden="true">·</span>
      <span class="tnum">{clock(row.duration_ms)}</span>
      {#if row.source_app}
        <span class="sep" aria-hidden="true">·</span>
        <span>{row.source_app}</span>
      {/if}
      {#if faces.length}
        <span class="faces" aria-label={row.participants.join(', ')}>
          {#each faces as name (name)}
            <span class="face" style:background={speakerColor(name)} title={name}>
              {initials(name)}
            </span>
          {/each}
          {#if more > 0}
            <span class="face more" title={row.participants.slice(MAX_FACES).join(', ')}>+{more}</span>
          {/if}
        </span>
      {/if}
    </div>
    {#if row.one_line}
      <p class="summary">{row.one_line}</p>
    {:else if row.state === 'done'}
      <p class="summary muted">{t('list.no_summary')}</p>
    {/if}
  </a>
  {#if hits.length}
    <ul class="hits">
      {#each hits as hit, i (i)}
        <li>
          <a href={meetingPath(row.id, hit.start_ms)} onclick={link}>
            <span class="mono tnum when">{clock(hit.start_ms)}</span>
            <span class="who">{hit.speaker}</span>
            <span class="snippet">
              {#each parts(hit.snippet) as p, j (j)}
                {#if p.hit}<mark>{p.text}</mark>{:else}{p.text}{/if}
              {/each}
            </span>
          </a>
        </li>
      {/each}
    </ul>
  {/if}
</article>

<style>
  .card {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    box-shadow: var(--shadow);
    transition:
      border-color 120ms,
      box-shadow 120ms;
  }

  .card:hover {
    border-color: var(--border-strong);
  }

  .card.selected {
    border-color: var(--accent);
    box-shadow: 0 0 0 3px var(--accent-soft);
  }

  .main {
    display: block;
    padding: 16px;
    border-radius: var(--radius);
  }

  .main:focus-visible {
    outline-offset: -2px;
  }

  .top {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 16px;
  }

  h3 {
    font-size: 16px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .meta {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px;
    margin-top: 4px;
    font-size: 13px;
    color: var(--text-2);
  }

  .sep {
    color: var(--text-3);
  }

  .faces {
    display: inline-flex;
    margin-left: 8px;
  }

  .face {
    display: inline-grid;
    place-items: center;
    width: 24px;
    height: 24px;
    margin-left: -4px;
    border: 2px solid var(--surface);
    border-radius: 50%;
    color: #fff;
    font-size: 10px;
    font-weight: 600;
    letter-spacing: -0.02em;
  }

  .face:first-child {
    margin-left: 0;
  }

  .face.more {
    background: var(--surface-2);
    color: var(--text-2);
  }

  .summary {
    margin: 8px 0 0;
    font-size: 14px;
    color: var(--text-2);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .hits {
    margin: 0;
    padding: 4px 8px 8px;
    list-style: none;
    border-top: 1px solid var(--border);
  }

  .hits a {
    display: grid;
    grid-template-columns: auto auto 1fr;
    gap: 8px;
    align-items: baseline;
    padding: 4px 8px;
    border-radius: var(--radius-sm);
    font-size: 14px;
  }

  .hits a:hover {
    background: var(--hover);
  }

  .when {
    color: var(--accent);
  }

  .who {
    color: var(--text-2);
    font-weight: 500;
  }

  .snippet {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
