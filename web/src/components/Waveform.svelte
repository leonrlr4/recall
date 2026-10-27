<script lang="ts">
  import { clock } from '../lib/format';
  import { t } from '../lib/i18n';
  import { draw, loadPeaks, type Peaks } from '../lib/player';
  import type { Chapter } from '../lib/types';

  let {
    src,
    durationMs,
    currentMs,
    chapters,
    onseek,
  }: {
    src: string;
    durationMs: number;
    currentMs: number;
    chapters: Chapter[];
    onseek: (ms: number) => void;
  } = $props();

  let canvas = $state<HTMLCanvasElement>();
  let peaks = $state<Peaks | null>(null);
  let status = $state<'loading' | 'ready' | 'failed'>('loading');
  let width = $state(0);
  let scheme = $state(0);
  let hoverX = $state<number | null>(null);

  $effect(() => {
    const ctl = new AbortController();
    status = 'loading';
    loadPeaks(src, undefined, ctl.signal)
      .then((p) => {
        peaks = p;
        status = 'ready';
      })
      .catch(() => {
        // Decoding failures (unsupported codec, missing file) only hide the
        // waveform; the audio element still plays.
        if (!ctl.signal.aborted) status = 'failed';
      });
    return () => ctl.abort();
  });

  $effect(() => {
    const mq = matchMedia('(prefers-color-scheme: dark)');
    const bump = () => scheme++;
    mq.addEventListener('change', bump);
    return () => mq.removeEventListener('change', bump);
  });

  $effect(() => {
    void width;
    void scheme;
    if (!canvas || !peaks) return;
    const css = getComputedStyle(canvas);
    const v = (name: string) => css.getPropertyValue(name).trim();
    draw(canvas, peaks, {
      durationMs,
      currentMs,
      chapters: chapters.map((c) => c.start_ms),
      colors: {
        me: v('--accent'),
        them: v('--orange'),
        playhead: v('--text'),
        tick: v('--text-2'),
      },
    });
  });

  function seekAt(e: MouseEvent) {
    if (!canvas || durationMs <= 0) return;
    const rect = canvas.getBoundingClientRect();
    const f = Math.min(1, Math.max(0, (e.clientX - rect.left) / rect.width));
    onseek(f * durationMs);
  }

  function move(e: MouseEvent) {
    if (!canvas) return;
    hoverX = e.clientX - canvas.getBoundingClientRect().left;
  }

  const hoverMs = $derived(
    hoverX !== null && width > 0 ? Math.max(0, (hoverX / width) * durationMs) : null,
  );
</script>

{#if status !== 'failed'}
  <div class="wave" bind:clientWidth={width}>
    <div class="legend" aria-hidden="true">
      <span class="me">{t('player.me')}</span>
      <span class="them">{t('player.them')}</span>
    </div>
    <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
    <div class="area" onclick={seekAt} onmousemove={move} onmouseleave={() => (hoverX = null)}>
      {#if status === 'loading'}
        <div class="loading">{t('player.loading_waveform')}</div>
      {/if}
      <canvas bind:this={canvas} aria-label={t('player.waveform')}></canvas>
      {#each chapters as c, i (i)}
        {#if durationMs > 0}
          <button
            class="tick"
            style:left="{(c.start_ms / durationMs) * 100}%"
            title="{clock(c.start_ms)} {c.title}"
            aria-label={t('player.jump_chapter', { title: c.title })}
            onclick={(e) => {
              e.stopPropagation();
              onseek(c.start_ms);
            }}
          ></button>
        {/if}
      {/each}
      {#if hoverMs !== null && hoverX !== null}
        <span class="hover-time mono tnum" style:left="{hoverX}px">{clock(hoverMs)}</span>
      {/if}
    </div>
  </div>
{/if}

<style>
  .wave {
    position: relative;
  }

  .legend {
    display: flex;
    gap: 12px;
    margin-bottom: 4px;
    font-size: 12px;
    color: var(--text-2);
  }

  .legend span::before {
    content: '';
    display: inline-block;
    width: 8px;
    height: 8px;
    margin-right: 4px;
    border-radius: 2px;
    vertical-align: 0;
  }

  .legend .me::before {
    background: var(--accent);
  }

  .legend .them::before {
    background: var(--orange);
  }

  .area {
    position: relative;
    height: 96px;
    cursor: pointer;
  }

  canvas {
    display: block;
    width: 100%;
    height: 100%;
  }

  .loading {
    position: absolute;
    inset: 8px 0 0;
    display: grid;
    place-items: center;
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    color: var(--text-3);
    font-size: 13px;
  }

  .tick {
    position: absolute;
    top: -4px;
    width: 12px;
    height: 16px;
    margin-left: -6px;
    padding: 0;
    border: 0;
    background: transparent;
  }

  .tick:hover::after,
  .tick:focus-visible::after {
    content: '';
    position: absolute;
    left: 4px;
    top: 4px;
    width: 4px;
    height: 10px;
    border-radius: 2px;
    background: var(--accent);
  }

  .hover-time {
    position: absolute;
    bottom: -20px;
    transform: translateX(-50%);
    padding: 0 4px;
    border-radius: 4px;
    background: var(--text);
    color: var(--bg);
    font-size: 11px;
    pointer-events: none;
    white-space: nowrap;
  }
</style>
