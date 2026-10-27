<script lang="ts">
  import MeetingCard from '../components/MeetingCard.svelte';
  import SearchBox from '../components/SearchBox.svelte';
  import { listMeetings } from '../lib/api';
  import { dateKey } from '../lib/format';
  import { locale, t } from '../lib/i18n';
  import { ignoreKey } from '../lib/keys';
  import { live } from '../lib/live.svelte';
  import { meetingPath, navigate } from '../lib/router';
  import type { Hit, ListFilter, MeetingRow } from '../lib/types';

  let q = $state('');
  let app = $state('');
  let participant = $state('');
  let fromDate = $state('');
  let toDate = $state('');

  let rows = $state<MeetingRow[]>([]);
  let hits = $state<Hit[]>([]);
  let loading = $state(true);
  let error = $state<string | null>(null);
  let selected = $state(-1);
  let searchInput = $state<HTMLInputElement | undefined>();

  // Filter choices come from every row seen, so narrowing one filter does
  // not empty the others' menus.
  let knownApps = $state<string[]>([]);
  let knownPeople = $state<string[]>([]);

  const filtered = $derived(Boolean(q || app || participant || fromDate || toDate));

  /** A `YYYY-MM-DD` date input as the RFC 3339 instant of local midnight. */
  function dayStart(day: string, plusDays = 0): string {
    const [y, m, d] = day.split('-').map(Number);
    return new Date(y, m - 1, d + plusDays).toISOString();
  }

  function filter(): ListFilter {
    return {
      q: q.trim() || undefined,
      app: app || undefined,
      participant: participant || undefined,
      from: fromDate ? dayStart(fromDate) : undefined,
      // The "to" date is inclusive in the UI and exclusive in the API.
      to: toDate ? dayStart(toDate, 1) : undefined,
    };
  }

  let seq = 0;
  async function load(f: ListFilter) {
    const mine = ++seq;
    try {
      const res = await listMeetings(f);
      if (mine !== seq) return;
      rows = res.meetings;
      hits = res.hits;
      error = null;
      const apps = new Set(knownApps);
      const ppl = new Set(knownPeople);
      for (const r of res.meetings) {
        if (r.source_app) apps.add(r.source_app);
        for (const p of r.participants) ppl.add(p);
      }
      knownApps = [...apps].sort();
      knownPeople = [...ppl].sort((a, b) => a.localeCompare(b));
      if (selected >= rows.length) selected = rows.length - 1;
    } catch (e) {
      if (mine === seq) error = e instanceof Error ? e.message : String(e);
    } finally {
      if (mine === seq) loading = false;
    }
  }

  // Refetch on filter changes (debounced while typing) and on daemon events.
  $effect(() => {
    const f = filter();
    void live.version;
    const timer = setTimeout(() => load(f), f.q ? 250 : 0);
    return () => clearTimeout(timer);
  });

  function clear() {
    q = app = participant = fromDate = toDate = '';
  }

  const hitsByMeeting = $derived.by(() => {
    const m = new Map<string, Hit[]>();
    for (const h of hits) {
      const list = m.get(h.meeting) ?? [];
      if (list.length < 3) list.push(h);
      m.set(h.meeting, list);
    }
    return m;
  });

  function dayLabel(key: string, sample: Date): string {
    const today = new Date();
    const yesterday = new Date(today.getFullYear(), today.getMonth(), today.getDate() - 1);
    if (key === dateKey(today)) return t('list.today');
    if (key === dateKey(yesterday)) return t('list.yesterday');
    return sample.toLocaleDateString(locale(), {
      weekday: 'short',
      month: 'long',
      day: 'numeric',
      year: sample.getFullYear() === today.getFullYear() ? undefined : 'numeric',
    });
  }

  const groups = $derived.by(() => {
    const out: { key: string; label: string; items: { row: MeetingRow; index: number }[] }[] = [];
    rows.forEach((row, index) => {
      const d = new Date(row.started);
      const key = dateKey(d);
      let g = out[out.length - 1];
      if (!g || g.key !== key) {
        g = { key, label: dayLabel(key, d), items: [] };
        out.push(g);
      }
      g.items.push({ row, index });
    });
    return out;
  });

  function keydown(e: KeyboardEvent) {
    if (e.key === '/' && !ignoreKey(e)) {
      e.preventDefault();
      searchInput?.focus();
      searchInput?.select();
      return;
    }
    if (ignoreKey(e) || rows.length === 0) return;
    if (e.key === 'j') {
      selected = Math.min(rows.length - 1, selected + 1);
    } else if (e.key === 'k') {
      selected = Math.max(0, selected - 1);
    } else if (e.key === 'Enter' && selected >= 0 && !(e.target instanceof HTMLAnchorElement)) {
      navigate(meetingPath(rows[selected].id));
    } else {
      return;
    }
    e.preventDefault();
  }

  function submitSearch() {
    searchInput?.blur();
    if (rows.length) selected = 0;
  }
</script>

<svelte:window onkeydown={keydown} />

