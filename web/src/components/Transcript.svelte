<script lang="ts">
  import { tick } from 'svelte';
  import { clock, speakerColor } from '../lib/format';
  import { t } from '../lib/i18n';
  import type { Segment } from '../lib/types';

  let {
    segments,
    nameOf,
    active,
    playing,
    editing = $bindable<number | null>(null),
    canCycle,
    busy,
    onplay,
    onsave,
    onnextspeaker,
    ondelete,
  }: {
    segments: Segment[];
    nameOf: (s: Segment) => string;
    /** Line the playhead is in, or the line moved to with `[` `]`. */
    active: number;
    playing: boolean;
    editing?: number | null;
    /** False when there is no other speaker to cycle to. */
    canCycle: boolean;
    /** A segment edit is in flight; tools stay disabled until it returns. */
    busy: boolean;
    onplay: (index: number) => void;
    onsave: (index: number, text: string) => Promise<void>;
    onnextspeaker: (index: number) => void;
    ondelete: (index: number) => void;
  } = $props();

  let draft = $state('');
  let saving = $state(false);
  let lines: HTMLElement[] = $state([]);
  let editor = $state<HTMLTextAreaElement>();

  // Start an edit whenever `editing` is set, whether by the pencil button or
  // the `e` shortcut in the meeting page.
  let started: number | null = null;
  $effect(() => {
    if (editing !== null && editing !== started) {
      started = editing;
      draft = segments[editing]?.text ?? '';
      tick().then(() => {
        editor?.focus();
        editor?.setSelectionRange(draft.length, draft.length);
        autosize();
      });
    } else if (editing === null) {
      started = null;
    }
  });

  // Keep the active line in view while it moves.
  $effect(() => {
    const el = lines[active];
    if (el && editing === null) el.scrollIntoView({ block: 'nearest', behavior: playing ? 'smooth' : 'auto' });
  });

  function autosize() {
    if (!editor) return;
    editor.style.height = 'auto';
    editor.style.height = `${editor.scrollHeight}px`;
  }

  async function save() {
    if (editing === null || busy) return;
    const index = editing;
    const text = draft.trim();
    if (!text || text === segments[index]?.text) {
      editing = null;
      return;
    }
    saving = true;
    try {
      await onsave(index, text);
      editing = null;
    } finally {
      saving = false;
    }
  }

  function editorKey(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault();
      editing = null;
    } else if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      save();
    }
  }
</script>

