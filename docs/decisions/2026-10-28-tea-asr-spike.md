# TEA-ASR-1.1 spike: no-go

Decision: keep Breeze-ASR-25 q8_0 as the transcription engine. TEA-ASR-1.1
meets the speed and stability criteria but misses the accuracy criterion
(3.7 points better, against the required 5), and as measured it misses the
memory criterion too. Linking it beside whisper-rs crashes, so a switch
would also mean dropping whisper-rs or running TEA in its own process.

## Setup

| Item | Value |
|---|---|
| Machine | Intel Core Ultra X7 368H (16 threads), 62 GB RAM, CPU only (no Vulkan headers) |
| TEA runtime | CrispASR at commit `34a8e43` (VERSION 0.8.46), Rust crate `crispasr` as a path dependency, shared `libcrispasr` built with CMake Release, `-march=native`, OpenMP, CBLAS |
| TEA model | `dseditor/TEA-ASR-1.1-CrispASR-GGUF` `tea-asr-1.1-q8_0.gguf` (2.51 GB, sha256 `addf14b8...4f4153`), backend auto-detected as `qwen3` |
| Aligner | `cstr/qwen3-forced-aligner-0.6b-GGUF` `qwen3-forced-aligner-0.6b-q5_0.gguf` through `crispasr::align_words` |
| TEA settings | 8 threads, `use_gpu: false`, flash attention on, language hint `zh` |
| Breeze baseline | `recall transcribe-file` built from a clean export of `9c5b6bf`, `ggml-breeze-asr-25-q8_0.bin`, whisper-rs 0.16, 8 threads (the config default) |
| Fallback | Not needed: `crispasr` built and loaded the model on the first attempt |

TEA ran with the `zh` hint because it scored better than auto-detection on
the first ASCEND session (50 clips): 9.7% against 13.5% mixed error rate.
Breeze also runs with `zh`. That session (ascend-00) is also one of the
scored sessions, so the choice slightly favours TEA on ASCEND.

### Test set

- ASCEND test split (CC BY-SA 4.0): 300 of 1,315 clips, stratified by the
  dataset's language label (156 `zh`, 85 `mixed`, 59 `en`), 700 s of speech.
- CSZS zh-en test split (MIT), first parquet shard: 300 random `correct`
  utterances, 1,384 s of speech. CSZS is synthesised speech, ASCEND is
  spontaneous conversation.
- Seed 11. Clips are joined into 12 sessions of 50 clips with 2 s of silence
  between clips (3,307 s in total). Each session goes through
  `transcribe-file` as the `them` track with a silent `mic` track.
- TEA gets exactly the same audio and the same cuts: a scratch binary built
  from the same export makes the stereo Opus file the way `transcribe-file`
  does, decodes it with `decode_stereo`, and runs `SileroVad` plus
  `merge_close(300 ms, 30 s)`. That gives 655 regions with 1,902 s of
  speech. TEA transcribes each region in its own call. Breeze glues regions
  into 28 s windows, as the pipeline does.

### Scoring

`bench/run.py` has no mixed error rate, so the spike reuses its
`tokenize()` (one token per CJK character, one per lowercase word,
punctuation stripped). Both reference and hypothesis go through OpenCC
`t2s` first, because the references are Simplified and both engines write
Traditional. Mixed error rate = (substitutions + deletions + insertions) /
reference tokens, from a Levenshtein alignment of each whole session (all
transcript lines in time order against all reference clips in order),
summed over sessions. Scoring whole sessions avoids having to assign
Breeze's lines to clips: diarization and merging shift line boundaries
across clips. `bench/run.py`'s own `found` metric is reported next to it.

## Results

Accuracy (measured):

| Set | Ref tokens | Breeze MER | TEA MER | Gap (points) | Breeze del / sub / ins | TEA del / sub / ins | Breeze found | TEA found |
|---|---|---|---|---|---|---|---|---|
| ASCEND (300 clips) | 2,482 | 19.5% | 13.0% | 6.5 | 11.8 / 6.5 / 1.3 | 4.9 / 6.8 / 1.3 | 0.820 | 0.885 |
| CSZS (300 clips) | 4,576 | 12.0% | 9.9% | 2.1 | 1.8 / 9.9 / 0.4 | 1.8 / 7.3 / 0.7 | 0.884 | 0.908 |
| Pooled (600 clips) | 7,058 | 14.7% | 11.0% | 3.7 | | | | |

The 95% interval of the pooled gap, bootstrapped over the 12 sessions, is
2.0 to 5.4 points. TEA was better in 10 of 12 sessions; it was worse in two
CSZS sessions (by 0.5 and 1.4 points). Most of TEA's ASCEND advantage is
deletions: Breeze drops many short spontaneous utterances that TEA keeps.
Assigning transcript lines to clips by midpoint, 72 of the 300 ASCEND clips
get no Breeze text and 21 get no TEA text (none on CSZS for either). That
count is approximate for Breeze, whose diarized lines can cross clip
boundaries.

The aligner printed "alignment collapse suspected" (too many zero-length or
zero-position words) for 21 of the 655 regions, 17 of them in ASCEND
sessions. Those regions keep their text; only their word times are suspect,
and MER does not use word times.

Speed (measured, wall clock). Other agents were building and testing on the
same machine throughout, with a 1-minute load average between 6 and 18 on 16
threads, so these numbers are pessimistic for both engines. Each row names
its denominator: TEA's session rows divide by VAD speech (the audio TEA was
given), Breeze's ASR row by the same VAD speech, Breeze's whole-pipeline row
by session length, and TEA's single-call row, run in whole mode on
continuous clips without VAD, by clip length:

