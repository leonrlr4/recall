<script lang="ts">
  import { ask } from '../lib/api';
  import { t } from '../lib/i18n';

  let { id }: { id: string } = $props();

  type Turn = { q: string; a: string; state: 'streaming' | 'done' | 'error' | 'stopped' };
  let turns = $state<Turn[]>([]);
  let question = $state('');
  let ctl: AbortController | null = null;
  const busy = $derived(turns.length > 0 && turns[turns.length - 1].state === 'streaming');
  let log = $state<HTMLElement>();

  async function submit(e?: Event) {
    e?.preventDefault();
    const q = question.trim();
    if (!q || busy) return;
    question = '';
    turns.push({ q, a: '', state: 'streaming' });
    const turn = turns[turns.length - 1];
    ctl = new AbortController();
    try {
      await ask(
        id,
        q,
        (token) => {
          turn.a += token;
          log?.scrollTo({ top: log.scrollHeight });
        },
        ctl.signal,
      );
      turn.state = 'done';
    } catch (err) {
      turn.state = ctl?.signal.aborted ? 'stopped' : 'error';
    } finally {
      ctl = null;
    }
  }

  function stop() {
    ctl?.abort();
  }

  function key(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) submit(e);
  }
</script>

<section class="panel ask">
  <h2 class="panel-title">{t('ask.title')}</h2>
  {#if turns.length}
    <div class="log" bind:this={log} aria-live="polite">
      {#each turns as turn, i (i)}
        <div class="turn">
          <p class="q">{turn.q}</p>
          <p class="a" class:err={turn.state === 'error'}>
            {#if turn.state === 'error'}
              {t('ask.failed')}
            {:else}
              {turn.a}{#if turn.state === 'streaming'}<span class="caret" aria-hidden="true"></span>{/if}
              {#if turn.state === 'stopped'}<span class="muted"> {t('ask.stopped')}</span>{/if}
            {/if}
          </p>
        </div>
      {/each}
    </div>
  {:else}
    <p class="muted intro">{t('ask.intro')}</p>
  {/if}
  <form onsubmit={submit}>
    <textarea
      class="input q-input"
      rows="2"
      bind:value={question}
      placeholder={t('ask.placeholder')}
      aria-label={t('ask.title')}
      onkeydown={key}
    ></textarea>
    {#if busy}
      <button type="button" class="btn" onclick={stop}>{t('ask.stop')}</button>
    {:else}
      <button type="submit" class="btn primary" disabled={!question.trim()}>{t('ask.send')}</button>
    {/if}
  </form>
</section>

<style>
  .ask {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 16px;
  }

  .ask .panel-title {
    margin: 0;
  }

  .intro {
    margin: 0;
    font-size: 14px;
  }

  .log {
    display: flex;
    flex-direction: column;
    gap: 16px;
    max-height: 360px;
    overflow-y: auto;
  }

  .turn p {
    margin: 0;
  }

  .q {
    font-weight: 600;
  }

  .a {
    margin-top: 4px !important;
    line-height: 1.6;
    white-space: pre-wrap;
    color: var(--text);
  }

  .a.err {
    color: var(--danger);
  }

  .caret {
    display: inline-block;
    width: 7px;
    height: 1em;
    margin-left: 2px;
    vertical-align: -2px;
    background: var(--accent);
    animation: blink 1s steps(2) infinite;
  }

  @keyframes blink {
    50% {
      opacity: 0;
    }
  }

  form {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 8px;
  }

  .q-input {
    height: auto;
    padding: 8px 12px;
    line-height: 1.5;
    resize: vertical;
  }
</style>
