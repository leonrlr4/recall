//! Speech to text with Breeze ASR 25 through whisper.cpp, written out in
//! Traditional Chinese.

use std::ffi::c_int;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use opencc::OpenCC;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperSegment,
    WhisperState, WhisperTokenId,
};

use crate::decode::SAMPLE_RATE;
use crate::vad::Region;

/// A word (for Chinese, usually one character) with its time on the
/// recording's timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    pub start_ms: u32,
    pub end_ms: u32,
    pub text: String,
}

/// One whisper segment, in Traditional Chinese, on the recording's timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Utterance {
    pub start_ms: u32,
    pub end_ms: u32,
    pub text: String,
    pub words: Vec<Word>,
}

/// Turns speech regions of a 16 kHz mono track into utterances.
pub trait Transcriber: Send {
    /// Transcribes each of `regions` (in time order) in `samples16k`. `lang`
    /// is a whisper language code such as `zh`.
    fn transcribe(
        &mut self,
        samples16k: &[f32],
        regions: &[Region],
        lang: &str,
    ) -> Result<Vec<Utterance>>;

    /// Name recorded in the meeting's metadata.
    fn model_name(&self) -> String;
}

/// Audio added before and after each region so whisper hears whole words.
const PAD_MS: u32 = 200;
/// Longest window of glued regions sent to whisper in one call. whisper runs
/// its encoder over 30 s whatever the input length (about 10 s of CPU for
/// Breeze), so packing regions together instead of one call per region is
/// what keeps a meeting's transcription time near its speech time.
const WINDOW_MS: u32 = 28_000;
/// Silence inserted between glued regions so whisper ends a sentence there.
const GAP_MS: u32 = 300;
/// whisper.cpp rejects or mangles input shorter than one second, so shorter
/// windows are extended with trailing silence.
const MIN_WINDOW_SAMPLES: usize = SAMPLE_RATE as usize;
const SAMPLES_PER_MS: usize = SAMPLE_RATE as usize / 1_000;

/// Breeze ASR 25 (or any ggml whisper model) through whisper.cpp.
pub struct WhisperTranscriber {
    ctx: WhisperContext,
    state: WhisperState,
    /// Everything but the language, built once.
    params: FullParams<'static, 'static>,
    name: String,
}

impl WhisperTranscriber {
    /// Loads a ggml model. With the `vulkan` feature the model runs on the
    /// GPU; otherwise on `threads` CPU threads.
    pub fn load(model: &Path, threads: usize) -> Result<Self> {
        if !model.is_file() {
            bail!("whisper model not found at {}", model.display());
        }
        // Sends whisper.cpp and ggml logging to the (absent) `log` backend
        // instead of stderr. Only the first call has an effect.
        whisper_rs::install_logging_hooks();

        let mut ctx_params = WhisperContextParameters::default();
        ctx_params.use_gpu(cfg!(feature = "vulkan"));
        let ctx = WhisperContext::new_with_params(model, ctx_params)
            .with_context(|| format!("loading whisper model {}", model.display()))?;
        let state = ctx.create_state().context("creating whisper state")?;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(c_int::try_from(threads.max(1)).context("thread count")?);
        params.set_token_timestamps(true);
        params.set_no_context(true);
        params.set_single_segment(false);
        params.set_suppress_blank(true);
        // No initial prompt, and do not add one: whisper.cpp tokenizes the
        // prompt with std::regex, and the prebuilt static sherpa-onnx
        // libraries (libsherpa-onnx-core.a, libonnxruntime.a) carry their own
        // std::regex template instantiations from an older GCC. The linker
        // keeps theirs, so `whisper_tokenize` aborts with
        // "free(): invalid pointer". Traditional output comes from
        // `to_traditional` instead.
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        let name = model
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Self {
            ctx,
            state,
            params,
            name,
        })
    }
}

