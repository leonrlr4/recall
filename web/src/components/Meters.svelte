<script lang="ts">
  import { t } from '../lib/i18n';

  let { mic, sys }: { mic: number[]; sys: number[] } = $props();

  const rows = $derived([
    { key: 'me', label: t('player.me'), peaks: mic },
    { key: 'them', label: t('player.them'), peaks: sys },
  ]);

  const pct = (v: number) => Math.round(Math.min(1, Math.max(0, v)) * 100);
  const max = (peaks: number[]) => peaks.reduce((m, p) => Math.max(m, p), 0);
</script>

<div class="meters">
  {#each rows as row (row.key)}
    <div class="row {row.key}">
      <span class="label">{row.label}</span>
      <div
        class="bars"
        role="img"
        aria-label={t('settings.selftest.meter', { track: row.label, pct: pct(max(row.peaks)) })}
      >
        {#each row.peaks as p, i (i)}
          <span class="bar" style:height="{Math.max(4, pct(p))}%"></span>
        {/each}
      </div>
      <span class="peak mono tnum">{pct(max(row.peaks))}%</span>
    </div>
  {/each}
</div>

<style>
  .meters {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .row {
    display: grid;
    grid-template-columns: 48px minmax(0, 1fr) 48px;
    align-items: center;
    gap: 8px;
    font-size: 13px;
  }

  .label {
    color: var(--text-2);
    font-weight: 500;
  }

  .bars {
    display: flex;
    align-items: flex-end;
    gap: 2px;
    height: 40px;
    padding: 4px;
    border-radius: var(--radius-sm);
    background: var(--surface-2);
  }

  .bar {
    flex: 1;
    min-width: 1px;
    border-radius: 1px;
  }

  .me .bar {
    background: var(--accent);
  }

  .them .bar {
    background: var(--orange);
  }

  .peak {
    color: var(--text-2);
    text-align: right;
  }
</style>
