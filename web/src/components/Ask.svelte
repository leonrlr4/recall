<script lang="ts">
  import { ask } from '../lib/api';
  import { t } from '../lib/i18n';

  let { id }: { id: string } = $props();

  type Turn = {
    q: string;
    a: string;
    state: 'streaming' | 'done' | 'error' | 'stopped';
    error?: string;
  };
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
      if (ctl?.signal.aborted) {
        turn.state = 'stopped';
      } else {
        turn.state = 'error';
        turn.error = err instanceof Error ? err.message : String(err);
      }
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
          {#if turn.a || turn.state !== 'error'}
            <p class="a">
              {turn.a}{#if turn.state === 'streaming'}<span class="caret" aria-hidden="true"></span>{/if}
              {#if turn.state === 'stopped'}<span class="muted"> {t('ask.stopped')}</span>{/if}
            </p>
          {/if}
          {#if turn.state === 'error'}
            <p class="a err" role="alert">
              {t('ask.failed')}
              {#if turn.error}<span class="err-detail">{turn.error}</span>{/if}
            </p>
          {/if}
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

  .err-detail {
    display: block;
    margin-top: 4px;
    font-family: var(--mono);
    font-size: 13px;
    opacity: 0.85;
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