{#if segments.length === 0}
  <p class="muted empty-t">{t('transcript.empty')}</p>
{:else}
  <ol class="transcript">
    {#each segments as seg, i (i)}
      {@const color = speakerColor(seg.speaker ?? seg.side)}
      <li
        bind:this={lines[i]}
        class="line"
        class:active={i === active}
        class:editing={i === editing}
        style:--spk={color}
      >
        <button class="time mono tnum" onclick={() => onplay(i)} title={t('transcript.play_from')}>
          {clock(seg.start_ms)}
        </button>
        <div class="content">
          <span class="who">{nameOf(seg)}</span>
          {#if i === editing}
            <textarea
              bind:this={editor}
              bind:value={draft}
              class="editor"
              rows="1"
              disabled={saving}
              aria-label={t('transcript.edit_label')}
              oninput={autosize}
              onkeydown={editorKey}
            ></textarea>
            <div class="edit-bar">
              <button class="btn primary" onclick={save} disabled={saving || busy}>{t('common.save')}</button>
              <button class="btn" onclick={() => (editing = null)} disabled={saving}>
                {t('common.cancel')}
              </button>
              <span class="muted hint">{t('transcript.edit_hint')}</span>
            </div>
          {:else}
            <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
            <span class="text" onclick={() => onplay(i)}>{seg.text}</span>
          {/if}
        </div>
        {#if i !== editing}
          <div class="tools">
            <button class="icon-btn" title={t('transcript.edit')} aria-label={t('transcript.edit')} disabled={busy} onclick={() => (editing = i)}>
              <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
                <path d="M11 2.5 13.5 5 6 12.5H3.5V10z" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" />
              </svg>
            </button>
            <button
              class="icon-btn"
              title={t('transcript.next_speaker')}
              aria-label={t('transcript.next_speaker')}
              disabled={!canCycle || busy}
              onclick={() => onnextspeaker(i)}
            >
              <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
                <circle cx="6" cy="5.5" r="2.5" fill="none" stroke="currentColor" stroke-width="1.4" />
                <path d="M1.5 13.5c.5-2.5 2.3-4 4.5-4s4 1.5 4.5 4M11 4h3.5M13 2.5 14.5 4 13 5.5" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" />
              </svg>
            </button>
            <!-- Deleting shifts line indices, which would restart an open edit
                 on its new index and drop the draft; finish the edit first. -->
            <button
              class="icon-btn danger"
              title={t('transcript.delete')}
              aria-label={t('transcript.delete')}
              disabled={busy || editing !== null}
              onclick={() => ondelete(i)}
            >
              <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
                <path d="M3 4.5h10M6.5 4.5V3h3v1.5M4.5 4.5l.5 9h6l.5-9" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" />
              </svg>
            </button>
          </div>
        {/if}
      </li>
    {/each}
  </ol>
{/if}

<style>
  .transcript {
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .line {
    position: relative;
    display: grid;
    grid-template-columns: 56px minmax(0, 1fr);
    gap: 8px;
    padding: 8px;
    border-radius: var(--radius-sm);
    font-size: 16px;
    line-height: 1.6;
    transition: background 120ms;
    scroll-margin: 96px 0;
  }

  .line:hover {
    background: var(--hover);
  }

  .line.active {
    background: var(--accent-soft);
  }

  .line.active::before {
    content: '';
    position: absolute;
    left: 0;
    top: 8px;
    bottom: 8px;
    width: 3px;
    border-radius: 2px;
    background: var(--accent);
  }

  .line.editing {
    padding-right: 8px;
    background: var(--surface-2);
  }

  .time {
    align-self: start;
    height: 26px;
    padding: 0;
    border: 0;
    background: none;
    color: var(--text-3);
    font-size: 13px;
    text-align: left;
    transition: color 120ms;
  }

  .time:hover,
  .line.active .time {
    color: var(--accent);
  }

  .who {
    margin-right: 8px;
    color: var(--spk);
    font-weight: 600;
  }

  .text {
    cursor: pointer;
    overflow-wrap: anywhere;
  }

  /* Floats over the line's top-right corner on hover, so the text keeps
     the full column width. */
  .tools {
    position: absolute;
    top: -14px;
    right: 8px;
    z-index: 2;
    display: flex;
    gap: 2px;
    padding: 2px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--surface);
    box-shadow: var(--shadow-lg);
    opacity: 0;
    pointer-events: none;
    transition: opacity 120ms;
  }

  .line:hover .tools,
  .line:focus-within .tools {
    opacity: 1;
    pointer-events: auto;
  }

  .line:first-child .tools {
    top: 4px;
  }

  @media (hover: none) {
    .line {
      padding-right: 104px;
    }

    .tools {
      top: 6px;
      opacity: 1;
      pointer-events: auto;
      border: 0;
      box-shadow: none;
      background: transparent;
    }
  }

  .editor {
    display: block;
    width: 100%;
    margin-top: 4px;
    padding: 8px;
    border: 1px solid var(--accent);
    border-radius: var(--radius-sm);
    background: var(--surface);
    box-shadow: 0 0 0 3px var(--accent-soft);
    font-size: 16px;
    line-height: 1.6;
    resize: none;
    overflow: hidden;
  }

  .editor:focus {
    outline: none;
  }

  .edit-bar {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 8px;
  }

  .hint {
    font-size: 12px;
  }

  .empty-t {
    padding: 16px 8px;
  }
</style>
