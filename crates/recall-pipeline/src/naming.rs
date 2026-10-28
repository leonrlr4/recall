//! Turning diarization clusters into speakers: which cluster said each
//! remote line, which known person each cluster is, and the final speaker
//! ids on the transcript.

use recall_store::speakers::ME;
use recall_store::{Cluster, Segment, Side, SpeakerId, Speakers, cosine};

use crate::diarize::Turn;

/// Furthest a remote segment's midpoint may be from a turn's midpoint for it
/// to take that turn's cluster when it overlaps no turn at all.
const NEAREST_TURN_MS: u32 = 2_000;

/// Prefix of the temporary speaker id [`assign_clusters`] gives remote
/// segments; [`apply_names`] replaces it.
const CLUSTER_REF: &str = "cluster:";

/// Gives each segment a speaker: `me` for the microphone, and for the remote
/// side a temporary `cluster:{i}` id pointing into the returned clusters.
///
/// A remote segment takes the cluster whose `turns` overlap it longest, ties
/// going to the cluster that spoke first. One that overlaps no turn takes
/// the cluster of the turn with the nearest midpoint, if that is within
/// [`NEAREST_TURN_MS`], and otherwise stays without a speaker.
///
/// Only clusters some segment uses are returned, labelled `Remote 1`,
/// `Remote 2`… in order of their first turn, with their embedding taken from
/// `centroids` (indexed by [`Turn::cluster`]) and no speaker yet.
pub fn assign_clusters(
    segments: &mut [Segment],
    turns: &[Turn],
    centroids: &[Vec<f32>],
) -> Vec<Cluster> {
    // Diarizer cluster numbers in order of first speech.
    let mut by_first_speech: Vec<(u32, usize)> = Vec::new();
    for t in turns {
        match by_first_speech.iter_mut().find(|(_, c)| *c == t.cluster) {
            Some(first) => first.0 = first.0.min(t.start_ms),
            None => by_first_speech.push((t.start_ms, t.cluster)),
        }
    }
    by_first_speech.sort();
    let rank = |cluster: usize| {
        by_first_speech
            .iter()
            .position(|(_, c)| *c == cluster)
            .expect("every turn's cluster is ranked")
    };

    let picks: Vec<Option<usize>> = segments
        .iter()
        .map(|s| match s.side {
            Side::Me => None,
            Side::Them => most_overlap(s, turns, rank).or_else(|| nearest_turn(s, turns)),
        })
        .collect();

    let mut used: Vec<usize> = picks.iter().flatten().copied().collect();
    used.sort_by_key(|&c| rank(c));
    used.dedup();

    for (segment, pick) in segments.iter_mut().zip(&picks) {
        segment.speaker = match (segment.side, pick) {
            (Side::Me, _) => Some(SpeakerId(ME.into())),
            (Side::Them, Some(cluster)) => {
                let i = used
                    .iter()
                    .position(|c| c == cluster)
                    .expect("pick is used");
                Some(SpeakerId(format!("{CLUSTER_REF}{i}")))
            }
            (Side::Them, None) => None,
        };
    }

    used.iter()
        .enumerate()
        .map(|(i, &cluster)| Cluster {
            label: format!("Remote {}", i + 1),
            embedding: centroids.get(cluster).cloned().unwrap_or_default(),
            speaker: None,
            named_by: None,
        })
        .collect()
}

/// The cluster whose turns overlap `segment` longest; on a tie the one with
/// the lowest `rank`.
fn most_overlap(segment: &Segment, turns: &[Turn], rank: impl Fn(usize) -> usize) -> Option<usize> {
    let mut overlap: Vec<(usize, u32)> = Vec::new();
    for t in turns {
        let ms = segment
            .end_ms
            .min(t.end_ms)
            .saturating_sub(segment.start_ms.max(t.start_ms));
        if ms == 0 {
            continue;
        }
        match overlap.iter_mut().find(|(c, _)| *c == t.cluster) {
            Some((_, total)) => *total += ms,
            None => overlap.push((t.cluster, ms)),
        }
    }
    overlap
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(rank(b.0).cmp(&rank(a.0))))
        .map(|(cluster, _)| cluster)
}