<div class="layout">
  <aside>
    <SearchBox bind:value={q} bind:input={searchInput} onsubmit={submitSearch} />

    <section class="filters" aria-label={t('list.filters')}>
      <label class="field">
        {t('list.filter_app')}
        <select class="select" bind:value={app}>
          <option value="">{t('list.any')}</option>
          {#each knownApps as a (a)}
            <option value={a}>{a}</option>
          {/each}
        </select>
      </label>
      <label class="field">
        {t('list.filter_participant')}
        <select class="select" bind:value={participant}>
          <option value="">{t('list.anyone')}</option>
          {#each knownPeople as p (p)}
            <option value={p}>{p}</option>
          {/each}
        </select>
      </label>
      <div class="dates">
        <label class="field">
          {t('list.filter_from')}
          <input class="input" type="date" bind:value={fromDate} max={toDate || undefined} />
        </label>
        <label class="field">
          {t('list.filter_to')}
          <input class="input" type="date" bind:value={toDate} min={fromDate || undefined} />
        </label>
      </div>
      {#if filtered}
        <button class="btn clear" onclick={clear}>{t('list.clear_filters')}</button>
      {/if}
    </section>

    <section class="keys" aria-label={t('keys.title')}>
      <h2 class="panel-title">{t('keys.title')}</h2>
      <dl>
        <dt><kbd>/</kbd></dt>
        <dd>{t('keys.search')}</dd>
        <dt><kbd>j</kbd> <kbd>k</kbd></dt>
        <dd>{t('keys.move')}</dd>
        <dt><kbd>{t('keys.enter_key')}</kbd></dt>
        <dd>{t('keys.open')}</dd>
      </dl>
    </section>
  </aside>

  <section class="results" aria-busy={loading}>
    <div class="results-head">
      <h1>{filtered ? t('list.results') : t('list.title')}</h1>
      {#if !loading && !error}
        <span class="muted count">{rows.length === 1 ? t('list.count_one') : t('list.count', { n: rows.length })}</span>
      {/if}
    </div>

    {#if error}
      <div class="error" role="alert">{t('list.load_failed', { error })}</div>
    {:else if loading}
      <div class="skeletons" aria-label={t('common.loading')}>
        {#each [0, 1, 2] as i (i)}
          <div class="skeleton"></div>
        {/each}
      </div>
    {:else if rows.length === 0}
      <div class="empty">
        {#if filtered}
          <p>{t('list.no_results')}</p>
          <button class="btn" onclick={clear}>{t('list.clear_filters')}</button>
        {:else}
          <p class="empty-title">{t('list.empty_title')}</p>
          <p>{t('list.empty_body')}</p>
        {/if}
      </div>
    {:else}
      {#each groups as g (g.key)}
        <section class="group">
          <h2 class="day">{g.label}</h2>
          <div class="cards">
            {#each g.items as { row, index } (row.id)}
              <MeetingCard {row} hits={hitsByMeeting.get(row.id)} selected={index === selected} />
            {/each}
          </div>
        </section>
      {/each}
    {/if}
  </section>
</div>

<style>
  .layout {
    display: grid;
    grid-template-columns: 280px minmax(0, 1fr);
    gap: 32px;
    max-width: var(--max);
    margin: 0 auto;
    padding: 24px var(--gutter) 48px;
  }

  aside {
    position: sticky;
    top: 72px;
    align-self: start;
    display: flex;
    flex-direction: column;
    gap: 24px;
  }

  .filters {
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  .dates {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
  }

  .dates .input {
    padding: 0 8px;
  }

  .clear {
    align-self: flex-start;
  }

  .keys dl {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 8px 12px;
    margin: 0;
    font-size: 13px;
    color: var(--text-2);
  }

  .keys dd {
    margin: 0;
  }

  .results-head {
    display: flex;
    align-items: baseline;
    gap: 12px;
    margin-bottom: 16px;
  }

  h1 {
    font-size: 24px;
    letter-spacing: -0.01em;
  }

  .count {
    font-size: 14px;
  }

  .group + .group {
    margin-top: 32px;
  }

  .day {
    position: sticky;
    top: 48px;
    z-index: 1;
    margin: 0 0 8px;
    padding: 8px 0;
    background: var(--bg);
    font-size: 13px;
    font-weight: 600;
    color: var(--text-2);
  }

  .cards {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .skeletons {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .skeleton {
    height: 96px;
    border-radius: var(--radius);
    background: linear-gradient(90deg, var(--surface-2), var(--surface), var(--surface-2));
    background-size: 200% 100%;
    animation: shimmer 1.2s linear infinite;
  }

  @keyframes shimmer {
    to {
      background-position: -200% 0;
    }
  }

  .empty-title {
    font-size: 16px;
    font-weight: 600;
    color: var(--text);
  }

  @media (max-width: 800px) {
    .layout {
      grid-template-columns: minmax(0, 1fr);
      gap: 24px;
    }

    aside {
      position: static;
    }

    .keys {
      display: none;
    }
  }
</style>
