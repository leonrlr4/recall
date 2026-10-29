import { describe, expect, it } from 'vitest';
import { applyPatch, askErrorKey, configDiff, fieldOfError, mergePatch, selftestVerdict, uiLangOf } from './settings';
import type { Config } from './types';

const base: Config = {
  meetings_dir: '/home/a/Meetings',
  data_dir: '/home/a/.local/share/recall',
  record_screen: false,
  allowlist: ['zoom', 'firefox'],
  debounce_secs: 8,
  end_grace_secs: 30,
  llm: { backend: 'ollama', model: 'qwen', endpoint: 'http://127.0.0.1:11434', context_tokens: 32768 },
  summary_lang: 'zh-TW',
  stt: { engine: 'breeze', threads: 8 },
  port: 7474,
};

const clone = (c: Config): Config => structuredClone(c);

describe('selftestVerdict', () => {
  const loud = [0, 0.01, 0.4, 0.2];
  const quiet = [0, 0.01, 0.02, 0.005];

  it('selftest_verdict_ok_when_both_tracks_have_signal', () => {
    expect(selftestVerdict(loud, loud)).toBe('ok');
  });

  it('selftest_verdict_flags_the_silent_track', () => {
    expect(selftestVerdict(loud, quiet)).toBe('sys_silent');
    expect(selftestVerdict(quiet, loud)).toBe('mic_silent');
    expect(selftestVerdict(quiet, quiet)).toBe('both_silent');
  });

  it('selftest_verdict_treats_empty_tracks_as_silent', () => {
    expect(selftestVerdict([], loud)).toBe('mic_silent');
  });
});

describe('configDiff', () => {
  it('config_diff_is_empty_when_nothing_changed', () => {
    expect(configDiff(base, clone(base))).toEqual({});
  });

  it('config_diff_sends_only_changed_fields_and_section_keys', () => {
    const next = clone(base);
    next.port = 8000;
    next.llm.model = 'llama';
    next.allowlist = ['zoom'];
    expect(configDiff(base, next)).toEqual({
      port: 8000,
      llm: { model: 'llama' },
      allowlist: ['zoom'],
    });
  });

  it('config_diff_sends_null_to_clear_ui_lang', () => {
    const before = { ...clone(base), ui_lang: 'en' };
    const after = { ...clone(base), ui_lang: null };
    expect(configDiff(before, after)).toEqual({ ui_lang: null });
    expect(configDiff(clone(base), { ...clone(base), ui_lang: null })).toEqual({});
  });
});

describe('settings helpers', () => {
  it('ui_lang_of_maps_unset_and_unknown_to_auto', () => {
    expect(uiLangOf(undefined)).toBe('auto');
    expect(uiLangOf(null)).toBe('auto');
    expect(uiLangOf('fr')).toBe('auto');
    expect(uiLangOf('en')).toBe('en');
  });

  it('field_of_error_finds_the_named_key', () => {
    expect(fieldOfError('port must not be 0')).toBe('port');
    expect(fieldOfError('meetings_dir must be an absolute path, got x')).toBe('meetings_dir');
    expect(fieldOfError('unknown config key "llm.modle"')).toBe(null);
    expect(fieldOfError('invalid config: invalid type for stt.threads')).toBe('stt.threads');
    expect(fieldOfError('disk full')).toBe(null);
    expect(fieldOfError('invalid config: invalid type for llm.context_tokens')).toBe('llm.context_tokens');
  });

  it('ask_error_key_translates_the_context_error_only', () => {
    expect(
      askErrorKey('this meeting is too long for the current context; raise llm.context_tokens'),
    ).toBe('ask.too_long');
    expect(askErrorKey('Ollama returned 404')).toBe(null);
  });
});

describe('applyPatch', () => {
  it('apply_patch_merges_sections_key_by_key', () => {
    const out = applyPatch(base, { port: 8000, llm: { model: 'llama' } });
    expect(out.port).toBe(8000);
    expect(out.llm).toEqual({ ...base.llm, model: 'llama' });
    expect(base.port).toBe(7474);
  });

  it('apply_patch_null_ui_lang_removes_it', () => {
    const out = applyPatch({ ...clone(base), ui_lang: 'en' }, { ui_lang: null });
    expect('ui_lang' in out).toBe(false);
  });

  it('merge_patch_keeps_both_sides_and_later_wins', () => {
    expect(
      mergePatch({ port: 1, llm: { model: 'a' } }, { llm: { endpoint: 'e' }, port: 2 }),
    ).toEqual({ port: 2, llm: { model: 'a', endpoint: 'e' } });
  });

  it('diff_of_applied_patch_round_trips', () => {
    const patch = { allowlist: ['zoom'], stt: { threads: 4 } };
    expect(configDiff(base, applyPatch(base, patch))).toEqual(patch);
  });
});
