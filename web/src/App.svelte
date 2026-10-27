<script lang="ts">
  import { onMount } from 'svelte';
  import { t } from './lib/i18n';
  import { connectLive, live } from './lib/live.svelte';
  import { current, link, onRoute, type Route } from './lib/router';
  import List from './routes/List.svelte';
  import Meeting from './routes/Meeting.svelte';
  import People from './routes/People.svelte';
  import Settings from './routes/Settings.svelte';

  let route = $state<Route>(current());

  onMount(() => {
    connectLive();
    return onRoute((r) => (route = r));
  });

  $effect(() => {
    const title =
      route.name === 'people'
        ? t('nav.people')
        : route.name === 'settings'
          ? t('nav.settings')
          : null;
    if (route.name !== 'meeting') document.title = title ? `${title} · recall` : 'recall';
  });
</script>

<header class="topbar">
  <div class="inner">
    <a class="brand" href="/" onclick={link}>
      <span class="logo" aria-hidden="true"></span>
      recall
    </a>
    <nav aria-label={t('nav.label')}>
      <a href="/" onclick={link} class:active={route.name === 'list' || route.name === 'meeting'}>
        {t('nav.meetings')}
      </a>
      <a href="/people" onclick={link} class:active={route.name === 'people'}>{t('nav.people')}</a>
      <a href="/settings" onclick={link} class:active={route.name === 'settings'}>
        {t('nav.settings')}
      </a>
    </nav>
    <div class="status" aria-live="polite">
      {#if live.recording !== null}
        <span class="rec">
          <span class="dot"></span>
          {live.recording ? t('status.recording', { app: live.recording }) : t('status.recording_any')}
        </span>
      {/if}
    </div>
  </div>
</header>

<main>
  {#if route.name === 'list'}
    <List />
  {:else if route.name === 'meeting'}
    {#key route.id}
      <Meeting id={route.id} startMs={route.t} />
    {/key}
  {:else if route.name === 'people'}
    <People />
  {:else if route.name === 'settings'}
    <Settings />
  {:else}
    <div class="page empty">
      <p>{t('common.not_found')}</p>
      <a class="btn" href="/" onclick={link}>{t('common.back_to_list')}</a>
    </div>
  {/if}
</main>

<style>
  .topbar {
    position: sticky;
    top: 0;
    z-index: 10;
    background: color-mix(in srgb, var(--bg) 88%, transparent);
    backdrop-filter: blur(8px);
    border-bottom: 1px solid var(--border);
  }

  .inner {
    display: flex;
    align-items: center;
    gap: 24px;
    max-width: var(--max);
    height: 48px;
    margin: 0 auto;
    padding: 0 var(--gutter);
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 8px;
    font-weight: 700;
    letter-spacing: -0.01em;
  }

  .logo {
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: conic-gradient(var(--accent) 0 50%, var(--orange) 50% 100%);
  }

  nav {
    display: flex;
    gap: 4px;
  }

  nav a {
    padding: 4px 12px;
    border-radius: var(--radius-sm);
    color: var(--text-2);
    font-size: 14px;
    transition:
      background 120ms,
      color 120ms;
  }

  nav a:hover {
    background: var(--hover);
    color: var(--text);
  }

  nav a.active {
    color: var(--text);
    background: var(--surface-2);
    font-weight: 500;
  }

  .status {
    margin-left: auto;
    font-size: 13px;
  }

  .rec {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 2px 12px;
    border-radius: 999px;
    background: var(--danger-soft);
    color: var(--live);
    font-weight: 500;
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--live);
    animation: pulse 1.6s ease-in-out infinite;
  }

  @keyframes pulse {
    50% {
      opacity: 0.35;
    }
  }

  .page {
    max-width: var(--max);
    margin: 0 auto;
    padding: 24px var(--gutter);
  }
</style>
