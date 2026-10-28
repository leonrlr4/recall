//! Naming remote clusters from Google Meet speaking events: who Meet says
//! was talking, and when, voted against the diarization clusters.

use std::collections::HashMap;

use recall_store::{Cluster, MeetEvent, MeetEventKind, Segment, SpeakerId};

use crate::naming::CLUSTER_REF;

/// A span in which exactly one named Meet participant was speaking, in
/// milliseconds from the recording origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interval {
    pub start_ms: i64,
    pub end_ms: i64,
    pub name: String,
}

/// Least overlapped speech, in milliseconds, a name needs to win a cluster.
const MIN_VOTE_MS: i64 = 30_000;
/// Share of a cluster's overlapped speech, in percent, a name needs to win it.
const MIN_VOTE_PERCENT: i64 = 60;

/// The spans in which exactly one named participant was speaking.
///
/// Events are taken in time order. Each participant's `speaking_start` opens
/// an interval that its `speaking_stop`, its next `speaking_start`, or the
/// last event of all closes; a stop with nothing open is ignored. A
/// participant takes the name from its latest `name` event, so several
/// stream ids with one name (someone who rejoined) count as one person, and a
/// participant never named is dropped. Every span in which two or more names
/// overlap is then removed. The result is in time order, with back-to-back
/// spans of one name joined.
pub fn speaking_intervals(events: &[MeetEvent]) -> Vec<Interval> {
    let mut events: Vec<&MeetEvent> = events.iter().collect();
    events.sort_by_key(|e| e.t);
    let Some(last_t) = events.last().map(|e| e.t) else {
        return Vec::new();
    };

    let mut names: HashMap<&str, &str> = HashMap::new();
    let mut open: HashMap<&str, i64> = HashMap::new();
    let mut spans: Vec<(i64, i64, &str)> = Vec::new();
    for event in &events {
        match &event.kind {
            MeetEventKind::SpeakingStart { participant } => {
                if let Some(from) = open.insert(participant, event.t) {
                    spans.push((from, event.t, participant));
                }
            }
            MeetEventKind::SpeakingStop { participant } => {
                if let Some(from) = open.remove(participant.as_str()) {
                    spans.push((from, event.t, participant));
                }
            }
            MeetEventKind::Name { participant, name } => {
                names.insert(participant, name);
            }
            _ => {}
        }
    }
    spans.extend(open.into_iter().map(|(p, from)| (from, last_t, p)));

    let named: Vec<(i64, i64, &str)> = spans
        .into_iter()
        .filter(|(from, to, _)| to > from)
        .filter_map(|(from, to, p)| names.get(p).map(|name| (from, to, *name)))
        .collect();

    // Sweep the span boundaries; keep each piece where one name is speaking.
    let mut bounds: Vec<i64> = named.iter().flat_map(|(f, t, _)| [*f, *t]).collect();
    bounds.sort_unstable();
    bounds.dedup();
    let mut out: Vec<Interval> = Vec::new();
    for piece in bounds.windows(2) {
        let (from, to) = (piece[0], piece[1]);
        let mut speaking = named
            .iter()
            .filter(|(f, t, _)| *f <= from && to <= *t)
            .map(|(_, _, name)| *name);
        let Some(name) = speaking.next() else {
            continue;
        };
        if speaking.any(|other| other != name) {
            continue;
        }
        match out.last_mut() {
            Some(prev) if prev.end_ms == from && prev.name == name => prev.end_ms = to,
            _ => out.push(Interval {
                start_ms: from,
                end_ms: to,
                name: name.to_string(),
            }),
        }
    }
    out
}

