<script lang="ts">
  import { onMount } from 'svelte';
  import Ask from '../components/Ask.svelte';
  import ScreenVideo from '../components/ScreenVideo.svelte';
  import StateChip from '../components/StateChip.svelte';
  import Summary from '../components/Summary.svelte';
  import Transcript from '../components/Transcript.svelte';
  import VaultSwitch from '../components/VaultSwitch.svelte';
  import Waveform from '../components/Waveform.svelte';
  import {
    audioUrl,
    deleteMeeting,
    getMeeting,
    patchMeeting,
    patchPerson,
    people,
    rerun,
    screenUrl,
  } from '../lib/api';
  import { clock, stateOfSteps } from '../lib/format';
  import { locale, t } from '../lib/i18n';
  import { ignoreKey } from '../lib/keys';
  import { live } from '../lib/live.svelte';
  import { canRenameInMeeting, renameRequest, renameSuggestions } from '../lib/people';
  import { link, meetingPath, navigate } from '../lib/router';
  import { shiftAfterDelete } from '../lib/transcript';
  import { STEPS, type MeetingDetail, type Person, type Segment, type Step } from '../lib/types';
  import { syncOn, vaultBusy, vaultState } from '../lib/vault';

  let { id, startMs = null }: { id: string; startMs?: number | null } = $props();

  let detail = $state<MeetingDetail | null>(null);
  /** The speaker registry, for which voices are still unnamed. Empty until
   *  it loads, which offers no rename. */
  let registry = $state<Person[]>([]);
  let error = $state<string | null>(null);
  let notice = $state<{ text: string; kind: 'ok' | 'err' } | null>(null);

  let audio = $state<HTMLAudioElement>();
  let paused = $state(true);
  let currentMs = $state(0);
  let cursor = $state<number | null>(null);
  let editing = $state<number | null>(null);
  /** A segment PATCH is in flight. Segment edits address lines by index, so
   *  they run one at a time to keep each index valid for the response. */
  let pending = $state(false);

  let titleEditing = $state(false);
  let titleDraft = $state('');
  let rerunStep = $state<Step>('transcribe');

  const meeting = $derived(detail?.meeting ?? null);
  const segments = $derived(meeting?.segments ?? []);

  /** Numbers loads; an answer older than the latest request is dropped. */
  let loadSeq = 0;

  async function load(withRegistry = true) {
    if (withRegistry) void loadRegistry();
    const mine = ++loadSeq;
    try {
      const fetched = await getMeeting(id);
      if (mine !== loadSeq) return;
      detail = fetched;
      error = null;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function loadRegistry() {
    try {
      registry = await people();
    } catch {
      // Without the registry the page offers no rename; nothing else needs it.
      registry = [];
    }
  }

  onMount(() => {
    load();
  });

  // Reload when the daemon finishes or fails this meeting.
  let seenVersion = live.version;
  $effect(() => {
    const v = live.version;
    if (v !== seenVersion && live.changed === id) {
      seenVersion = v;
      load();
    }
  });

  // While processing, every progress event brings more of the saved
  // transcript; reload it so the lines appear as they are transcribed.
  const progress = $derived(live.progress[id]);
  $effect(() => {
    if (progress && !pending) void load(false);
  });

  $effect(() => {
    if (meeting) document.title = `${meeting.title} · recall`;
  });

  // Follow the playhead smoothly while playing; `timeupdate` alone fires
  // only about four times a second.
  $effect(() => {
    if (paused || !audio) return;
    let raf = 0;
    const loop = () => {
      if (audio) currentMs = audio.currentTime * 1000;
      raf = requestAnimationFrame(loop);
    };
    raf = requestAnimationFrame(loop);
    return () => cancelAnimationFrame(raf);
  });

  /** Index of the last line starting at or before `ms`. */
  function lineAt(ms: number): number {
    let lo = 0;
    let hi = segments.length - 1;
    let found = -1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (segments[mid].start_ms <= ms) {
        found = mid;
        lo = mid + 1;
      } else {
        hi = mid - 1;
      }
    }
    return found;
  }

  // While playing the active line follows the audio; when paused it stays
  // where `[` `]` or a click put it.
  const playingLine = $derived(lineAt(currentMs));
  const active = $derived(!paused || cursor === null ? playingLine : cursor);
  $effect(() => {
    if (!paused) cursor = null;
  });

  function seek(ms: number, play = true) {
    if (!audio) return;
    audio.currentTime = ms / 1000;
    currentMs = ms;
    if (play) audio.play().catch(() => {});
  }

  function playLine(i: number) {
    cursor = i;
    seek(segments[i].start_ms);
  }

  function metadataLoaded() {
    if (startMs !== null) {
      seek(startMs, false);
      cursor = lineAt(startMs);
    }
  }

  // Known speaker ids in order of first appearance, for "next speaker".
  const speakers = $derived.by(() => {
    if (!detail) return [];
    const out: string[] = [];
    const add = (s: string | null | undefined) => {
      if (s && !out.includes(s)) out.push(s);
    };
    for (const s of segments) add(s.speaker);
    for (const c of detail.meeting.clusters) add(c.speaker);
    for (const s of Object.keys(detail.names)) add(s);
    return out;
  });

  function nameOf(seg: Segment): string {
    const names = detail?.names ?? {};
    if (seg.speaker) {
      if (names[seg.speaker]) return names[seg.speaker];
      if (seg.speaker === 'me') return t('speaker.me');
      return t('speaker.unknown');
    }
    return seg.side === 'me' ? t('speaker.me') : t('speaker.them');
  }

  /** Speakers whose cluster took its name from Google Meet. */
  const meetNamed = $derived(
    new Set(
      (meeting?.clusters ?? [])
        .filter((c) => c.named_by === 'meet' && c.speaker)
        .map((c) => c.speaker as string),
    ),
  );

  function suggestions(speaker: string): string[] {
    return detail ? renameSuggestions(detail.meeting.participants_hint, detail.names, speaker) : [];
  }

  /** Names the unnamed voice `speaker`, or merges it into the named person
   *  who already goes by `name`. */
  async function renameSpeaker(speaker: string, name: string) {
    try {
      await patchPerson(speaker, renameRequest(speaker, name, registry));
    } catch (e) {
      flash(t('meeting.save_failed', { error: e instanceof Error ? e.message : String(e) }), 'err');
      throw e;
    }
    await load();
  }

  function flash(text: string, kind: 'ok' | 'err' = 'ok') {
    notice = { text, kind };
    setTimeout(() => {
      if (notice?.text === text) notice = null;
    }, 3000);
  }

  async function patch(body: Parameters<typeof patchMeeting>[1]) {
    if (!detail) return;
    if (pending) throw new Error('a segment edit is already in flight');
    pending = true;
    try {
      detail.meeting = await patchMeeting(id, body);
      // The edit queues an update of the published note; show it.
      if (vaultOn) void load(false);
    } catch (e) {
      flash(t('meeting.save_failed', { error: e instanceof Error ? e.message : String(e) }), 'err');
      throw e;
    } finally {
      pending = false;
    }
  }

  async function saveText(index: number, text: string) {
    await patch({ segments: [{ index, text }] });
  }

  function nextSpeaker(index: number) {
    if (speakers.length < 2 || pending) return;
    const cur = segments[index].speaker;
    const at = cur ? speakers.indexOf(cur) : -1;
    const speaker = speakers[(at + 1) % speakers.length];
    patch({ segments: [{ index, speaker }] }).catch(() => {});
  }

  function deleteLine(index: number) {
    // The Transcript disables delete while a line is open; this guards the
    // same rule for any other caller.
    if (pending || editing !== null) return;
    patch({ segments: [{ index, delete: true }] })
      .then(() => {
        editing = shiftAfterDelete(editing, index);
        cursor = shiftAfterDelete(cursor, index);
      })
      .catch(() => {});
  }

  function startTitleEdit() {
    if (!meeting) return;
    titleDraft = meeting.title;
    titleEditing = true;
  }

  async function saveTitle() {
    const title = titleDraft.trim();
    if (!meeting || !title || title === meeting.title) {
      titleEditing = false;
      return;
    }
    // A segment edit in flight would return the old title, or this response
    // would overwrite its segments; keep the editor open until it lands.
    if (pending) return;
    titleEditing = false;
    pending = true;
    try {
      const updated = await patchMeeting(id, { title });
      // Renaming moves the meeting directory, which changes the id.
      if (updated.id !== id) navigate(meetingPath(updated.id), { replace: true });
      else if (detail) detail.meeting = updated;
    } catch (e) {
      flash(t('meeting.save_failed', { error: e instanceof Error ? e.message : String(e) }), 'err');
    } finally {
      pending = false;
    }
  }

  function titleKey(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.isComposing) saveTitle();
    else if (e.key === 'Escape') titleEditing = false;
  }

  async function doRerun() {
    try {
      await rerun(id, rerunStep);
      flash(t('meeting.rerun_queued', { step: t(`step.${rerunStep}`) }));
    } catch (e) {
      flash(t('meeting.rerun_failed', { error: e instanceof Error ? e.message : String(e) }), 'err');
    }
  }

  /** A publish switch PATCH is in flight. */
  let syncSaving = $state(false);

  async function setSync(on: boolean) {
    syncSaving = true;
    try {
      await patchMeeting(id, { sync: on });
      await load(false);
    } catch (e) {
      flash(t('vault.save_failed', { error: e instanceof Error ? e.message : String(e) }), 'err');
    } finally {
      syncSaving = false;
    }
  }

  async function doDelete() {
    const key = detail?.vault?.path ? 'meeting.delete_confirm_vault' : 'meeting.delete_confirm';
    if (!meeting || !confirm(t(key, { title: meeting.title }))) return;
    try {
      await deleteMeeting(id);
      navigate('/', { replace: true });
    } catch (e) {
      flash(t('meeting.delete_failed', { error: e instanceof Error ? e.message : String(e) }), 'err');
    }
  }

  function keydown(e: KeyboardEvent) {
    if (ignoreKey(e) || !audio || editing !== null || titleEditing) return;
    const last = segments.length - 1;
    switch (e.key) {
      case ' ':
        // Leave space to focused buttons, links and the audio controls.
        if (
          e.target instanceof HTMLButtonElement ||
          e.target instanceof HTMLAnchorElement ||
          e.target instanceof HTMLMediaElement
        ) {
          return;
        }
        if (audio.paused) audio.play().catch(() => {});
        else audio.pause();
        break;
      case '[':
        if (last >= 0) playLine(Math.max(0, active - 1));
        break;
      case ']':
        if (last >= 0) playLine(Math.min(last, active + 1));
        break;
      case 'e':
        if (active >= 0 && !pending) editing = active;
        break;
      case 's':
        if (active >= 0 && !pending) nextSpeaker(active);
        break;
      default:
        return;
    }
    e.preventDefault();
  }

  const started = $derived(
    meeting
      ? new Date(meeting.started).toLocaleString(locale(), {
          dateStyle: 'medium',
          timeStyle: 'short',
        })
      : '',
  );
  // A progress event names the step running now, which the loaded steps
  // only know as of the last load; done/failed reload the meeting.
  const meetingState = $derived(
    progress ? `processing:${progress.step}` : meeting ? stateOfSteps(meeting.steps) : 'done',
  );
  const summarizing = $derived(meetingState !== 'done' && !meetingState.startsWith('failed'));

  const vault = $derived(detail?.vault ?? null);
  const vaultOn = $derived(vault && meeting ? syncOn(meeting.sync, vault) : false);
  const vaultNow = $derived(vault ? vaultState(vaultOn, meetingState, vault) : 'off');
  // The push runs on the daemon's own schedule and sends no event; poll
  // while it is on its way, and now and then while it waits for a retry.
  $effect(() => {
    if (!vaultBusy(vaultNow) && vaultNow !== 'failed') return;
    const timer = setInterval(
      () => {
        if (!pending && !syncSaving) void load(false);
      },
      vaultNow === 'failed' ? 30_000 : 3_000,
    );
    return () => clearInterval(timer);
  });
