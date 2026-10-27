//! Speaker diarization of the remote track: who spoke when, and a voice
//! embedding per speaker.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use sherpa_onnx::{
    FastClusteringConfig, OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig,
    OfflineSpeakerSegmentationModelConfig, OfflineSpeakerSegmentationPyannoteModelConfig,
    SpeakerEmbeddingExtractor, SpeakerEmbeddingExtractorConfig,
};

use crate::decode::SAMPLE_RATE;
use crate::models::{self, ModelId};

/// A stretch of audio one speaker cluster talks in, in milliseconds from the
/// start of the recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Turn {
    pub start_ms: u32,
    pub end_ms: u32,
    /// Cluster number from the diarizer, an index into its centroids.
    pub cluster: usize,
}

impl Turn {
    fn len_ms(&self) -> u32 {
        self.end_ms.saturating_sub(self.start_ms)
    }
}

/// Splits 16 kHz mono audio into speaker turns.
pub trait Diarizer: Send {
    /// Turns in time order, and one L2-normalised voice embedding per
    /// cluster, indexed by [`Turn::cluster`]. A cluster whose turns were all
    /// too short to embed gets an empty embedding.
    fn diarize(&mut self, samples16k: &[f32]) -> Result<(Vec<Turn>, Vec<Vec<f32>>)>;
}

/// Agglomerative clustering stops merging clusters whose distance is above
/// this.
const CLUSTER_THRESHOLD: f32 = 0.5;
/// Seconds: shorter speech is dropped.
const MIN_DURATION_ON: f32 = 0.3;
/// Seconds: shorter gaps within one speaker are filled.
const MIN_DURATION_OFF: f32 = 0.5;
/// How many of a cluster's longest turns its centroid averages.
const CENTROID_TURNS: usize = 3;

/// Pyannote segmentation plus CAM++ embeddings through sherpa-onnx.
pub struct SherpaDiarizer {
    diarization: OfflineSpeakerDiarization,
    // A second copy of CAM++: sherpa's diarizer does not expose the
    // embeddings it clusters, so centroids are computed again from the turns.
    extractor: SpeakerEmbeddingExtractor,
}

impl SherpaDiarizer {
    /// Loads the pyannote and CAM++ models from the `models` directory.
    pub fn load(models: &Path, threads: usize) -> Result<Self> {
        let segmentation = models::path(models, ModelId::PyannoteSeg);
        let embedding = models::path(models, ModelId::CamPlusPlus);
        for model in [&segmentation, &embedding] {
            if !model.is_file() {
                bail!("diarization model not found at {}", model.display());
            }
        }
        let num_threads = i32::try_from(threads.max(1)).context("thread count")?;
        let embedding_config = SpeakerEmbeddingExtractorConfig {
            model: Some(embedding.to_string_lossy().into_owned()),
            num_threads,
            debug: false,
            provider: Some("cpu".into()),
        };
        let config = OfflineSpeakerDiarizationConfig {
            segmentation: OfflineSpeakerSegmentationModelConfig {
                pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                    model: Some(segmentation.to_string_lossy().into_owned()),
                    ..Default::default()
                },
                num_threads,
                debug: false,
                provider: Some("cpu".into()),
            },
            embedding: embedding_config.clone(),
            clustering: FastClusteringConfig {
                num_clusters: -1,
                threshold: CLUSTER_THRESHOLD,
                compute_confidence: false,
            },
            min_duration_on: MIN_DURATION_ON,
            min_duration_off: MIN_DURATION_OFF,
        };
        let diarization = OfflineSpeakerDiarization::create(&config).ok_or_else(|| {
            anyhow!(
                "loading speaker diarization from {} and {}",
                segmentation.display(),
                embedding.display()
            )
        })?;
        let rate = diarization.sample_rate();
        if rate != SAMPLE_RATE as i32 {
            bail!("diarization model expects {rate} Hz audio, not {SAMPLE_RATE} Hz");
        }
        let extractor = SpeakerEmbeddingExtractor::create(&embedding_config)
            .ok_or_else(|| anyhow!("loading speaker embedding from {}", embedding.display()))?;
        Ok(Self {
            diarization,
            extractor,
        })
    }

    /// The CAM++ embedding of `samples`, or `None` when they are too short.
    fn embed(&self, samples: &[f32]) -> Option<Vec<f32>> {
        let stream = self.extractor.create_stream()?;
        stream.accept_waveform(SAMPLE_RATE as i32, samples);
        stream.input_finished();
        if !self.extractor.is_ready(&stream) {
            return None;
        }
        self.extractor.compute(&stream)
    }
}

impl Diarizer for SherpaDiarizer {
    fn diarize(&mut self, samples16k: &[f32]) -> Result<(Vec<Turn>, Vec<Vec<f32>>)> {
        if samples16k.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let result = self
            .diarization
            .process(samples16k)
            .ok_or_else(|| anyhow!("speaker diarization failed"))?;
        // Sherpa's speaker numbers can skip values (one speaker came back as
        // speaker 1), so they are renumbered 0, 1… in order of first speech.
        let mut speaker_ids: Vec<i32> = Vec::new();
        let turns: Vec<Turn> = result
            .sort_by_start_time()
            .iter()
            .map(|s| {
                let cluster = match speaker_ids.iter().position(|&id| id == s.speaker) {
                    Some(i) => i,
                    None => {
                        speaker_ids.push(s.speaker);
                        speaker_ids.len() - 1
                    }
                };
                Turn {
                    start_ms: seconds_to_ms(s.start),
                    end_ms: seconds_to_ms(s.end),
                    cluster,
                }
            })
            .collect();
        let centroids = (0..speaker_ids.len())
            .map(|cluster| {
                let embeddings: Vec<Vec<f32>> = longest_turns(&turns, cluster, CENTROID_TURNS)
                    .into_iter()
                    .filter_map(|t| self.embed(slice_ms(samples16k, t)))
                    .collect();
                mean_normalized(&embeddings)
            })
            .collect();
        Ok((turns, centroids))
    }
}