/// The name Meet gives each cluster, if any, one per cluster.
///
/// A segment belongs to cluster `i` when its speaker is `cluster:{i}` or is
/// `clusters[i].speaker` (a rerun, after the ids were resolved). For each
/// name, the overlap of its `intervals` with the cluster's segments is
/// summed; the name with the most wins when it has at least
/// [`MIN_VOTE_MS`] and at least [`MIN_VOTE_PERCENT`] of the cluster's
/// total overlap.
pub fn vote(
    clusters: &[Cluster],
    segments: &[Segment],
    intervals: &[Interval],
) -> Vec<Option<String>> {
    clusters
        .iter()
        .enumerate()
        .map(|(i, cluster)| {
            let tag = SpeakerId(format!("{CLUSTER_REF}{i}"));
            let mut by_name: Vec<(&str, i64)> = Vec::new();
            for segment in segments.iter().filter(|s| {
                s.speaker
                    .as_ref()
                    .is_some_and(|id| *id == tag || Some(id) == cluster.speaker.as_ref())
            }) {
                let (start, end) = (i64::from(segment.start_ms), i64::from(segment.end_ms));
                for interval in intervals {
                    let ms = end.min(interval.end_ms) - start.max(interval.start_ms);
                    if ms <= 0 {
                        continue;
                    }
                    match by_name.iter_mut().find(|(n, _)| *n == interval.name) {
                        Some((_, total)) => *total += ms,
                        None => by_name.push((&interval.name, ms)),
                    }
                }
            }
            let total: i64 = by_name.iter().map(|(_, ms)| ms).sum();
            by_name
                .into_iter()
                .max_by_key(|(_, ms)| *ms)
                .filter(|(_, ms)| *ms >= MIN_VOTE_MS && *ms * 100 >= total * MIN_VOTE_PERCENT)
                .map(|(name, _)| name.to_string())
        })
        .collect()
}

