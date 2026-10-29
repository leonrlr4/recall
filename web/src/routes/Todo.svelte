<script lang="ts">
  import { onMount } from 'svelte';
  import ActionCheck from '../components/ActionCheck.svelte';
  import { actionItems, patchAction } from '../lib/api';
  import { clock } from '../lib/format';
  import { locale, t } from '../lib/i18n';
  import { live } from '../lib/live.svelte';
  import { link, meetingPath } from '../lib/router';
  import { DEFAULT_FILTER, filterTodos, meetings, owners, sortTodos } from '../lib/todo';
  import type { ActionRow } from '../lib/types';

  let all = $state<ActionRow[] | null>(null);
  let loadError = $state<string | null>(null);
  let actionError = $state<string | null>(null);
  let owner = $state(DEFAULT_FILTER.owner);
  let status = $state(DEFAULT_FILTER.status);
  let meeting = $state(DEFAULT_FILTER.meeting);
  /** Rows with a PATCH in flight, as `meeting\nindex`. */
  let saving = $state<Set<string>>(new Set());

  const knownOwners = $derived(all ? owners(all) : []);
  const knownMeetings = $derived(all ? meetings(all) : []);
  const rows = $derived(all ? sortTodos(filterTodos(all, { owner, status, meeting })) : []);

  const key = (r: ActionRow) => `${r.meeting}\n${r.index}`;

  async function load() {
    try {
      all = await actionItems();
      loadError = null;
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    }
  }

  onMount(() => {
    load();
  });

  // A finished job may have rewritten a meeting's items.
  let seenVersion = live.version;
  $effect(() => {
    const v = live.version;
    if (v !== seenVersion) {
      seenVersion = v;
      load();
    }
  });

  /** Shows the change at once; a refused one is undone and the list
   *  reloaded, since a 409 can mean a rerun replaced the items. */
  async function toggle(row: ActionRow, done: boolean) {
    const k = key(row);
    saving = new Set(saving).add(k);
    actionError = null;
    const before = { done: row.done, done_at: row.done_at };
    row.done = done;
    try {
      Object.assign(row, await patchAction(row.meeting, row.index, { done, text: row.text }));
    } catch (e) {
      Object.assign(row, before);
      actionError = t('action.save_failed', { error: e instanceof Error ? e.message : String(e) });
      await load();
    } finally {
      const next = new Set(saving);
      next.delete(k);
      saving = next;
    }
  }

  function startedLabel(r: ActionRow): string {
    return new Date(r.started).toLocaleDateString(locale(), { month: 'short', day: 'numeric' });
  }
</script>

<div class="page">
  <h1>{t('todo.title')}</h1>
  <p class="muted intro">{t('todo.intro')}</p>

  <section class="filters" aria-label={t('todo.filters')}>
    <label class="field">
      {t('todo.owner')}
      <select class="select" bind:value={owner}>
        <option value="me">{t('todo.owner_me')}</option>
        <option value="all">{t('todo.owner_all')}</option>
        {#each knownOwners as o (o)}
          <option value={o}>{o}</option>
        {/each}
      </select>
    </label>
    <label class="field">
      {t('todo.status')}
      <select class="select" bind:value={status}>
        <option value="open">{t('todo.status_open')}</option>
        <option value="done">{t('todo.status_done')}</option>
        <option value="all">{t('todo.status_all')}</option>
      </select>
    </label>
    <label class="field wide">
      {t('todo.meeting')}
      <select class="select" bind:value={meeting}>
        <option value="">{t('todo.meeting_all')}</option>
        {#each knownMeetings as m (m.id)}
          <option value={m.id}>{m.title}</option>
        {/each}
      </select>
    </label>
  </section>

  {#if actionError}
    <div class="error" role="alert">{actionError}</div>
  {/if}

  {#if loadError}
    <div class="error" role="alert">{t('todo.load_failed', { error: loadError })}</div>
  {:else if !all}
    <p class="muted">{t('common.loading')}</p>
  {:else if all.length === 0}
    <div class="empty">{t('todo.none_yet')}</div>
  {:else if rows.length === 0}
    <div class="empty">{t('todo.no_results')}</div>
  {:else}
    <p class="muted count">
      {rows.length === 1 ? t('todo.count_one') : t('todo.count', { n: rows.length })}
    </p>
    <ul class="panel list">
      {#each rows as row (key(row))}
        <li>
          <ActionCheck
            item={row}
            disabled={saving.has(key(row))}
            ontoggle={(done) => toggle(row, done)}
          >
            {#snippet extra()}
              <a
                class="source"
                href={meetingPath(row.meeting, row.source_ms ?? undefined)}
                onclick={link}
              >
                {startedLabel(row)} · {row.title}{#if row.source_ms !== null}
                  <span class="mono tnum"> {clock(row.source_ms)}</span>{/if}
              </a>
            {/snippet}
          </ActionCheck>
        </li>
      {/each}
    </ul>
  {/if}
</div>

<style>
  .page {
    max-width: 960px;
    margin: 0 auto;
    padding: 24px var(--gutter) 48px;
  }

  h1 {
    font-size: 24px;
    margin-bottom: 8px;
  }

  .intro {
    margin: 0 0 24px;
    font-size: 14px;
  }

  .filters {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 160px)) minmax(0, 320px);
    gap: 12px;
    margin-bottom: 16px;
  }

  @media (max-width: 640px) {
    .filters {
      grid-template-columns: 1fr 1fr;
    }

    .filters .wide {
      grid-column: 1 / -1;
    }
  }

  .error {
    margin-bottom: 16px;
  }

  .count {
    margin: 0 0 8px;
    font-size: 13px;
  }

  .list {
    margin: 0;
    padding: 4px 0;
    list-style: none;
  }

  .list li {
    padding: 10px 16px;
  }

  .list li + li {
    border-top: 1px solid var(--border);
  }

  .source {
    color: var(--accent);
  }

  .source:hover {
    text-decoration: underline;
  }
</style>
