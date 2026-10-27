<script lang="ts">
  import { t } from '../lib/i18n';
</script>

<div class="page">
  <h1>{t('settings.title')}</h1>
  <p class="muted">{t('common.coming_soon')}</p>
</div>

<style>
  .page {
    max-width: var(--max);
    margin: 0 auto;
    padding: 24px var(--gutter);
  }

  h1 {
    font-size: 24px;
    margin-bottom: 8px;
  }
</style>
