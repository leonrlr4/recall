<script lang="ts">
  import { t } from '../lib/i18n';
  import { audioMsAt, catchUpRate, videoCorrection, videoSeconds } from '../lib/player';

  let {
    src,
    offsetMs,
    currentMs,
    paused,
    onplay,
    onpause,
    onseek,
  }: {
    src: string;
    /** `screen_offset_ms`; 0 when the meeting does not record one. */
    offsetMs: number;
    /** The audio position, which the video follows. */
    currentMs: number;
    /** The audio is paused. */
    paused: boolean;
    onplay: () => void;
    onpause: () => void;
    /** The user moved the video; the audio should move to this position. */
    onseek: (ms: number) => void;
  } = $props();

  let video = $state<HTMLVideoElement>();
  // Plays, pauses and seeks this component started and whose events have
  // not arrived yet. Their events are not the user working the video's
  // controls, so they are not passed on to the audio.
  let ownPlays = 0;
  let ownPauses = 0;
  let ownSeeks = 0;

  function moveTo(seconds: number) {
    if (!video) return;
    ownSeeks++;
    video.currentTime = seconds;
  }

  // Follow the audio clock: move the video when it drifts, and play it only
  // while the audio plays inside the recorded span of the screen.
  $effect(() => {
    if (!video) return;
    const to = videoCorrection(video.currentTime, currentMs, offsetMs, video.duration);
    if (to !== null) moveTo(to);
    const started = currentMs >= offsetMs;
    video.playbackRate =
      to === null && !paused && started ? catchUpRate(video.currentTime, currentMs, offsetMs) : 1;
    if (!paused && started && video.paused && !video.ended) {
      ownPlays++;
      video.play().catch(() => {});
    } else if ((paused || !started) && !video.paused) {
      ownPauses++;
      video.pause();
    }
  });

  function played() {
    if (ownPlays > 0) {
      ownPlays--;
      return;
    }
    // Before the screen started the effect pauses the video again at once.
    if (paused) onplay();
  }

  function pausedHere() {
    if (ownPauses > 0) {
      ownPauses--;
      return;
    }
    // Reaching its end is not a request to stop the audio.
    if (!paused && video && !video.ended) onpause();
  }

  function seeked() {
    if (ownSeeks > 0) {
      ownSeeks--;
      return;
    }
    if (!video) return;
    if (videoCorrection(video.currentTime, currentMs, offsetMs, video.duration) !== null) {
      onseek(audioMsAt(video.currentTime, offsetMs));
    }
  }

  function metadataLoaded() {
    moveTo(videoSeconds(currentMs, offsetMs));
  }
</script>

<!-- The audio element carries the sound; the video is muted and follows it. -->
<video
  bind:this={video}
  {src}
  muted
  playsinline
  controls
  preload="metadata"
  aria-label={t('player.screen')}
  onplay={played}
  onpause={pausedHere}
  onseeked={seeked}
  onloadedmetadata={metadataLoaded}
></video>

<style>
  video {
    display: block;
    width: 100%;
    max-height: 60vh;
    border-radius: var(--radius-sm);
    background: #000;
  }
</style>