</script>

<svelte:window onkeydown={keydown} />

<div class="page">
  <a class="back" href="/" onclick={link}>
    <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
      <path d="M10 3 5 8l5 5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" />
    </svg>
    {t('common.back_to_list')}
  </a>

  {#if error}
    <div class="error" role="alert">{t('meeting.load_failed', { error })}</div>
  {:else if !meeting}
    <p class="muted">{t('common.loading')}</p>
  {:else}
    <header class="head">
      <div class="title-row">
        {#if titleEditing}
          <!-- svelte-ignore a11y_autofocus -->
          <input
            class="input title-input"
            bind:value={titleDraft}
            onkeydown={titleKey}
            onblur={saveTitle}
            aria-label={t('meeting.title_label')}
            autofocus
          />
        {:else}
          <h1>
            <button class="title-btn" onclick={startTitleEdit} title={t('meeting.rename')}>
              {meeting.title || t('meeting.untitled')}
            </button>
          </h1>
        {/if}
        <StateChip state={meetingState} pct={progress?.pct ?? null} />
      </div>
      <div class="meta">
        <span>{started}</span>
        <span aria-hidden="true">·</span>
        <span class="tnum">{clock(meeting.duration_ms)}</span>
        {#if meeting.source_app}
          <span aria-hidden="true">·</span>
          <span>{meeting.source_app}</span>
        {/if}
        {#if meeting.screen_error}
          <span aria-hidden="true">·</span>
          <span class="screen-error" title={meeting.screen_error}>
            {t('meeting.screen_error', { error: meeting.screen_error })}
          </span>
        {/if}
        <span class="spacer"></span>
        <label class="rerun">
          <span class="sr-only">{t('meeting.rerun_step')}</span>
          <select class="select small" bind:value={rerunStep}>
            {#each STEPS as s (s)}
              <option value={s}>{t(`step.${s}`)}</option>
            {/each}
          </select>
          <button class="btn" onclick={doRerun}>{t('meeting.rerun')}</button>
        </label>
        <button class="btn danger" onclick={doDelete}>{t('meeting.delete')}</button>
      </div>
      {#if notice}
        <div class="notice {notice.kind}" role="status">{notice.text}</div>
      {/if}
    </header>

    <div class="cols">
      <aside class="left">
        <Summary
          summary={meeting.summary}
          pending={summarizing}
          {currentMs}
          onseek={(ms) => {
            cursor = lineAt(ms);
            seek(ms);
          }}
        />
      </aside>

      <div class="middle">
        {#if detail?.has_screen}
          <section class="panel screen" aria-label={t('player.screen')}>
            <ScreenVideo
              src={screenUrl(id)}
              offsetMs={meeting.screen_offset_ms ?? 0}
              {currentMs}
              {paused}
              onplay={() => audio?.play().catch(() => {})}
              onpause={() => audio?.pause()}
              onseek={(ms) => {
                cursor = lineAt(ms);
                seek(ms, false);
              }}
            />
          </section>
        {/if}
        <section class="transcript-panel panel" aria-label={t('transcript.title')}>
          <div class="mid-head">
            <h2 class="panel-title">{t('transcript.title')}</h2>
            <span class="muted keys" title={t('keys.meeting')}>
              <kbd>{t('keys.space_key')}</kbd> <kbd>[</kbd> <kbd>]</kbd> <kbd>e</kbd> <kbd>s</kbd>
              <span class="sr-only">{t('keys.meeting')}</span>
            </span>
          </div>
          <Transcript
            {segments}
            {nameOf}
            {active}
            playing={!paused}
            bind:editing
            canCycle={speakers.length > 1}
            busy={pending}
            onplay={playLine}
            onsave={saveText}
            onnextspeaker={nextSpeaker}
            ondelete={deleteLine}
            renameable={(speaker) => canRenameInMeeting(speaker, registry)}
          {suggestions}
            viaMeet={(speaker) => meetNamed.has(speaker)}
            onrename={renameSpeaker}
          />
        </section>
      </div>

      <aside class="right">
        {#if vault}
          <VaultSwitch
            on={vaultOn}
            processing={meetingState}
            status={vault}
            busy={syncSaving}
            onchange={setSync}
          />
        {/if}
        <section class="panel player">
          <Waveform
            src={audioUrl(id)}
            durationMs={meeting.duration_ms}
            {currentMs}
            chapters={meeting.summary?.chapters ?? []}
            onseek={(ms) => {
              cursor = lineAt(ms);
              seek(ms);
            }}
          />
          <audio
            bind:this={audio}
            bind:paused
            src={audioUrl(id)}
            preload="metadata"
            controls
            ontimeupdate={() => audio && (currentMs = audio.currentTime * 1000)}
            onloadedmetadata={metadataLoaded}
          ></audio>
        </section>
        <Ask {id} />
      </aside>
    </div>
  {/if}
</div>

<style>
  .page {
    max-width: var(--max);
    margin: 0 auto;
    padding: 16px var(--gutter) 48px;
  }

  .back {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    margin-left: -4px;
    padding: 4px 8px 4px 4px;
    border-radius: var(--radius-sm);
    color: var(--text-2);
    font-size: 14px;
    transition:
      background 120ms,
      color 120ms;
  }

  .back:hover {
    background: var(--hover);
    color: var(--text);
  }

  .head {
    margin: 8px 0 24px;
  }

  .title-row {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .title-row {
    flex-wrap: wrap;
  }

  h1 {
    font-size: 24px;
    letter-spacing: -0.01em;
  }

  .title-btn {
    margin-left: -8px;
    padding: 0 8px;
    border: 0;
    border-radius: var(--radius-sm);
    background: none;
    font: inherit;
    text-align: left;
    cursor: text;
    transition: background 120ms;
  }

  .title-btn:hover {
    background: var(--hover);
  }

  .title-input {
    max-width: 640px;
    height: 40px;
    font-size: 20px;
    font-weight: 600;
  }

  .meta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
    margin-top: 8px;
    font-size: 14px;
    color: var(--text-2);
  }

  .spacer {
    flex: 1;
  }

  .rerun {
    display: flex;
    gap: 8px;
  }

  .select.small {
    width: auto;
    height: 32px;
    padding: 0 8px;
  }

  .notice {
    display: inline-block;
    margin-top: 8px;
    padding: 4px 12px;
    border-radius: var(--radius-sm);
    font-size: 14px;
  }

  .notice.ok {
    background: var(--ok-soft);
    color: var(--ok);
  }

  .notice.err {
    background: var(--danger-soft);
    color: var(--danger);
  }

  .cols {
    display: grid;
    grid-template-columns: 320px minmax(0, 1fr) 360px;
    grid-template-areas: 'left middle right';
    gap: 24px;
    align-items: start;
  }

  .left {
    grid-area: left;
    position: sticky;
    top: 72px;
    max-height: calc(100vh - 96px);
    overflow-y: auto;
  }

  .middle {
    grid-area: middle;
    display: flex;
    flex-direction: column;
    gap: 16px;
    min-width: 0;
  }

  .screen {
    padding: 8px;
  }

  .transcript-panel {
    padding: 8px;
  }

  .screen-error {
    max-width: 320px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--orange);
  }

  .mid-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 8px 8px 0;
  }

  .keys {
    display: flex;
    gap: 4px;
  }

  .right {
    grid-area: right;
    position: sticky;
    top: 72px;
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  .player {
    display: flex;
    flex-direction: column;
    gap: 24px;
    padding: 16px;
  }

  audio {
    width: 100%;
    height: 40px;
  }

  @media (max-width: 1023px) {
    .cols {
      grid-template-columns: minmax(0, 1fr);
      grid-template-areas: 'right' 'left' 'middle';
    }

    .left,
    .right {
      position: static;
      max-height: none;
    }

    .keys {
      display: none;
    }
  }
</style>
