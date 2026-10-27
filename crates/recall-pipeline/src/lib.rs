//! Post-processing pipeline that turns a recording into a meeting record.

pub mod decode;
pub mod merge;
pub mod models;
pub mod transcribe;
pub mod vad;

pub use decode::decode_stereo;
pub use merge::merge_sides;
pub use models::{ModelId, ModelSpec};
pub use transcribe::{Transcriber, Utterance, WhisperTranscriber, Word, to_traditional};
pub use vad::{Region, SileroVad, Vad, drop_echo, merge_close};
