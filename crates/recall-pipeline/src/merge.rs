//! Joins the two transcribed tracks into one transcript.

use recall_store::{Segment, Side};

use crate::transcribe::Utterance;

/// Turns `me` (microphone) and `them` (system audio) utterances into
/// segments ordered by start time. Overlapping speech stays as two segments;
/// on equal start times `me` comes first. `speaker` is left for diarization.
pub fn merge_sides(me: Vec<Utterance>, them: Vec<Utterance>) -> Vec<Segment> {
    let mut out: Vec<Segment> = me
        .into_iter()
        .map(|u| (u, Side::Me))
        .chain(them.into_iter().map(|u| (u, Side::Them)))
        .map(|(u, side)| Segment {
            start_ms: u.start_ms,
            end_ms: u.end_ms,
            side,
            speaker: None,
            text: u.text,
        })
        .collect();
    // Stable, so each side keeps its own order on ties; Me sorts before Them.
    out.sort_by_key(|s| (s.start_ms, s.side == Side::Them));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(start_ms: u32, end_ms: u32, text: &str) -> Utterance {
        Utterance {
            start_ms,
            end_ms,
            text: text.to_owned(),
            words: Vec::new(),
        }
    }

    fn seg(start_ms: u32, end_ms: u32, side: Side, text: &str) -> Segment {
        Segment {
            start_ms,
            end_ms,
            side,
            speaker: None,
            text: text.to_owned(),
        }
    }

    #[test]
    fn merge_sides_interleaves_by_start() {
        let me = vec![u(0, 1_000, "a"), u(5_000, 6_000, "c"), u(9_000, 9_500, "e")];
        let them = vec![
            u(2_000, 3_000, "b"),
            u(7_000, 8_000, "d"),
            u(9_000, 9_200, "f"),
        ];
        assert_eq!(
            merge_sides(me, them),
            vec![
                seg(0, 1_000, Side::Me, "a"),
                seg(2_000, 3_000, Side::Them, "b"),
                seg(5_000, 6_000, Side::Me, "c"),
                seg(7_000, 8_000, Side::Them, "d"),
                // Equal starts: Me first.
                seg(9_000, 9_500, Side::Me, "e"),
                seg(9_000, 9_200, Side::Them, "f"),
            ]
        );
    }

    #[test]
    fn merge_sides_keeps_overlap_both() {
        let me = vec![u(1_000, 4_000, "我先講")];
        let them = vec![u(500, 2_000, "sorry, go ahead")];
        assert_eq!(
            merge_sides(me, them),
            vec![
                seg(500, 2_000, Side::Them, "sorry, go ahead"),
                seg(1_000, 4_000, Side::Me, "我先講"),
            ]
        );
    }
}
