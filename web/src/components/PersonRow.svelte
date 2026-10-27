<script lang="ts">
  import { tick } from 'svelte';
  import { initials, speakerColor } from '../lib/format';
  import { t } from '../lib/i18n';
  import type { Person } from '../lib/types';

  let {
    person,
    others,
    busy,
    onrename,
    onmerge,
  }: {
    person: Person;
    /** Everyone this person could be merged into. */
    others: Person[];
    /** A change is in flight; editing stays off until the list reloads. */
    busy: boolean;
    onrename: (name: string) => Promise<void>;
    onmerge: (into: Person) => void;
  } = $props();

  /** The registry's own user; renamable, but never merged away. */
  const isMe = $derived(person.id === 'me');

  let editing = $state(false);
  let draft = $state('');
  let input = $state<HTMLInputElement>();
  let button = $state<HTMLButtonElement>();

  /** Closes the editor and puts focus back on the name, for keyboard users. */
  async function close() {
    editing = false;
    await tick();
    button?.focus();
  }

  async function edit() {
    draft = person.name;
    editing = true;
    await tick();
    input?.select();
  }

  async function save() {
    if (!editing) return;
    const name = draft.trim();
    close();
    if (!name || name === person.name) return;
    try {
      await onrename(name);
    } catch {
      // The page shows the error; reopen with what was typed.
      draft = name;
      editing = true;
    }
  }

  function key(e: KeyboardEvent) {
    if (e.isComposing) return;
    if (e.key === 'Enter') {
      e.preventDefault();
      save();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      close();
    }
  }

  function pickMerge(e: Event) {
    const sel = e.currentTarget as HTMLSelectElement;
    const into = others.find((p) => p.id === sel.value);
    sel.value = '';
    if (into) onmerge(into);
  }
</script>

<tr>
  <td>
    <div class="name">
      <span class="avatar" style:--spk={speakerColor(person.id)} aria-hidden="true">{initials(person.name)}</span>
      {#if editing}
        <input
          bind:this={input}
          bind:value={draft}
          class="input"
          aria-label={t('people.name_label', { name: person.name })}
          aria-describedby="people-edit-hint"
          onkeydown={key}
          onblur={save}
        />
      {:else}
        <button bind:this={button} class="name-btn" onclick={edit} disabled={busy} title={t('people.rename')}>
          {person.name}
          {#if isMe}<span class="me-tag">{t('people.you')}</span>{/if}
        </button>
      {/if}
    </div>
  </td>
  <td class="aliases muted">{person.aliases.length > 0 ? person.aliases.join(', ') : '–'}</td>
  <td class="num tnum">{person.samples}</td>
  <td class="num tnum">{person.meetings}</td>
  <td class="merge">
    {#if isMe}
      <span class="help">{t('people.me_no_merge')}</span>
    {:else if others.length > 0}
      <select
        class="select"
        aria-label={t('people.merge_label', { name: person.name })}
        disabled={busy}
        onchange={pickMerge}
      >
        <option value="" selected>{t('people.merge_into')}</option>
        {#each others as o (o.id)}
          <option value={o.id}>{o.name}</option>
        {/each}
      </select>
    {/if}
  </td>
</tr>

<style>
  td {
    padding: 8px 12px;
    border-top: 1px solid var(--border);
    vertical-align: middle;
  }

  .name {
    display: flex;
    align-items: center;
    gap: 12px;
    min-width: 200px;
  }

  .avatar {
    display: inline-grid;
    flex: none;
    place-items: center;
    width: 32px;
    height: 32px;
    border-radius: 50%;
    background: color-mix(in srgb, var(--spk) 16%, transparent);
    color: var(--spk);
    font-size: 13px;
    font-weight: 600;
  }

  .name-btn {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    margin-left: -8px;
    padding: 4px 8px;
    border: 0;
    border-radius: var(--radius-sm);
    background: none;
    font-weight: 500;
    text-align: left;
    cursor: text;
    transition: background 120ms;
  }

  .name-btn:hover:not(:disabled) {
    background: var(--hover);
  }

  .name-btn:disabled {
    cursor: default;
  }

  .name .input {
    max-width: 240px;
    height: 32px;
  }

  .me-tag {
    padding: 0 8px;
    border-radius: 999px;
    background: var(--accent-soft);
    color: var(--accent);
    font-size: 12px;
    font-weight: 500;
  }

  .aliases {
    font-size: 14px;
  }

  .num {
    text-align: right;
  }

  .merge .select {
    width: auto;
    min-width: 160px;
    height: 32px;
  }

  .help {
    color: var(--text-3);
    font-size: 12px;
  }
</style>
