<script lang="ts">
  import { onMount } from 'svelte';
  import { ASK_ALL_BUSY, askAll, getConfig } from '../lib/api';
  import { splitCitations } from '../lib/citations';
  import { locale, t } from '../lib/i18n';
  import { link, meetingPath } from '../lib/router';
  import type { Citation } from '../lib/types';

  /** `null` until the config loads. */
  let enabled = $state<boolean | null>(null);
  let question = $state('');
  let asked = $state('');
  let phase = $state<'idle' | 'running' | 'done' | 'error' | 'stopped'>('idle');
  let error = $state<string | null>(null);
  /** The latest tool call, shown while the answer is being worked out. */
  let step = $state<string | null>(null);
  /** Text streamed since the last tool call; replaced by the final answer. */
  let draft = $state('');
  let answer = $state<{ text: string; citations: Citation[] } | null>(null);
  let started = 0;
  let elapsed = $state(0);
  let ctl: AbortController | null = null;

  const pieces = $derived(
    answer ? splitCitations(answer.text, new Set(answer.citations.map((c) => c.id))) : [],
  );
  const titles = $derived(new Map(answer?.citations.map((c) => [c.id, c.title]) ?? []));

  onMount(() => {
    getConfig()
      .then((c) => (enabled = c.claude_code?.enabled ?? false))
      .catch(() => (enabled = true));
    const timer = setInterval(() => {
      if (phase === 'running') elapsed = Math.floor((Date.now() - started) / 1000);
    }, 1000);
    return () => {
      clearInterval(timer);
      ctl?.abort();
    };
  });

  async function submit(e?: Event) {
    e?.preventDefault();
    const q = question.trim();
    if (!q || phase === 'running') return;
    asked = q;
    phase = 'running';
    error = null;
    step = null;
    draft = '';
    answer = null;
    started = Date.now();
    elapsed = 0;
    ctl = new AbortController();
    try {
      answer = await askAll(
        q,
        {
          onTool: (name, detail) => {
            step = detail ? `${name}: ${detail}` : name;
            draft = '';
          },
          onText: (text) => (draft += text),
        },
        ctl.signal,
      );
      phase = 'done';
    } catch (err) {
      if (ctl?.signal.aborted) {
        phase = 'stopped';
      } else {
        phase = 'error';
        error = err instanceof Error ? err.message : String(err);
      }
    } finally {
      ctl = null;
    }
  }

  function key(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) submit(e);
  }

  function when(iso: string): string {
    return new Date(iso).toLocaleString(locale(), { dateStyle: 'medium', timeStyle: 'short' });
  }
</script>

<div class="page">
  <h1>{t('qa.title')}</h1>
  <p class="muted intro">{t('qa.intro')}</p>

  {#if enabled === false}
    <div class="panel off">
      <p>{t('qa.off')}</p>
      <pre>[claude_code]
enabled = true
cwd = "~/Work"</pre>
      <p class="muted">{t('qa.off_hint')}</p>
    </div>
  {:else}
    <form onsubmit={submit}>
      <textarea
        class="input q-input"
        rows="3"
        bind:value={question}
        placeholder={t('qa.placeholder')}
        aria-label={t('qa.title')}
        onkeydown={key}
      ></textarea>
      {#if phase === 'running'}
        <button type="button" class="btn" onclick={() => ctl?.abort()}>{t('ask.stop')}</button>
      {:else}
        <button type="submit" class="btn primary" disabled={!question.trim() || enabled === null}>
          {t('ask.send')}
        </button>
      {/if}
    </form>

    {#if phase !== 'idle'}
      <section class="panel result" aria-live="polite">
        <p class="q">{asked}</p>
        {#if phase === 'running'}
          <p class="muted progress">
            {t('qa.working', { seconds: String(elapsed) })}
            {#if step}<span class="step">{step}</span>{/if}
          </p>
          {#if draft}<p class="a draft">{draft}</p>{/if}
        {:else if phase === 'stopped'}
          <p class="muted">{t('ask.stopped')}</p>
        {:else if phase === 'error'}
          <p class="a err" role="alert">
            {error === ASK_ALL_BUSY ? t('qa.busy') : t('qa.failed')}
            {#if error && error !== ASK_ALL_BUSY}<span class="err-detail">{error}</span>{/if}
          </p>
        {:else if answer}
          <p class="a">
            {#each pieces as piece, i (i)}
              {#if piece.kind === 'text'}{piece.text}{:else}<a
                  class="cite"
                  href={meetingPath(piece.id, piece.ms)}
                  onclick={link}
                  title={titles.get(piece.id)}>{titles.get(piece.id)} {piece.time}</a
                >{/if}
            {/each}
          </p>
          {#if answer.citations.length}
            <h2 class="panel-title">{t('qa.cited')}</h2>
            <ul class="cited">
              {#each answer.citations as c (c.id)}
                <li>
                  <a href={meetingPath(c.id)} onclick={link}>{c.title}</a>
                  <span class="muted">{when(c.started)}</span>
                </li>
              {/each}
            </ul>
          {/if}
        {/if}
      </section>
    {/if}
  {/if}
</div>

<style>
  .page {
    max-width: 860px;
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

  .off {
    padding: 16px;
  }

  .off p {
    margin: 0 0 8px;
  }

  .off pre {
    margin: 0 0 8px;
    padding: 8px 12px;
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    font-family: var(--mono);
    font-size: 13px;
    overflow-x: auto;
  }

  form {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 8px;
    margin-bottom: 16px;
  }

  .q-input {
    height: auto;
    padding: 8px 12px;
    line-height: 1.5;
    resize: vertical;
  }

  .result {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 16px;
  }

  .result p {
    margin: 0;
  }

  .q {
    font-weight: 600;
  }

  .progress {
    font-size: 14px;
  }

  .step {
    display: block;
    margin-top: 4px;
    font-family: var(--mono);
    font-size: 13px;
    overflow-wrap: anywhere;
  }

  .a {
    line-height: 1.7;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .draft {
    color: var(--text-2);
  }

  .err {
    color: var(--danger);
  }

  .err-detail {
    display: block;
    margin-top: 4px;
    font-family: var(--mono);
    font-size: 13px;
    opacity: 0.85;
  }

  .cite {
    padding: 0 4px;
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    color: var(--accent);
    font-size: 13px;
    white-space: nowrap;
  }

  .cite:hover {
    background: var(--hover);
  }

  .panel-title {
    margin: 8px 0 0;
  }

  .cited {
    margin: 0;
    padding-left: 20px;
    font-size: 14px;
  }

  .cited li {
    margin: 2px 0;
  }

  .cited .muted {
    margin-left: 8px;
    font-size: 13px;
  }
</style>
