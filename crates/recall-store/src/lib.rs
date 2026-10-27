//! Meeting records and their on-disk layout.

pub mod dir;
pub mod events;
pub mod meeting;

pub use dir::{MeetingDir, sanitize_title};
pub use events::Event;
pub use meeting::{
    ActionItem, Chapter, Cluster, Meeting, MeetingId, Segment, Side, SpeakerId, Step, StepState,
    Summary,
};
