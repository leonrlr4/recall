<script lang="ts">
  import { onMount } from 'svelte';
  import PersonRow from '../components/PersonRow.svelte';
  import { patchPerson, people } from '../lib/api';
  import { t } from '../lib/i18n';
  import type { Person } from '../lib/types';

  let list = $state<Person[] | null>(null);
  let loadError = $state<string | null>(null);
  let actionError = $state<string | null>(null);
  let busy = $state(false);

  async function load() {
    try {
      const all = await people();
      // `me` first, then by name.
      list = all.sort((a, b) =>
        a.id === 'me' ? -1 : b.id === 'me' ? 1 : a.name.localeCompare(b.name),
      );
      loadError = null;
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    }
  }

  onMount(() => {
    load();
  });

  /** Runs one PATCH, then reloads the list whether it worked or not. */
  async function change(id: string, body: { name?: string; merge_into?: string }) {
    busy = true;
    actionError = null;
    try {
      await patchPerson(id, body);
    } catch (e) {
      actionError = t('people.save_failed', { error: e instanceof Error ? e.message : String(e) });
      throw e;
    } finally {
      await load();
      busy = false;
    }
  }

  function merge(from: Person, into: Person) {
    if (!confirm(t('people.merge_confirm', { from: from.name, into: into.name }))) return;
    change(from.id, { merge_into: into.id }).catch(() => {});
  }

  const onlyMe = $derived(list !== null && list.every((p) => p.id === 'me'));
</script>

<div class="page">
  <h1>{t('people.title')}</h1>
  <p class="muted intro">{t('people.intro')}</p>

  {#if actionError}
    <div class="error" role="alert">{actionError}</div>
  {/if}

  {#if loadError}
    <div class="error" role="alert">{t('people.load_failed', { error: loadError })}</div>
  {:else if !list}
    <p class="muted">{t('common.loading')}</p>
  {:else}
    <div class="panel table-wrap">
      <table>
        <thead>
          <tr>
            <th scope="col">{t('people.col_name')}</th>
            <th scope="col">{t('people.col_aliases')}</th>
            <th scope="col" class="num">{t('people.col_samples')}</th>
            <th scope="col" class="num">{t('people.col_meetings')}</th>
            <th scope="col"><span class="sr-only">{t('people.col_merge')}</span></th>
          </tr>
        </thead>
        <tbody>
          {#each list as person (person.id)}
            <PersonRow
              {person}
              others={list.filter((p) => p.id !== person.id)}
              {busy}
              onrename={(name) => change(person.id, { name })}
              onmerge={(into) => merge(person, into)}
            />
          {/each}
        </tbody>
      </table>
    </div>
    <p class="muted hint" id="people-edit-hint">{t('people.edit_hint')}</p>
    {#if onlyMe}
      <div class="empty">
        <p class="empty-title">{t('people.empty_title')}</p>
        <p>{t('people.empty_body')}</p>
      </div>
    {/if}
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

  .error {
    margin-bottom: 16px;
  }

  .table-wrap {
    overflow-x: auto;
  }

  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 14px;
  }

  th {
    padding: 8px 12px;
    color: var(--text-3);
    font-size: 12px;
    font-weight: 600;
    letter-spacing: 0.04em;
    text-align: left;
    text-transform: uppercase;
  }

  th.num {
    text-align: right;
  }

  .hint {
    margin: 8px 0 0;
    font-size: 12px;
  }

  .empty-title {
    margin: 0 0 4px;
    color: var(--text);
    font-weight: 600;
  }

  .empty p {
    margin: 0;
  }
</style>
