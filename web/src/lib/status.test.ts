import { describe, expect, it } from 'vitest';
import en from '../i18n/en.json';
import { format } from './i18n';
import { recordActions, statusAnnouncement, statusChipText, type Snapshot } from './status';

/** `t` over the English strings, so the expectations read as the UI does. */
function t(key: string, params?: Record<string, string | number>): string {
  const s = key
    .split('.')
    .reduce<unknown>((o, p) => (o as Record<string, unknown> | undefined)?.[p], en);
  if (typeof s !== 'string') throw new Error(`missing key ${key}`);
  return params ? format(s, params) : s;
}

const NOW = 1_800_000_000_000;
const snap = (phase: string, source_app: string | null = 'zoom', since_ms = NOW): Snapshot => ({
  phase,
  since_ms,
  source_app,
});

describe('status', () => {
  it('status_chip_text_for_each_phase', () => {
    expect(statusChipText(snap('idle', null), {}, NOW, t)).toEqual({ text: 'Idle', tone: 'idle' });
    expect(statusChipText(null, {}, NOW, t)).toEqual({ text: 'Idle', tone: 'idle' });
    expect(statusChipText(snap('armed'), {}, NOW, t)).toEqual({
      text: 'Detected zoom, starting…',
      tone: 'armed',
    });
    expect(statusChipText(snap('armed', null), {}, NOW, t).text).toBe('Meeting detected, starting…');
    expect(statusChipText(snap('recording', 'zoom', NOW - 125_000), {}, NOW, t)).toEqual({
      text: 'Recording zoom · 02:05',
      tone: 'recording',
    });
    expect(statusChipText(snap('recording', null, NOW - 3_725_000), {}, NOW, t).text).toBe(
      'Recording · 1:02:05',
    );
    // A clock slightly behind the daemon's never shows negative time.
    expect(statusChipText(snap('recording', 'zoom', NOW + 900), {}, NOW, t).text).toBe(
      'Recording zoom · 00:00',
    );
    expect(statusChipText(snap('finishing'), {}, NOW, t)).toEqual({
      text: 'Finishing…',
      tone: 'finishing',
    });
  });

  it('status_chip_shows_processing_only_without_a_recording', () => {
    const progress = { 'm1': { step: 'transcribe' as const, pct: 0.42 } };
    expect(statusChipText(snap('idle', null), progress, NOW, t)).toEqual({
      text: 'Processing · Transcribing 42%',
      tone: 'processing',
    });
    expect(statusChipText(null, progress, NOW, t).tone).toBe('processing');
    expect(statusChipText(snap('recording', 'zoom', NOW), progress, NOW, t).tone).toBe('recording');
    expect(statusChipText(snap('armed'), progress, NOW, t).tone).toBe('armed');
    expect(statusChipText(snap('finishing'), progress, NOW, t).tone).toBe('finishing');
  });

  it('status_chip_follows_the_most_recently_started_job', () => {
    const progress = {
      a: { step: 'summarize' as const, pct: 0.9 },
      b: { step: 'segment' as const, pct: 0.1 },
    };
    expect(statusChipText(snap('idle', null), progress, NOW, t).text).toBe(
      'Processing · Segmenting 10%',
    );
  });

  it('record_actions_per_phase', () => {
    expect(recordActions('idle')).toEqual(['start']);
    expect(recordActions('armed')).toEqual(['start']);
    expect(recordActions('recording')).toEqual(['stop', 'discard']);
    expect(recordActions('finishing')).toEqual([]);
  });

  it('status_announcement_leaves_out_changing_numbers', () => {
    const progress = { m1: { step: 'transcribe' as const, pct: 0.42 } };
    expect(statusAnnouncement(snap('idle', null), {}, t)).toBe('Idle');
    expect(statusAnnouncement(snap('armed'), {}, t)).toBe('Detected zoom, starting…');
    expect(statusAnnouncement(snap('recording', 'zoom', NOW - 125_000), progress, t)).toBe(
      'Recording zoom',
    );
    expect(statusAnnouncement(snap('recording', null), {}, t)).toBe('Recording');
    expect(statusAnnouncement(snap('finishing'), {}, t)).toBe('Finishing…');
    expect(statusAnnouncement(snap('idle', null), progress, t)).toBe('Processing · Transcribing');
    // The same phase gives the same string, so a live region stays quiet.
    expect(statusAnnouncement(snap('recording', 'zoom', NOW - 1_000), {}, t)).toBe(
      statusAnnouncement(snap('recording', 'zoom', NOW - 9_000), {}, t),
    );
  });
});