/// The cluster of the turn whose midpoint is nearest `segment`'s, if within
/// [`NEAREST_TURN_MS`]; on a tie the earlier turn.
fn nearest_turn(segment: &Segment, turns: &[Turn]) -> Option<usize> {
    // Doubled midpoints keep the arithmetic in integers.
    let mid2 = |start: u32, end: u32| u64::from(start) + u64::from(end);
    let at = mid2(segment.start_ms, segment.end_ms);
    turns
        .iter()
        .map(|t| (mid2(t.start_ms, t.end_ms).abs_diff(at), t.cluster))
        .filter(|(d2, _)| *d2 <= 2 * u64::from(NEAREST_TURN_MS))
        .min_by_key(|(d2, _)| *d2)
        .map(|(_, cluster)| cluster)
}

/// Sets each cluster's speaker to the known person whose voice is most like
/// its embedding, when the cosine similarity reaches `threshold`. Clusters
/// with no such person, or with an empty embedding, are left as they are. A
/// remote voice never matches `me` or a person nobody has named (see
/// [`recall_store::Person::named`]).
pub fn name_clusters(clusters: &mut [Cluster], speakers: &Speakers, threshold: f32) {
    for cluster in clusters.iter_mut().filter(|c| !c.embedding.is_empty()) {
        let embedding = &cluster.embedding;
        let best = speakers
            .people()
            .iter()
            .filter(|p| p.named && p.id.0 != ME)
            .flat_map(|p| p.embeddings.iter().map(move |e| (p, cosine(embedding, e))))
            .filter(|(_, score)| *score >= threshold)
            .max_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((person, _)) = best {
            cluster.speaker = Some(person.id.clone());
        }
    }
}