impl Transcriber for WhisperTranscriber {
    fn transcribe(
        &mut self,
        samples16k: &[f32],
        regions: &[Region],
        lang: &str,
    ) -> Result<Vec<Utterance>> {
        // whisper-rs leaks the C copy of the language on every
        // `set_language`, so it is set once per call, not per window.
        if lang.contains('\0') {
            bail!("language code {lang:?} contains a NUL byte");
        }
        let mut params = self.params.clone();
        params.set_language(Some(lang));
        let eot = self.ctx.token_eot();
        let mut out = Vec::new();
        let mut audio = Vec::new();
        for window in glue_regions(samples16k.len(), regions) {
            audio.clear();
            for (i, piece) in window.iter().enumerate() {
                if i > 0 {
                    audio.resize(audio.len() + GAP_MS as usize * SAMPLES_PER_MS, 0.0);
                }
                let start = piece.original_start_ms as usize * SAMPLES_PER_MS;
                let end = start + piece.len_ms as usize * SAMPLES_PER_MS;
                audio.extend_from_slice(&samples16k[start..end]);
            }
            if audio.len() < MIN_WINDOW_SAMPLES {
                audio.resize(MIN_WINDOW_SAMPLES, 0.0);
            }
            self.state.full(params.clone(), &audio).with_context(|| {
                format!(
                    "transcribing {} ms from {} ms",
                    audio.len() / SAMPLES_PER_MS,
                    window[0].original_start_ms
                )
            })?;
            for segment in self.state.as_iter() {
                let raw = segment.to_str_lossy().context("reading whisper segment")?;
                let text = to_traditional(raw.trim());
                if is_noise(&text) {
                    continue;
                }
                let start_ms = map_back(&window, centis_to_ms(segment.start_timestamp()));
                let end_ms = map_back_end(&window, centis_to_ms(segment.end_timestamp()));
                out.push(Utterance {
                    start_ms,
                    end_ms: end_ms.max(start_ms),
                    text,
                    words: words(&segment, eot, &window),
                });
            }
        }
        Ok(out)
    }

    fn model_name(&self) -> String {
        self.name.clone()
    }
}

/// A stretch of the recording placed in a glued window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Piece {
    /// Where the piece starts in the window.
    glued_start_ms: u32,
    /// Where the same audio starts in the recording.
    original_start_ms: u32,
    len_ms: u32,
}

/// Packs `regions` (in time order) of a buffer of `len` samples into
/// windows for whisper. Each region is widened by [`PAD_MS`] on both sides,
/// clamped to the buffer and trimmed so it never repeats audio already taken
/// by the previous region. Consecutive pieces share a window, with
/// [`GAP_MS`] of silence between them, while the window stays within
/// [`WINDOW_MS`]; a piece longer than that is a window of its own.
fn glue_regions(len: usize, regions: &[Region]) -> Vec<Vec<Piece>> {
    debug_assert!(
        regions.windows(2).all(|p| p[0].start_ms <= p[1].start_ms),
        "regions must be sorted by start"
    );
    let len_ms = u32::try_from(len / SAMPLES_PER_MS).unwrap_or(u32::MAX);
    let mut windows: Vec<Vec<Piece>> = Vec::new();
    let mut window_ms = 0;
    let mut taken_until = 0;
    for region in regions {
        let start = region
            .start_ms
            .saturating_sub(PAD_MS)
            .max(taken_until)
            .min(len_ms);
        let end = region.end_ms.saturating_add(PAD_MS).min(len_ms);
        if start >= end {
            continue;
        }
        taken_until = end;
        let piece_ms = end - start;
        match windows.last_mut() {
            Some(window) if window_ms + GAP_MS + piece_ms <= WINDOW_MS => {
                let glued_start_ms = window_ms + GAP_MS;
                window.push(Piece {
                    glued_start_ms,
                    original_start_ms: start,
                    len_ms: piece_ms,
                });
                window_ms = glued_start_ms + piece_ms;
            }
            _ => {
                windows.push(vec![Piece {
                    glued_start_ms: 0,
                    original_start_ms: start,
                    len_ms: piece_ms,
                }]);
                window_ms = piece_ms;
            }
        }
    }
    windows
}

/// A whisper time (centiseconds) in milliseconds.
fn centis_to_ms(centis: i64) -> u32 {
    u32::try_from(centis.max(0).saturating_mul(10)).unwrap_or(u32::MAX)
}

/// Maps a start time `glued_ms` in a window back to the recording's
/// timeline. A time in the silence between two pieces snaps forward to the
/// start of the following piece; a time past the last piece (the 1 s minimum
/// padding) snaps to its end.
fn map_back(window: &[Piece], glued_ms: u32) -> u32 {
    for piece in window {
        if glued_ms < piece.glued_start_ms {
            return piece.original_start_ms;
        }
        if glued_ms < piece.glued_start_ms + piece.len_ms {
            return piece.original_start_ms + (glued_ms - piece.glued_start_ms);
        }
    }
    window.last().map_or(0, |p| p.original_start_ms + p.len_ms)
}