fn seconds_to_ms(seconds: f32) -> u32 {
    // `as` saturates: negative and NaN give 0.
    (seconds * 1000.0).round() as u32
}

/// The samples of `turn`, clipped to the audio.
fn slice_ms<'a>(samples: &'a [f32], turn: &Turn) -> &'a [f32] {
    let at = |ms: u32| (ms as usize * SAMPLE_RATE as usize / 1000).min(samples.len());
    let start = at(turn.start_ms);
    &samples[start..at(turn.end_ms).max(start)]
}

/// Up to `n` of `cluster`'s turns, longest first; equal lengths keep time
/// order.
fn longest_turns(turns: &[Turn], cluster: usize, n: usize) -> Vec<&Turn> {
    let mut own: Vec<&Turn> = turns.iter().filter(|t| t.cluster == cluster).collect();
    own.sort_by_key(|t| std::cmp::Reverse(t.len_ms()));
    own.truncate(n);
    own
}

/// The element-wise mean of `vectors`, scaled to unit length. Empty input or
/// a zero mean gives an empty vector.
fn mean_normalized(vectors: &[Vec<f32>]) -> Vec<f32> {
    let Some(first) = vectors.first() else {
        return Vec::new();
    };
    let dim = first.len();
    let mut mean = vec![0.0f32; dim];
    for v in vectors.iter().filter(|v| v.len() == dim) {
        for (m, x) in mean.iter_mut().zip(v) {
            *m += x;
        }
    }
    let norm = mean.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 || !norm.is_finite() {
        return Vec::new();
    }
    mean.iter().map(|x| x / norm).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn turn(start_ms: u32, end_ms: u32, cluster: usize) -> Turn {
        Turn {
            start_ms,
            end_ms,
            cluster,
        }
    }

    #[test]
    fn longest_turns_takes_the_longest_of_one_cluster() {
        let turns = [
            turn(0, 1_000, 0),
            turn(1_000, 5_000, 1),
            turn(5_000, 7_000, 0),
            turn(7_000, 7_500, 0),
            turn(8_000, 11_000, 0),
        ];
        let picked: Vec<Turn> = longest_turns(&turns, 0, 3).into_iter().copied().collect();
        assert_eq!(
            picked,
            [
                turn(8_000, 11_000, 0),
                turn(5_000, 7_000, 0),
                turn(0, 1_000, 0)
            ]
        );
        assert!(longest_turns(&turns, 2, 3).is_empty());
    }

    #[test]
    fn mean_normalized_averages_to_unit_length() {
        let mean = mean_normalized(&[vec![2.0, 0.0], vec![0.0, 2.0]]);
        let half = 0.5f32.sqrt();
        assert!((mean[0] - half).abs() < 1e-6 && (mean[1] - half).abs() < 1e-6);
        assert!(mean_normalized(&[]).is_empty());
        assert!(mean_normalized(&[vec![1.0], vec![-1.0]]).is_empty());
    }

    #[test]
    fn slice_ms_clips_to_the_audio() {
        let samples = vec![0.0f32; 16_000];
        assert_eq!(slice_ms(&samples, &turn(500, 750, 0)).len(), 4_000);
        assert_eq!(slice_ms(&samples, &turn(900, 2_000, 0)).len(), 1_600);
        assert!(slice_ms(&samples, &turn(3_000, 4_000, 0)).is_empty());
    }

    /// The models directory under `$RECALL_HOME` or `~/.local/share/recall`
    /// holding both diarization models.
    fn installed_models_dir() -> Option<PathBuf> {
        let candidates = [
            std::env::var_os("RECALL_HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join("models")),
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/recall/models")),
        ];
        candidates.into_iter().flatten().find(|dir| {
            [ModelId::PyannoteSeg, ModelId::CamPlusPlus]
                .iter()
                .all(|&id| models::path(dir, id).is_file())
        })
    }

    #[test]
    fn sherpa_finds_one_speaker_in_fixture() {
        let Some(models) = installed_models_dir() else {
            eprintln!(
                "skipping sherpa_finds_one_speaker_in_fixture: pyannote and CAM++ models not \
                 found under $RECALL_HOME/models or ~/.local/share/recall/models"
            );
            return;
        };
        // 1 s silence, 6.6 s of one LibriSpeech speaker, 1 s silence.
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/speech.opus");
        let (samples, _) = crate::decode_stereo(&fixture).unwrap();

        let mut diarizer = SherpaDiarizer::load(&models, 1).unwrap();
        let (turns, centroids) = diarizer.diarize(&samples).unwrap();

        assert!(!turns.is_empty());
        assert!(turns.iter().all(|t| t.cluster == 0), "{turns:?}");
        assert!(turns.windows(2).all(|w| w[0].start_ms <= w[1].start_ms));
        assert_eq!(centroids.len(), 1);
        let norm = centroids[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "centroid norm {norm}");
        assert_eq!(diarizer.diarize(&[]).unwrap(), (vec![], vec![]));
    }
}