/// Every participant name the events mention, from `roster` and `name`
/// events, in the order first seen and without repeats.
pub fn roster(events: &[MeetEvent]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for event in events {
        let names: &[String] = match &event.kind {
            MeetEventKind::Roster { names } => names,
            MeetEventKind::Name { name, .. } => std::slice::from_ref(name),
            _ => &[],
        };
        for name in names {
            if !name.is_empty() && !out.contains(name) {
                out.push(name.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use recall_store::Side;

    use super::*;

    fn start(t: i64, participant: &str) -> MeetEvent {
        MeetEvent {
            t,
            kind: MeetEventKind::SpeakingStart {
                participant: participant.into(),
            },
        }
    }

    fn stop(t: i64, participant: &str) -> MeetEvent {
        MeetEvent {
            t,
            kind: MeetEventKind::SpeakingStop {
                participant: participant.into(),
            },
        }
    }

    fn name(t: i64, participant: &str, name: &str) -> MeetEvent {
        MeetEvent {
            t,
            kind: MeetEventKind::Name {
                participant: participant.into(),
                name: name.into(),
            },
        }
    }

    fn interval(start_ms: i64, end_ms: i64, name: &str) -> Interval {
        Interval {
            start_ms,
            end_ms,
            name: name.into(),
        }
    }

    fn cluster(label: &str) -> Cluster {
        Cluster {
            label: label.into(),
            embedding: vec![1.0],
            speaker: None,
            named_by: None,
        }
    }

    fn seg(start_ms: u32, end_ms: u32, speaker: &str) -> Segment {
        Segment {
            start_ms,
            end_ms,
            side: Side::Them,
            speaker: Some(SpeakerId(speaker.into())),
            text: String::new(),
        }
    }

    #[test]
    fn overlapping_speech_is_dropped() {
        let events = [
            name(0, "csrc:1", "Amy"),
            name(0, "csrc:2", "Bob"),
            start(0, "csrc:1"),
            start(5_000, "csrc:2"),
            stop(10_000, "csrc:1"),
            stop(15_000, "csrc:2"),
        ];
        assert_eq!(
            speaking_intervals(&events),
            [interval(0, 5_000, "Amy"), interval(10_000, 15_000, "Bob")]
        );
    }

    #[test]
    fn intervals_resolve_latest_name_and_close_open_ones() {
        let events = [
            // Out of time order, as a resent batch can leave the file.
            start(2_000, "csrc:1"),
            name(1_000, "csrc:1", "Presenting"),
            stop(4_000, "csrc:1"),
            // A second start closes the open interval.
            start(6_000, "csrc:1"),
            start(8_000, "csrc:1"),
            stop(12_000, "csrc:1"),
            // Never named: dropped.
            start(20_000, "csrc:3"),
            stop(21_000, "csrc:3"),
            // A stop with no start is ignored.
            stop(22_000, "csrc:1"),
            name(9_000, "csrc:1", "Tom"),
            // Still open at the last event (30 s).
            start(25_000, "csrc:1"),
            MeetEvent {
                t: 30_000,
                kind: MeetEventKind::Status {
                    status: "ok".into(),
                },
            },
        ];
        assert_eq!(
            speaking_intervals(&events),
            [
                interval(2_000, 4_000, "Tom"),
                interval(6_000, 12_000, "Tom"),
                interval(25_000, 30_000, "Tom"),
            ]
        );
    }

    #[test]
    fn rejoin_with_new_csrc_maps_by_name() {
        // Tom rejoins with a new stream id; his two streams briefly overlap,
        // which is still one person speaking.
        let events = [
            name(0, "csrc:1", "Tom"),
            start(0, "csrc:1"),
            stop(20_000, "csrc:1"),
            name(19_000, "csrc:9", "Tom"),
            start(19_000, "csrc:9"),
            stop(40_000, "csrc:9"),
        ];
        let intervals = speaking_intervals(&events);
        assert_eq!(intervals, [interval(0, 40_000, "Tom")]);

        let clusters = [cluster("Remote 1")];
        let segments = [seg(0, 40_000, "cluster:0")];
        assert_eq!(vote(&clusters, &segments, &intervals), [Some("Tom".into())]);
    }

    #[test]
    fn vote_needs_30s_and_60_percent() {
        let clusters = [cluster("Remote 1")];
        let segments = [seg(0, 100_000, "cluster:0")];

        let alone = [interval(0, 29_000, "Tom")];
        assert_eq!(vote(&clusters, &segments, &alone), [None]);

        let split = [interval(0, 40_000, "Tom"), interval(50_000, 80_000, "Amy")];
        assert_eq!(vote(&clusters, &segments, &split), [None]);

        let clear = [interval(0, 40_000, "Tom"), interval(50_000, 60_000, "Amy")];
        assert_eq!(vote(&clusters, &segments, &clear), [Some("Tom".into())]);
    }

    #[test]
    fn vote_counts_only_each_clusters_own_segments() {
        let mut clusters = [cluster("Remote 1"), cluster("Remote 2")];
        // A rerun: cluster 1's segments already carry its speaker id.
        clusters[1].speaker = Some(SpeakerId("p-amy".into()));
        let segments = [
            seg(0, 35_000, "cluster:0"),
            seg(40_000, 80_000, "p-amy"),
            seg(80_000, 90_000, "me"),
        ];
        let intervals = [interval(0, 35_000, "Tom"), interval(40_000, 90_000, "Amy")];
        assert_eq!(
            vote(&clusters, &segments, &intervals),
            [Some("Tom".into()), Some("Amy".into())]
        );
    }

    #[test]
    fn roster_unions_roster_and_name_events_in_first_seen_order() {
        let events = [
            name(0, "csrc:1", "Tom"),
            MeetEvent {
                t: 1,
                kind: MeetEventKind::Roster {
                    names: vec!["Amy".into(), "Tom".into(), "Bob".into()],
                },
            },
            name(2, "csrc:2", "Cat"),
            name(3, "csrc:3", "Amy"),
        ];
        assert_eq!(roster(&events), ["Tom", "Amy", "Bob", "Cat"]);
        assert!(roster(&[]).is_empty());
    }
}