/// Maps an end time `glued_ms` in a window back to the recording's timeline.
/// Whisper puts closing timestamps in the trailing silence, which in a window
/// is a glue gap, and the next piece can be minutes later in the recording;
/// so a time in a gap, or past the last piece, snaps back to the end of the
/// preceding piece.
fn map_back_end(window: &[Piece], glued_ms: u32) -> u32 {
    let mut previous_end = window.first().map_or(0, |p| p.original_start_ms);
    for piece in window {
        if glued_ms < piece.glued_start_ms {
            return previous_end;
        }
        if glued_ms <= piece.glued_start_ms + piece.len_ms {
            return piece.original_start_ms + (glued_ms - piece.glued_start_ms);
        }
        previous_end = piece.original_start_ms + piece.len_ms;
    }
    previous_end
}

/// Groups a segment's text tokens into words. A word starts at a token that
/// begins with whitespace, and at every boundary next to a non-ASCII
/// character, so Chinese splits per character (byte-level tokens that hold
/// half a character are joined first) and English per space-led word.
/// Special and timestamp tokens (ids from end-of-text up) are skipped, as are
/// words with no letters or digits. Times are mapped through `window` to the
/// recording's timeline.
fn words(segment: &WhisperSegment<'_>, eot: WhisperTokenId, window: &[Piece]) -> Vec<Word> {
    let mut out = Vec::new();
    let mut bytes: Vec<u8> = Vec::new();
    let (mut t0, mut t1) = (0i64, 0i64);
    let flush = |bytes: &mut Vec<u8>, t0: i64, t1: i64, out: &mut Vec<Word>| {
        let text = to_traditional(String::from_utf8_lossy(bytes).trim());
        bytes.clear();
        if text.chars().any(char::is_alphanumeric) {
            let start_ms = map_back(window, centis_to_ms(t0));
            out.push(Word {
                start_ms,
                end_ms: map_back_end(window, centis_to_ms(t1)).max(start_ms),
                text,
            });
        }
    };
    for i in 0..segment.n_tokens() {
        let Some(token) = segment.get_token(i) else {
            continue;
        };
        if token.token_id() >= eot {
            continue;
        }
        let Ok(piece) = token.to_bytes() else {
            continue;
        };
        if piece.is_empty() {
            continue;
        }
        let data = token.token_data();
        let boundary = match std::str::from_utf8(&bytes) {
            // The previous token left half a character: keep joining.
            Err(_) => false,
            Ok(prev) => {
                piece[0].is_ascii_whitespace()
                    || piece[0] >= 0xC0
                    || prev.chars().last().is_some_and(|c| !c.is_ascii())
            }
        };
        if boundary && !bytes.is_empty() {
            flush(&mut bytes, t0, t1, &mut out);
        }
        if bytes.is_empty() {
            t0 = data.t0;
        }
        bytes.extend_from_slice(piece);
        t1 = data.t1;
    }
    if !bytes.is_empty() {
        flush(&mut bytes, t0, t1, &mut out);
    }
    out
}

/// Phrases Whisper-family models invent over silence or music, learnt from
/// subtitle credits and video outros in their training data. Matched only
/// in short text. Each is a credit-line shape, not a topic word, so speech
/// about subtitles ("這個字幕檔是誰製作的") is kept.
const HALLUCINATIONS: &[&str] = &[
    "字幕由",
    "字幕組",
    "字幕志願者",
    "Amara.org",
    "不吝點贊",
    "點點欄目",
];
/// Longer text is real speech even if it mentions one of the phrases above.
const HALLUCINATION_MAX_CHARS: usize = 40;

/// Whether `text` should be dropped: empty, only punctuation, a lone
/// bracketed sound tag such as `[BLANK_AUDIO]` or `(音樂)`, or a known
/// silence hallucination such as `字幕由 Amara.org 社群提供`.
fn is_noise(text: &str) -> bool {
    let t = text.trim();
    if !t.chars().any(char::is_alphanumeric) {
        return true;
    }
    let tag = [('[', ']'), ('(', ')'), ('（', '）'), ('【', '】')]
        .iter()
        .any(|&(open, close)| {
            t.strip_prefix(open)
                .and_then(|rest| rest.strip_suffix(close))
                .is_some_and(|inner| !inner.contains([open, close]))
        });
    if tag {
        return true;
    }
    if t.chars().count() > HALLUCINATION_MAX_CHARS {
        return false;
    }
    HALLUCINATIONS.iter().any(|h| t.contains(h)) || has_subtitle_by(t)
}