/// Registers every cluster still without a speaker as a new unnamed person
/// labelled by the cluster and carrying its embedding, so it can be named
/// later, then
/// replaces the `cluster:{i}` ids on `segments` with the clusters' speakers.
pub fn apply_names(segments: &mut [Segment], clusters: &mut [Cluster], speakers: &mut Speakers) {
    for cluster in clusters.iter_mut().filter(|c| c.speaker.is_none()) {
        let embedding = Some(cluster.embedding.clone()).filter(|e| !e.is_empty());
        cluster.speaker = Some(speakers.add_unnamed(&cluster.label, embedding));
    }
    for segment in segments {
        let Some(i) = segment
            .speaker
            .as_ref()
            .and_then(|id| id.0.strip_prefix(CLUSTER_REF))
            .and_then(|i| i.parse::<usize>().ok())
        else {
            continue;
        };
        segment.speaker = clusters.get(i).and_then(|c| c.speaker.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start_ms: u32, end_ms: u32, side: Side) -> Segment {
        Segment {
            start_ms,
            end_ms,
            side,
            speaker: None,
            text: String::new(),
        }
    }

    fn turn(start_ms: u32, end_ms: u32, cluster: usize) -> Turn {
        Turn {
            start_ms,
            end_ms,
            cluster,
        }
    }

    fn speaker(s: &str) -> Option<SpeakerId> {
        Some(SpeakerId(s.into()))
    }

    fn cluster(label: &str, embedding: Vec<f32>) -> Cluster {
        Cluster {
            label: label.into(),
            embedding,
            speaker: None,
            named_by: None,
        }
    }

    #[test]
    fn assign_clusters_picks_max_overlap_and_labels_in_order() {
        // Sherpa cluster 1 speaks first, so it is "Remote 1"; cluster 2 says
        // nothing that was transcribed and gets no label.
        let turns = [
            turn(0, 4_000, 1),
            turn(4_000, 10_000, 0),
            turn(10_000, 12_000, 1),
            turn(30_000, 31_000, 2),
        ];
        let centroids = [vec![0.0, 1.0], vec![1.0, 0.0], vec![0.5, 0.5]];
        let mut segments = [
            seg(500, 3_500, Side::Them),     // inside cluster 1
            seg(1_000, 2_000, Side::Me),     // the microphone is always me
            seg(3_000, 9_000, Side::Them),   // 1 s of cluster 1, 5 s of cluster 0
            seg(9_500, 11_000, Side::Them),  // 0.5 s of cluster 0, 1 s of cluster 1
            seg(3_800, 4_200, Side::Them),   // 200 ms each: the earlier speaker wins
            seg(12_500, 13_500, Side::Them), // no overlap, midpoint 2 s from a cluster 1 turn
            seg(20_000, 21_000, Side::Them), // nothing within 2 s
        ];

        let clusters = assign_clusters(&mut segments, &turns, &centroids);

        assert_eq!(
            clusters,
            [
                cluster("Remote 1", vec![1.0, 0.0]),
                cluster("Remote 2", vec![0.0, 1.0]),
            ]
        );
        let speakers: Vec<_> = segments.iter().map(|s| s.speaker.clone()).collect();
        assert_eq!(
            speakers,
            [
                speaker("cluster:0"),
                speaker("me"),
                speaker("cluster:1"),
                speaker("cluster:0"),
                speaker("cluster:0"),
                speaker("cluster:0"),
                None,
            ]
        );
    }

    #[test]
    fn assign_clusters_without_turns_leaves_remote_unassigned() {
        let mut segments = [seg(0, 1_000, Side::Them), seg(0, 1_000, Side::Me)];
        assert!(assign_clusters(&mut segments, &[], &[]).is_empty());
        assert_eq!(segments[0].speaker, None);
        assert_eq!(segments[1].speaker, speaker("me"));
    }

    #[test]
    fn name_clusters_matches_known_voice_and_leaves_unknown() {
        let mut speakers = Speakers::default();
        let tom = speakers.add("Tom", Some(vec![1.0, 0.0, 0.0]));
        speakers.add_embedding(&SpeakerId(ME.into()), vec![0.0, 0.0, 1.0], 10);
        let mut clusters = [
            cluster("Remote 1", vec![0.9, 0.1, 0.0]),
            // Sounds exactly like me, but a remote voice is never me.
            cluster("Remote 2", vec![0.0, 0.0, 1.0]),
            cluster("Remote 3", vec![0.0, 1.0, 0.0]),
            cluster("Remote 4", vec![]),
        ];

        name_clusters(&mut clusters, &speakers, 0.6);

        assert_eq!(clusters[0].speaker, Some(tom));
        assert_eq!(clusters[1].speaker, None);
        assert_eq!(clusters[2].speaker, None);
        assert_eq!(clusters[3].speaker, None);

        // Even a threshold every voice passes never names an empty embedding.
        name_clusters(&mut clusters[3..], &speakers, 0.0);
        assert_eq!(clusters[3].speaker, None);
    }

    #[test]
    fn name_clusters_skips_unnamed_voices_until_they_are_named() {
        let mut speakers = Speakers::default();
        let stranger = speakers.add_unnamed("Remote 1", Some(vec![1.0, 0.0]));
        let mut clusters = [cluster("Remote 1", vec![1.0, 0.0])];

        name_clusters(&mut clusters, &speakers, 0.6);
        assert_eq!(clusters[0].speaker, None);

        speakers.rename(&stranger, "Ann").unwrap();
        name_clusters(&mut clusters, &speakers, 0.6);
        assert_eq!(clusters[0].speaker, Some(stranger));
    }

    #[test]
    fn apply_names_resolves_cluster_refs_and_registers_unknown_voices() {
        let mut speakers = Speakers::default();
        let tom = speakers.add("Tom", Some(vec![1.0, 0.0]));
        let mut clusters = [
            Cluster {
                speaker: Some(tom.clone()),
                ..cluster("Remote 1", vec![0.9, 0.1])
            },
            cluster("Remote 2", vec![0.0, 1.0]),
        ];
        let mut segments = [
            Segment {
                speaker: speaker("cluster:1"),
                ..seg(0, 1_000, Side::Them)
            },
            Segment {
                speaker: speaker("me"),
                ..seg(0, 1_000, Side::Me)
            },
            Segment {
                speaker: speaker("cluster:0"),
                ..seg(1_000, 2_000, Side::Them)
            },
            seg(2_000, 3_000, Side::Them),
        ];

        apply_names(&mut segments, &mut clusters, &mut speakers);

        let new = clusters[1]
            .speaker
            .clone()
            .expect("unknown voice registered");
        let person = speakers.get(&new).unwrap();
        assert_eq!(person.name, "Remote 2");
        assert!(!person.named);
        assert_eq!(person.embeddings, [vec![0.0, 1.0]]);
        assert_eq!(clusters[0].speaker, Some(tom.clone()));
        let ids: Vec<_> = segments.iter().map(|s| s.speaker.clone()).collect();
        assert_eq!(ids, [Some(new), speaker("me"), Some(tom), None]);
        assert_eq!(speakers.people().len(), 3);
    }
}
