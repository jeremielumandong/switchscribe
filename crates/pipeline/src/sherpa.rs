//! sherpa-onnx: pyannote segmentation plus TitaNet embeddings for diarizing the Teams
//! track, and the same embeddings for voiceprints.

use std::path::Path;

use sherpa_onnx::{
    OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig, SpeakerEmbeddingExtractor,
    SpeakerEmbeddingExtractorConfig,
};

use crate::{Diarizer, Embedder, Error, RATE, Result, Turn};

/// Clustering threshold for TitaNet-small: on sherpa-onnx's four-speaker sample, 0.9
/// found four speakers; 0.5 found eight.
pub const CLUSTER_THRESHOLD: f32 = 0.9;

fn threads() -> i32 {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 8) as i32)
        .unwrap_or(4)
}

/// sherpa-onnx aborts on a missing model rather than failing, so check first.
fn model_path(path: &Path) -> Result<String> {
    let ok = std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0);
    if !ok {
        return Err(Error::Model(format!("{}: not found", path.display())));
    }
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Model(format!("{}: not a UTF-8 path", path.display())))
}

/// Offline diarization: segmentation, embeddings and clustering in one pass.
pub struct SherpaDiarizer {
    inner: OfflineSpeakerDiarization,
}

impl SherpaDiarizer {
    /// Load the segmentation and embedding models.
    pub fn load(segmentation: &Path, embedding: &Path) -> Result<Self> {
        let mut config = OfflineSpeakerDiarizationConfig::default();
        config.segmentation.pyannote.model = Some(model_path(segmentation)?);
        config.segmentation.num_threads = threads();
        config.embedding.model = Some(model_path(embedding)?);
        config.embedding.num_threads = threads();
        config.clustering.threshold = CLUSTER_THRESHOLD;
        let inner = OfflineSpeakerDiarization::create(&config)
            .ok_or_else(|| Error::Model("could not load the diarization models".into()))?;
        Ok(Self { inner })
    }
}

impl Diarizer for SherpaDiarizer {
    fn diarize(&self, audio: &[f32]) -> Result<Vec<Turn>> {
        if audio.len() < RATE as usize {
            return Ok(Vec::new());
        }
        let result = self
            .inner
            .process(audio)
            .ok_or_else(|| Error::Engine("diarization failed".into()))?;
        Ok(result
            .sort_by_start_time()
            .into_iter()
            .map(|s| Turn {
                start_ms: (s.start * 1000.0) as i64,
                end_ms: (s.end * 1000.0) as i64,
                cluster: s.speaker.max(0) as usize,
            })
            .collect())
    }
}

/// Speaker embeddings for voiceprints.
pub struct SherpaEmbedder {
    inner: SpeakerEmbeddingExtractor,
}

impl SherpaEmbedder {
    /// Load the embedding model.
    pub fn load(model: &Path) -> Result<Self> {
        let config = SpeakerEmbeddingExtractorConfig {
            model: Some(model_path(model)?),
            num_threads: threads(),
            ..SpeakerEmbeddingExtractorConfig::default()
        };
        let inner = SpeakerEmbeddingExtractor::create(&config)
            .ok_or_else(|| Error::Model("could not load the voiceprint model".into()))?;
        Ok(Self { inner })
    }
}

impl Embedder for SherpaEmbedder {
    fn embed(&self, audio: &[f32]) -> Result<Vec<f32>> {
        let stream = self
            .inner
            .create_stream()
            .ok_or_else(|| Error::Engine("embedding stream".into()))?;
        stream.accept_waveform(RATE as i32, audio);
        stream.input_finished();
        if !self.inner.is_ready(&stream) {
            return Err(Error::NoAudio);
        }
        self.inner
            .compute(&stream)
            .ok_or_else(|| Error::Engine("embedding failed".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs only with `SWITCHSCRIBE_TEST_MODELS` pointing at a folder holding the two
    /// speaker models and sherpa-onnx's `0-four-speakers-zh.wav` (as `four.wav`).
    #[test]
    fn diarizes_the_four_speaker_sample() {
        let Some(dir) = std::env::var_os("SWITCHSCRIBE_TEST_MODELS") else {
            return;
        };
        let dir = Path::new(&dir);
        let seg = dir.join(crate::models::SEGMENTATION_MODEL.file);
        let emb = dir.join(crate::models::EMBEDDING_MODEL.file);
        let wave = sherpa_onnx::Wave::read(&dir.join("four.wav").to_string_lossy()).unwrap();
        let turns = SherpaDiarizer::load(&seg, &emb)
            .unwrap()
            .diarize(wave.samples())
            .unwrap();
        let mut clusters: Vec<usize> = turns.iter().map(|t| t.cluster).collect();
        clusters.sort_unstable();
        clusters.dedup();
        assert_eq!(clusters.len(), 4, "{turns:?}");
        let e = SherpaEmbedder::load(&emb).unwrap();
        let v = e.embed(&wave.samples()[..16_000 * 3]).unwrap();
        assert_eq!(v.len(), 192);
    }

    #[test]
    fn missing_models_fail_cleanly() {
        let missing = Path::new("/nonexistent/model.onnx");
        assert!(matches!(
            SherpaEmbedder::load(missing),
            Err(Error::Model(_))
        ));
        assert!(matches!(
            SherpaDiarizer::load(missing, missing),
            Err(Error::Model(_))
        ));
    }
}
