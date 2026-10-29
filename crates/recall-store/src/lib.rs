//! Meeting records, their on-disk layout, the speaker registry, the config
//! and the SQLite index.

pub mod config;
pub mod dir;
pub mod events;
pub mod index;
pub mod markdown;
pub mod meet;
pub mod meeting;
pub mod speakers;

pub use config::{Config, LlmConfig, SttConfig};
pub use dir::{CaptureClock, MeetingDir, sanitize_title};
pub use events::Event;
pub use index::{Hit, Index, Job, ListFilter, MeetingRow, RebuildReport, state_of};
pub use meet::{MeetBatch, MeetEvent, MeetEventKind, append_events, read_events};
pub use meeting::{
    ActionItem, Chapter, Cluster, Meeting, MeetingId, NamedBy, Priority, Segment, Side, SpeakerId,
    Step, StepState, Summary,
};
pub use speakers::{Person, Speakers, cosine};