/// "字幕 by <name>" or "字幕by<name>", with "by" as a whole word.
fn has_subtitle_by(t: &str) -> bool {
    t.match_indices("字幕").any(|(i, m)| {
        let rest = t[i + m.len()..].trim_start();
        rest.get(..2)
            .is_some_and(|by| by.eq_ignore_ascii_case("by"))
            && !rest[2..].starts_with(|c: char| c.is_ascii_alphanumeric())
    })
}

/// OpenCC configuration: Simplified to Traditional, Taiwan standard
/// characters, without rewriting vocabulary (s2twp would turn 软件 into 軟體).
const OPENCC_CONFIG: &str = "s2tw.json";

/// Converts Simplified Chinese to Traditional Chinese (Taiwan standard).
/// Text that is already Traditional, and non-Chinese text, is unchanged.
///
/// If libopencc cannot load its `s2tw.json` data file, the text is returned
/// as is and a warning is printed once.
pub fn to_traditional(text: &str) -> String {
    // The Mutex does not rely on the opencc crate's own `Sync` claim for the
    // C handle.
    static CONVERTER: OnceLock<Option<Mutex<OpenCC>>> = OnceLock::new();
    let converter = CONVERTER.get_or_init(|| {
        let mut cc = OpenCC::new(OPENCC_CONFIG);
        // `opencc_open` reports failure through the global last error, and
        // the crate keeps the invalid handle, which must never be used.
        match cc.last_error().filter(|e| !e.is_empty()) {
            None => Some(Mutex::new(cc)),
            Some(err) => {
                std::mem::forget(cc);
                eprintln!("recall: OpenCC {OPENCC_CONFIG} unavailable, text stays as is: {err}");
                None
            }
        }
    });
    let text = text.replace('\0', "");
    match converter {
        Some(cc) if !text.is_empty() => match cc.lock() {
            Ok(cc) => cc.convert(&text),
            Err(poisoned) => poisoned.into_inner().convert(&text),
        },
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;

    #[test]
    fn to_traditional_converts_simplified() {
        assert_eq!(to_traditional("测试"), "測試");
        assert_eq!(to_traditional("我们来测试一下 API"), "我們來測試一下 API");
        // Already Traditional text and plain English pass through unchanged.
        assert_eq!(to_traditional("會議記錄"), "會議記錄");
        assert_eq!(to_traditional("export the CSV"), "export the CSV");
        assert_eq!(to_traditional(""), "");
        // An interior NUL must not panic the FFI wrapper.
        assert_eq!(to_traditional("测\0试"), "測試");
    }

    fn p(glued_start_ms: u32, original_start_ms: u32, len_ms: u32) -> Piece {
        Piece {
            glued_start_ms,
            original_start_ms,
            len_ms,
        }
    }

    #[test]
    fn glue_regions_packs_up_to_28s() {
        let r = |start_ms, end_ms| Region { start_ms, end_ms };
        // 100 s of audio.
        let len = 100_000 * SAMPLES_PER_MS;
        let regions = [
            // Padded to 800..11200 and 11800..22200: 10.4 + 0.3 + 10.4 s.
            r(1_000, 11_000),
            r(12_000, 22_000),
            // Padded to 7.4 s, which would take the window to 28.8 s.
            r(23_000, 30_000),
            // A 30 s region alone, padded to 30.4 s.
            r(40_000, 70_000),
            // Its padding would repeat 69.9..70.2 s, so it starts at 70.2 s.
            r(70_100, 71_000),
            // Past the end of the buffer: dropped. Clamped at the end.
            r(99_500, 99_900),
            r(101_000, 102_000),
        ];
        assert_eq!(
            glue_regions(len, &regions),
            vec![
                vec![p(0, 800, 10_400), p(10_700, 11_800, 10_400)],
                vec![p(0, 22_800, 7_400)],
                vec![p(0, 39_800, 30_400)],
                vec![p(0, 70_200, 1_000), p(1_300, 99_300, 700)],
            ]
        );
        assert!(glue_regions(len, &[]).is_empty());
    }

    #[test]
    fn glue_regions_window_boundary_is_28000ms() {
        let r = |start_ms, end_ms| Region { start_ms, end_ms };
        let len = 100_000 * SAMPLES_PER_MS;
        // Padded: 0..10200 (10.2 s) and 20000..37500 (17.5 s); with the
        // 0.3 s gap exactly 28 000 ms.
        assert_eq!(
            glue_regions(len, &[r(200, 10_000), r(20_200, 37_300)]),
            vec![vec![p(0, 0, 10_200), p(10_500, 20_000, 17_500)]]
        );
        // One more millisecond splits.
        assert_eq!(
            glue_regions(len, &[r(200, 10_000), r(20_200, 37_301)]),
            vec![vec![p(0, 0, 10_200)], vec![p(0, 20_000, 17_501)]]
        );
    }

    #[test]
    fn map_back_snaps_silence_to_next_region() {
        let window = [p(0, 800, 10_400), p(10_700, 11_800, 10_400)];
        assert_eq!(map_back(&window, 0), 800);
        assert_eq!(map_back(&window, 500), 1_300);
        // Inside the 300 ms gap: the next region's start.
        assert_eq!(map_back(&window, 10_400), 11_800);
        assert_eq!(map_back(&window, 10_650), 11_800);
        assert_eq!(map_back(&window, 10_700), 11_800);
        assert_eq!(map_back(&window, 11_000), 12_100);
        // Past the last piece (minimum-length padding): its end.
        assert_eq!(map_back(&window, 30_000), 22_200);

        // Ends snap back instead: the gap is trailing silence of the piece
        // before it, and the next piece may be minutes later.
        assert_eq!(map_back_end(&window, 500), 1_300);
        assert_eq!(map_back_end(&window, 10_400), 11_200);
        assert_eq!(map_back_end(&window, 10_500), 11_200);
        assert_eq!(map_back_end(&window, 10_699), 11_200);
        assert_eq!(map_back_end(&window, 10_700), 11_800);
        assert_eq!(map_back_end(&window, 11_000), 12_100);
        assert_eq!(map_back_end(&window, 21_100), 22_200);
        assert_eq!(map_back_end(&window, 30_000), 22_200);
    }

    #[test]
    fn noise_filter_drops_silence_hallucinations_only() {
        for noise in [
            "",
            "  ",
            "。",
            "……",
            "[BLANK_AUDIO]",
            "(音樂)",
            "（掌聲）",
            "[Music]",
            "字幕由 Amara.org 社群提供",
            "字幕by索兰娅",
            "中文字幕志願者 楊茜茜",
            "請不吝點贊 訂閱 轉發 打賞支持明鏡與點點欄目",
        ] {
            assert!(is_noise(noise), "{noise:?} should be dropped");
        }
        for speech in [
            "好",
            "OK",
            "我們把字幕檔 export 出來",
            // Near misses for each credit pattern.
            "這個字幕檔是誰製作的",
            "字幕的由來是什麼",
            "字幕要組合起來",
            "我們找志願者做字幕",
            "我們用 Amara 做字幕",
            "字幕 bypass 掉",
            "stand by 一下",
            "(我覺得) 可以",
            "3.5",
        ] {
            assert!(!is_noise(speech), "{speech:?} should be kept");
        }
    }

    /// `models/ggml-breeze-asr-25-q8_0.bin` under `$RECALL_HOME` or
    /// `~/.local/share/recall`.
    fn installed_breeze() -> Option<PathBuf> {
        let candidates = [
            std::env::var_os("RECALL_HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join("models")),
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/recall/models")),
        ];
        candidates
            .into_iter()
            .flatten()
            .map(|dir| crate::models::path(&dir, crate::ModelId::BreezeGgml))
            .find(|model| model.is_file())
    }

    #[test]
    fn whisper_transcribes_fixture() {
        let Some(model) = installed_breeze() else {
            eprintln!(
                "skipping whisper_transcribes_fixture: ggml-breeze-asr-25-q8_0.bin not found \
                 under $RECALL_HOME/models or ~/.local/share/recall/models"
            );
            return;
        };
        // 1 s silence, 6.6 s of LibriSpeech (CC BY 4.0), 1 s silence: "After
        // early nightfall the yellow lamps would light up here and there the
        // squalid quarter of the brothels."
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/speech.opus");
        let (samples, _) = crate::decode_stereo(&fixture).unwrap();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let mut whisper = WhisperTranscriber::load(&model, threads).unwrap();
        assert_eq!(whisper.model_name(), "ggml-breeze-asr-25-q8_0");

        // Two regions, glued into one window, so mapping back to the file
        // timeline is exercised; a silent region later must produce nothing.
        let regions = [
            Region {
                start_ms: 900,
                end_ms: 4_500,
            },
            Region {
                start_ms: 4_500,
                end_ms: 7_800,
            },
        ];
        let started = Instant::now();
        let utterances = whisper.transcribe(&samples, &regions, "zh").unwrap();
        eprintln!(
            "transcribed {:.1} s of speech in {:.2?} with {threads} threads: {utterances:#?}",
            6.9,
            started.elapsed()
        );

        assert!(!utterances.is_empty());
        let text = utterances
            .iter()
            .map(|u| u.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        assert!(
            [
                "nightfall",
                "yellow",
                "lamps",
                "squalid",
                "quarter",
                "brothels"
            ]
            .iter()
            .any(|w| text.contains(w)),
            "{text}"
        );
        for u in &utterances {
            // Padding is 200 ms, so nothing can fall outside 700..8000.
            assert!(u.start_ms >= 700 && u.end_ms <= 8_000, "{u:?}");
            assert!(u.start_ms <= u.end_ms, "{u:?}");
            assert!(!u.words.is_empty(), "{u:?}");
            for w in &u.words {
                assert!(w.start_ms >= 700 && w.end_ms <= 8_000, "{w:?}");
            }
        }
        assert!(
            utterances
                .windows(2)
                .all(|p| p[0].start_ms <= p[1].start_ms)
        );

        let silence = whisper
            .transcribe(
                &samples,
                &[Region {
                    start_ms: 0,
                    end_ms: 700,
                }],
                "zh",
            )
            .unwrap();
        assert!(silence.is_empty(), "{silence:?}");

        let nul = whisper.transcribe(&samples, &regions, "z\0h").unwrap_err();
        assert!(format!("{nul:#}").contains("NUL"), "{nul:#}");
    }

    /// The fixture twice, 200 ms apart, through VAD, region merging and
    /// whisper as the runner does it: the sentence must come out twice.
    ///
    /// Known limitation, so ignored: VAD finds two regions (about 1.4-7.2 s
    /// and 10.2-16.0 s), [`glue_regions`] packs both into one window, and
    /// Breeze emits the sentence once, with timestamps stretched over both.
    /// Stricter temperature fallback (entropy 2.0, logprob -0.5) and a
    /// 1 s gap between glued regions do not change that; one region per
    /// window does, at about twice the time (13.6 s against 6.7 s for this
    /// test in release, model load included). Run with `--ignored` once that trade-off is decided.
    #[test]
    #[ignore = "whisper drops a repeated sentence within one glued window"]
    fn whisper_keeps_repeated_speech() {
        let Some(model) = installed_breeze() else {
            eprintln!("skipping whisper_keeps_repeated_speech: Breeze model not found");
            return;
        };
        let models = model.parent().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let twice = tmp.path().join("twice.opus");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/speech.opus");
        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&fixture)
            .args([
                "-f",
                "lavfi",
                "-t",
                "0.2",
                "-i",
                "anullsrc=r=48000:cl=mono",
                "-i",
            ])
            .arg(&fixture)
            .args([
                "-filter_complex",
                "[0:a][1:a][2:a]concat=n=3:v=0:a=1",
                "-c:a",
                "libopus",
            ])
            .arg(&twice)
            .status()
            .unwrap();
        assert!(status.success());
        let (samples, _) = crate::decode_stereo(&twice).unwrap();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let vad = crate::SileroVad::load(models, threads).unwrap();
        use crate::Vad as _;
        let regions = crate::merge_close(&vad.regions(&samples).unwrap(), 300, 30_000);
        let mut whisper = WhisperTranscriber::load(&model, threads).unwrap();

        let utterances = whisper.transcribe(&samples, &regions, "zh").unwrap();

        eprintln!("regions {regions:?}\nutterances {utterances:#?}");
        let said = utterances
            .iter()
            .filter(|u| u.text.to_lowercase().contains("nightfall"))
            .count();
        assert_eq!(said, 2, "{utterances:#?}");
    }
}
