<script lang="ts">
  import { onMount } from 'svelte';
  import Meters from '../components/Meters.svelte';
  import TagInput from '../components/TagInput.svelte';
  import { getConfig, putConfig, selftest } from '../lib/api';
  import { followBrowserLocale, setLocale, t } from '../lib/i18n';
  import { savedConfig } from '../lib/saved.svelte';
  import {
    applyPatch,
    BACKENDS,
    configDiff,
    fieldOfError,
    mergePatch,
    selftestVerdict,
    STT_ENGINES,
    SUMMARY_LANGS,
    UI_LANGS,
    uiLangOf,
    type Field,
    type UiLang,
    type Verdict,
  } from '../lib/settings';
  import type { Config, SelfTest } from '../lib/types';

  /** Seconds the self-test records; the server decides, this only drives the countdown. */
  const SELFTEST_SECS = 5;

  /** What the daemon runs with, or what was last saved; the diff base. */
  let saved = $state<Config | null>(null);
  let form = $state<Config | null>(null);
  let loadError = $state<string | null>(null);
  let saving = $state(false);
  let formError = $state<string | null>(null);
  let serverErrors = $state<Partial<Record<Field, string>>>({});
  let savedFlash = $state(false);

  let testing = $state(false);
  let countdown = $state(0);
  let test = $state<SelfTest | null>(null);
  let testError = $state<string | null>(null);
  const verdict = $derived<Verdict | null>(test ? selftestVerdict(test.mic_peaks, test.sys_peaks) : null);

  onMount(async () => {
    try {
      const c = applyPatch(await getConfig(), savedConfig.patch);
      saved = c;
      form = structuredClone(c);
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    }
  });

  const uiLang = $derived(uiLangOf(form?.ui_lang));

  function pickUiLang(e: Event) {
    if (!form) return;
    const v = (e.currentTarget as HTMLSelectElement).value as UiLang;
    form.ui_lang = v === 'auto' ? null : v;
    if (v === 'auto') followBrowserLocale();
    else setLocale(v);
  }

  const isInt = (v: unknown, min: number, max = Number.MAX_SAFE_INTEGER) =>
    typeof v === 'number' && Number.isInteger(v) && v >= min && v <= max;

  /** Client-side checks for what the browser can tell; the server checks the rest. */
  const localErrors = $derived.by(() => {
    const out: Partial<Record<Field, string>> = {};
    if (!form) return out;
    for (const f of ['debounce_secs', 'end_grace_secs'] as const) {
      if (!isInt(form[f], 0)) out[f] = t('settings.err_seconds');
    }
    if (!isInt(form.stt.threads, 1, 256)) out['stt.threads'] = t('settings.err_threads');
    if (!isInt(form.llm.context_tokens, 1024)) out['llm.context_tokens'] = t('settings.err_context_tokens');
    if (!isInt(form.port, 1, 65535)) out.port = t('settings.err_port');
    for (const f of ['meetings_dir', 'data_dir'] as const) {
      if (!form[f].startsWith('/')) out[f] = t('settings.err_absolute');
    }
    return out;
  });

  const errors = $derived({ ...serverErrors, ...localErrors });
  const patch = $derived(saved && form ? configDiff(saved, form) : {});
  const dirty = $derived(Object.keys(patch).length > 0);
  const valid = $derived(Object.keys(localErrors).length === 0);

  async function save(e: SubmitEvent) {
    e.preventDefault();
    if (!form || !dirty || !valid || saving) return;
    saving = true;
    formError = null;
    serverErrors = {};
    const sent = $state.snapshot(form);
    // `patch` is derived from `saved`, so hold this save's copy before
    // `saved` moves.
    const body = $state.snapshot(patch);
    try {
      const res = await putConfig(body);
      // GET /api/config keeps returning the running config until a restart,
      // so the saved values become the new diff base here.
      saved = sent;
      savedConfig.patch = mergePatch($state.snapshot(savedConfig.patch), body);
      if (res.restart_required) savedConfig.restartNeeded = true;
      savedFlash = true;
      setTimeout(() => (savedFlash = false), 3000);
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      const field = fieldOfError(msg);
      if (field && shown(field)) serverErrors = { [field]: msg };
      else formError = msg;
    } finally {
      saving = false;
    }
  }

  function reset() {
    if (!saved) return;
    form = structuredClone($state.snapshot(saved));
    serverErrors = {};
    formError = null;
    if (uiLangOf(saved.ui_lang) === 'auto') followBrowserLocale();
    else setLocale(uiLangOf(saved.ui_lang) as 'zh-TW' | 'en');
  }

  // Any edit clears the server's complaint about that form state.
  $effect(() => {
    void JSON.stringify(form);
    serverErrors = {};
  });

  async function runTest() {
    if (testing) return;
    testing = true;
    test = null;
    testError = null;
    countdown = SELFTEST_SECS;
    const timer = setInterval(() => {
      countdown = Math.max(0, countdown - 1);
    }, 1000);
    try {
      test = await selftest();
    } catch (e) {
      testError = e instanceof Error ? e.message : String(e);
    } finally {
      clearInterval(timer);
      testing = false;
    }
  }

  /** Whether the form currently renders `f`; the endpoint is only shown for Ollama. */
  const shown = (f: Field) => f !== 'llm.endpoint' || form?.llm.backend === 'ollama';

  const id = (f: Field) => `f-${f.replace('.', '-')}`;
  const describedby = (f: Field) => `${id(f)}-help${errors[f] ? ` ${id(f)}-err` : ''}`;
