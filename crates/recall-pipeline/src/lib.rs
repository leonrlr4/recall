//! Post-processing pipeline that turns a recording into a meeting record.
//!
//! sherpa-onnx is linked as shared libraries, not statically. whisper.cpp is
//! compiled here with the system GCC, while the static sherpa-onnx and
//! onnxruntime archives are prebuilt with an older one; both carry weak
//! instantiations of libstdc++'s `std::regex`, and the linker keeps whichever
//! it loads first. Mixing the two crashed onnxruntime's device discovery
//! (`free(): invalid pointer` in `std::__detail::_Compiler`) as soon as
//! linking the diarization code changed that order. As shared libraries,
//! onnxruntime and sherpa-onnx keep their own copies. `build.rs` adds the
//! rpaths that find them, and [`models::shared_library_dir`] tells an
//! installer where to copy them from.

pub mod decode;
pub mod diarize;
pub mod glossary;
pub mod meet_vote;
pub mod merge;
pub mod models;
pub mod naming;
pub mod runner;
pub mod summarize;
pub mod transcribe;
pub mod translate;
pub mod vad;

pub use decode::decode_stereo;
pub use diarize::{Diarizer, SherpaDiarizer, Turn};
pub use glossary::Glossary;
pub use meet_vote::{Interval, roster, speaking_intervals, vote};
pub use merge::merge_sides;
pub use models::{ModelId, ModelSpec};
pub use naming::{apply_names, assign_clusters, name_clusters, name_from_meet};
pub use summarize::{merge_prompt, summarize};
pub use transcribe::{Transcriber, Utterance, WhisperTranscriber, Word, to_traditional};
pub use vad::{Region, SileroVad, Vad, drop_echo, merge_close};