| Measure | Breeze | TEA |
|---|---|---|
| ASR seconds / VAD speech seconds, all 12 sessions | 0.66 (0.58 without one outlier session at 2.10) | 0.34 |
| ASR + aligner / VAD speech | | 0.55 (worst session 0.89) |
| Whole `transcribe-file` (VAD, ASR, diarization, naming) / session length | 0.45 | |
| Single call on continuous speech of 37 to 121 s, ASR + aligner / clip length | | 0.40 to 0.53 |
| CPU seconds for all 12 sessions | 11,148 (whole pipeline) | 6,184 (ASR + aligner) |

Breeze's ASR time is the gap between the `segment done` and
`transcribe done` lines on stderr. The ASCEND sessions run slower for TEA
(0.30 to 0.54 ASR-only) than CSZS because their regions are short and each
call has a fixed cost.

Memory (measured, peak RSS from `wait4`, MiB). `ru_maxrss` counts the
memory-mapped pages of the model files that were touched, so part of each
number is file-backed and reclaimable. Both engines are measured the same
way.

| Run | Peak RSS |
|---|---|
| Breeze `transcribe-file`, whole pipeline | 2,546 to 2,594 |
| TEA + aligner both loaded, per-region (≤ 30 s), 12 sessions | 4,300 to 4,728 |
| TEA alone, no aligner (one session) | 3,032 |
| TEA over all regions, session dropped, then aligner (one session, cszs-03) | 2,826 (wall 139.9 s, against 123.5 s for the same session with both loaded) |
| TEA + aligner on one 121 s call | 7,929 |

The aligner's memory grows with segment length; the pipeline never hands
over more than 30 s, so the last row only applies outside it.

Stability (measured):

| Input | Breeze | TEA |
|---|---|---|
| 1 h white noise at about -60 dBFS through the pipeline VAD | 0 regions, 0 lines | 0 regions, nothing to transcribe |
| 1 h digital silence through the pipeline VAD | 0 regions, 0 lines | 0 regions, nothing to transcribe |
| 1 h noise, no VAD, 120 calls of 30 s | not run | 0 lines |
| 1 h digital silence, no VAD, 120 calls of 30 s | not run | `嗯` in all 120 windows, nothing else |
| Continuous speech 37, 48, 62, 90, 121 s, one call each | not run | No loops (no 4-gram repeated more than twice), no empty output, MER 6.9% to 14.6%, deletions 0.6% to 5.1% |

`嗯` on digital zeros is a one-token hallucination, not a loop, and the
pipeline's VAD keeps silence from reaching the engine. recall's noise
filter does not drop it, so a TEA integration that ever bypasses VAD
would need `嗯` on silence added there.

## Go/no-go against the spec

| Criterion | Result | Met |
|---|---|---|
| MER at least 5 points below Breeze-ASR-25 q8_0 | 3.7 points pooled (ASCEND 6.5, CSZS 2.1) | No |
| RTF at or below 1.0 on CPU | 0.55 with the aligner, worst session 0.89 | Yes |
| Peak memory at or below 4 GB | 4.2 to 4.6 GiB (4.5 to 5.0 GB) with both models loaded; 2.8 GiB when the aligner runs after the ASR model is freed (one session) | No as run; yes with sequential loading |
| No repetition loops or empty output on silence or on segments over 30 s | No loops, no empty output on long speech; `嗯` per window on digital silence without VAD | Yes |

The spec does not say whether the 5 points apply pooled or per dataset.
The criterion fails pooled and on CSZS and passes only on ASCEND, so it
fails under either strict reading.

The verdict is close. The pooled 95% interval (2.0 to 5.4 points) contains
5, so a larger sample could land on either side. ASCEND's 6.5 points come
from 6 sessions and have no interval of their own, and the `zh` hint was
picked on one of those sessions. ASCEND alone is therefore weak evidence
that TEA clears the bar on conversation.

Revisit if a test on recall's own audio shows a larger gap: the spontaneous
conversation set (ASCEND) cleared 5 points while the synthesised one (CSZS)
did not, and real meetings are closer to ASCEND. The bench's `zh` and
`en-tw` cases are the place to check once Leon's recordings exist. An
integration would need sequential model loading to stay under 4 GB and one
of the ggml fixes below.

## ggml clash

`crispasr` links the CrispASR fork of ggml (0.26) as shared libraries.
whisper-rs-sys 0.15 links its own ggml (0.9.x) and whisper statically into
the executable. A scratch binary that depends on both builds and links
without errors, but because `libcrispasr` references `ggml_*` and
`whisper_*`, the linker exports the executable's static copies (302 `ggml_`
and 122 `whisper_` symbols in its dynamic symbol table), and at run time
they take precedence over the shared ones. The binary transcribed with
Breeze, then segfaulted while opening TEA:

```
#0 core_gguf::mmap_buffer_get_base(ggml_backend_buffer*)   libcrispasr.so.1
#1 ggml_backend_buffer_get_base ()                         executable (whisper-rs ggml)
#2 ggml_backend_tensor_alloc ()                            executable (whisper-rs ggml)
#3 core_gguf::load_weights_impl(...)                       libcrispasr.so.1
#5 qwen3_asr_init_from_file ()                             libcrispasr.so.1
```

Both engines cannot share one process as linked today. The options are
removing whisper-rs (CrispASR also runs Whisper GGML models, untested here),
running TEA in a separate process, or building one ggml for both, which
whisper-rs-sys does not support out of the box.