</script>

{#snippet below(f: Field, help: string)}
  <span class="help" id="{id(f)}-help">{help}</span>
  {#if errors[f]}
    <span class="field-err" id="{id(f)}-err">{errors[f]}</span>
  {/if}
{/snippet}

<div class="page">
  <h1>{t('settings.title')}</h1>

  {#if savedConfig.restartNeeded}
    <div class="restart" role="status">
      <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
        <path d="M13 8a5 5 0 1 1-1.5-3.6M13 2.5V5h-2.5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" />
      </svg>
      {t('settings.restart_required')}
    </div>
  {/if}

  {#if loadError}
    <div class="error" role="alert">{t('settings.load_failed', { error: loadError })}</div>
  {:else if !form}
    <p class="muted">{t('common.loading')}</p>
  {:else}
    <form onsubmit={save} novalidate>
      <section class="panel group" aria-labelledby="g-recording">
        <h2 id="g-recording">{t('settings.group.recording')}</h2>

        <div class="field">
          <label for={id('meetings_dir')}>{t('settings.meetings_dir')}</label>
          <input id={id('meetings_dir')} class="input mono" bind:value={form.meetings_dir} aria-invalid={!!errors.meetings_dir} aria-describedby={describedby('meetings_dir')} spellcheck="false" />
          {@render below('meetings_dir', t('settings.meetings_dir_help'))}
        </div>

        <div class="field">
          <label for={id('data_dir')}>{t('settings.data_dir')}</label>
          <input id={id('data_dir')} class="input mono" bind:value={form.data_dir} aria-invalid={!!errors.data_dir} aria-describedby={describedby('data_dir')} spellcheck="false" />
          {@render below('data_dir', t('settings.data_dir_help'))}
        </div>

        <div class="field check">
          <label>
            <input type="checkbox" bind:checked={form.record_screen} aria-describedby={describedby('record_screen')} />
            {t('settings.record_screen')}
          </label>
          {@render below('record_screen', t('settings.record_screen_help'))}
        </div>

        <div class="field">
          <label for={id('allowlist')}>{t('settings.allowlist')}</label>
          <TagInput id={id('allowlist')} bind:tags={form.allowlist} describedby={describedby('allowlist')} />
          {@render below('allowlist', t('settings.allowlist_help'))}
        </div>

        <div class="row3">
          <div class="field">
            <label for={id('debounce_secs')}>{t('settings.debounce_secs')}</label>
            <input id={id('debounce_secs')} class="input tnum" type="number" min="0" step="1" bind:value={form.debounce_secs} aria-invalid={!!errors.debounce_secs} aria-describedby={describedby('debounce_secs')} />
            {@render below('debounce_secs', t('settings.debounce_secs_help'))}
          </div>
          <div class="field">
            <label for={id('end_grace_secs')}>{t('settings.end_grace_secs')}</label>
            <input id={id('end_grace_secs')} class="input tnum" type="number" min="0" step="1" bind:value={form.end_grace_secs} aria-invalid={!!errors.end_grace_secs} aria-describedby={describedby('end_grace_secs')} />
            {@render below('end_grace_secs', t('settings.end_grace_secs_help'))}
          </div>
        </div>

        <div class="selftest">
          <div class="selftest-head">
            <button type="button" class="btn" onclick={runTest} disabled={testing}>
              {testing ? t('settings.selftest.running', { n: countdown }) : t('settings.selftest.run')}
            </button>
            <span class="help">{t('settings.selftest.help', { n: SELFTEST_SECS })}</span>
          </div>
          <div aria-live="polite">
            {#if testError}
              <p class="field-err">{t('settings.selftest.failed', { error: testError })}</p>
            {:else if test && verdict}
              <Meters mic={test.mic_peaks} sys={test.sys_peaks} />
              <p class="verdict {verdict === 'ok' ? 'ok' : 'warn'}">{t(`settings.selftest.${verdict}`)}</p>
            {/if}
          </div>
        </div>
      </section>

      <section class="panel group" aria-labelledby="g-stt">
        <h2 id="g-stt">{t('settings.group.transcription')}</h2>
        <div class="row2">
          <div class="field">
            <label for={id('stt.engine')}>{t('settings.stt_engine')}</label>
            <select id={id('stt.engine')} class="select" bind:value={form.stt.engine} aria-describedby={describedby('stt.engine')}>
              {#each STT_ENGINES.includes(form.stt.engine) ? STT_ENGINES : [...STT_ENGINES, form.stt.engine] as e (e)}
                <option value={e}>{e}</option>
              {/each}
            </select>
            {@render below('stt.engine', t('settings.stt_engine_help'))}
          </div>
          <div class="field">
            <label for={id('stt.threads')}>{t('settings.stt_threads')}</label>
            <input id={id('stt.threads')} class="input tnum" type="number" min="1" step="1" bind:value={form.stt.threads} aria-invalid={!!errors['stt.threads']} aria-describedby={describedby('stt.threads')} />
            {@render below('stt.threads', t('settings.stt_threads_help'))}
          </div>
        </div>
      </section>

      <section class="panel group" aria-labelledby="g-summary">
        <h2 id="g-summary">{t('settings.group.summary')}</h2>
        <div class="row2">
          <div class="field">
            <label for={id('llm.backend')}>{t('settings.llm_backend')}</label>
            <select id={id('llm.backend')} class="select" bind:value={form.llm.backend} aria-describedby={describedby('llm.backend')}>
              {#each BACKENDS as b (b)}
                <option value={b}>{t(`settings.backend.${b}`)}</option>
              {/each}
            </select>
            {@render below('llm.backend', t('settings.llm_backend_help'))}
          </div>
          <div class="field">
            <label for={id('llm.model')}>{t('settings.llm_model')}</label>
            <input id={id('llm.model')} class="input mono" bind:value={form.llm.model} aria-invalid={!!errors['llm.model']} aria-describedby={describedby('llm.model')} spellcheck="false" />
            {@render below('llm.model', t('settings.llm_model_help'))}
          </div>
        </div>
        {#if form.llm.backend === 'ollama'}
          <div class="field">
            <label for={id('llm.endpoint')}>{t('settings.llm_endpoint')}</label>
            <input id={id('llm.endpoint')} class="input mono" bind:value={form.llm.endpoint} aria-invalid={!!errors['llm.endpoint']} aria-describedby={describedby('llm.endpoint')} spellcheck="false" />
            {@render below('llm.endpoint', t('settings.llm_endpoint_help'))}
          </div>
        {/if}
        <div class="field">
          <label for={id('llm.context_tokens')}>{t('settings.llm_context_tokens')}</label>
          <input id={id('llm.context_tokens')} class="input tnum narrow" type="number" min="1024" step="1024" bind:value={form.llm.context_tokens} aria-invalid={!!errors['llm.context_tokens']} aria-describedby={describedby('llm.context_tokens')} />
          {@render below('llm.context_tokens', t('settings.llm_context_tokens_help'))}
        </div>
        <div class="field">
          <label for={id('summary_lang')}>{t('settings.summary_lang')}</label>
          <select id={id('summary_lang')} class="select narrow" bind:value={form.summary_lang} aria-describedby={describedby('summary_lang')}>
            {#each SUMMARY_LANGS.includes(form.summary_lang) ? SUMMARY_LANGS : [...SUMMARY_LANGS, form.summary_lang] as l (l)}
              <option value={l}>{SUMMARY_LANGS.includes(l) ? t(`settings.lang.${l}`) : l}</option>
            {/each}
          </select>
          {@render below('summary_lang', t('settings.summary_lang_help'))}
        </div>
      </section>

      <section class="panel group" aria-labelledby="g-ui">
        <h2 id="g-ui">{t('settings.group.interface')}</h2>
        <div class="field">
          <label for={id('ui_lang')}>{t('settings.ui_lang')}</label>
          <select id={id('ui_lang')} class="select narrow" value={uiLang} onchange={pickUiLang} aria-describedby={describedby('ui_lang')}>
            {#each UI_LANGS as l (l)}
              <option value={l}>{t(`settings.lang.${l}`)}</option>
            {/each}
          </select>
          {@render below('ui_lang', t('settings.ui_lang_help'))}
        </div>
      </section>

      <section class="panel group" aria-labelledby="g-server">
        <h2 id="g-server">{t('settings.group.server')}</h2>
        <div class="field">
          <label for={id('port')}>{t('settings.port')}</label>
          <input id={id('port')} class="input tnum narrow" type="number" min="1" max="65535" step="1" bind:value={form.port} aria-invalid={!!errors.port} aria-describedby={describedby('port')} />
          {@render below('port', t('settings.port_help'))}
        </div>
      </section>

      <div class="actions">
        {#if formError}
          <p class="field-err" role="alert">{t('settings.save_failed', { error: formError })}</p>
        {/if}
        {#if savedFlash}
          <span class="saved" role="status">{t('settings.saved')}</span>
        {/if}
        <button type="button" class="btn" onclick={reset} disabled={!dirty || saving}>{t('settings.reset')}</button>
        <button type="submit" class="btn primary" disabled={!dirty || !valid || saving}>
          {saving ? t('settings.saving') : t('common.save')}
        </button>
      </div>
    </form>
  {/if}
</div>

<style>
  .page {
    max-width: 720px;
    margin: 0 auto;
    padding: 24px var(--gutter) 48px;
  }

  h1 {
    font-size: 24px;
    margin-bottom: 24px;
  }

  form {
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  .group {
    display: flex;
    flex-direction: column;
    gap: 16px;
    padding: 16px;
  }

  h2 {
    font-size: 16px;
  }

  .field label {
    color: var(--text);
    font-size: 14px;
    font-weight: 500;
  }

  .field.check label {
    display: flex;
    align-items: center;
    gap: 8px;
    cursor: pointer;
  }

  .field.check input {
    width: 16px;
    height: 16px;
    margin: 0;
    accent-color: var(--accent);
  }

  .help {
    color: var(--text-3);
    font-size: 12px;
  }

  .field-err {
    margin: 0;
    color: var(--danger);
    font-size: 13px;
  }

  .input[aria-invalid='true'] {
    border-color: var(--danger);
  }

  .narrow {
    max-width: 240px;
  }

  .row2,
  .row3 {
    display: grid;
    gap: 16px;
  }

  .row2 {
    grid-template-columns: repeat(2, minmax(0, 1fr));
  }

  .row3 {
    grid-template-columns: repeat(3, minmax(0, 1fr));
  }

  .selftest {
    display: flex;
    flex-direction: column;
    gap: 16px;
    padding-top: 16px;
    border-top: 1px solid var(--border);
  }

  .selftest-head {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 12px;
  }

  .verdict {
    margin: 8px 0 0;
    padding: 8px 12px;
    border-radius: var(--radius-sm);
    font-size: 14px;
  }

  .verdict.ok {
    background: var(--ok-soft);
    color: var(--ok);
  }

  .verdict.warn {
    background: var(--orange-soft);
    color: var(--orange);
  }

  .restart {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 16px;
    padding: 12px 16px;
    border-radius: var(--radius);
    background: var(--orange-soft);
    color: var(--orange);
    font-size: 14px;
    font-weight: 500;
  }

  .actions {
    position: sticky;
    bottom: 0;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: flex-end;
    gap: 8px;
    padding: 12px 0;
    background: var(--bg);
  }

  .actions .field-err {
    margin-right: auto;
  }

  .saved {
    color: var(--ok);
    font-size: 14px;
  }

  @media (max-width: 639px) {
    .row2,
    .row3 {
      grid-template-columns: minmax(0, 1fr);
    }
  }
</style>
