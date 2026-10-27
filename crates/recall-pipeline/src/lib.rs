//! Post-processing pipeline that turns a recording into a meeting record.

pub mod decode;
pub mod models;
pub mod vad;

pub use decode::decode_stereo;
pub use models::{ModelId, ModelSpec};
pub use vad::{Region, SileroVad, Vad, drop_echo, merge_close};
