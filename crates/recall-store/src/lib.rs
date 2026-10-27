//! Meeting records, their on-disk layout, the speaker registry, the config
//! and the SQLite index.

pub mod config;
pub mod dir;
pub mod events;
pub mod index;
pub mod markdown;
pub mod meeting;
pub mod speakers;

pub use config::{Config, LlmConfig, SttConfig};
pub use dir::{MeetingDir, sanitize_title};
pub use events::Event;
pub use index::{Hit, Index, Job, ListFilter, MeetingRow, RebuildReport, state_of};
pub use meeting::{
    ActionItem, Chapter, Cluster, Meeting, MeetingId, Segment, Side, SpeakerId, Step, StepState,
    Summary,
};
pub use speakers::{Person, Speakers, cosine};
