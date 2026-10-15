//! The voiceprint library (M3): speaker embeddings averaged per person, matched to
//! diarized clusters by cosine similarity.

use serde::{Deserialize, Serialize};

/// A speaker embedding.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Embedding(pub Vec<f32>);

impl Embedding {
    /// Cosine similarity in -1..=1; 0 for empty, zero or mismatched vectors.
    pub fn cosine(&self, other: &Embedding) -> f32 {
        if self.0.len() != other.0.len() || self.0.is_empty() {
            return 0.0;
        }
        let (mut dot, mut a, mut b) = (0.0f32, 0.0f32, 0.0f32);
        for (x, y) in self.0.iter().zip(&other.0) {
            dot += x * y;
            a += x * x;
            b += y * y;
        }
        if a == 0.0 || b == 0.0 {
            0.0
        } else {
            dot / (a.sqrt() * b.sqrt())
        }
    }
}

/// How sure a voiceprint match is, which decides what the UI does with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    /// Name the cluster automatically.
    High,
    /// Suggest the name and ask to confirm (a dashed "Alex?" chip).
    Medium,
    /// Leave it as "Speaker N" and ask once per meeting.
    Low,
}

/// Similarity thresholds. Placeholders until the M3 benchmarks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    /// At or above: [`Confidence::High`].
    pub high: f32,
    /// At or above (and below `high`): [`Confidence::Medium`].
    pub medium: f32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            high: 0.75,
            medium: 0.5,
        }
    }
}

impl Thresholds {
    /// The confidence band for a similarity.
    pub fn classify(&self, similarity: f32) -> Confidence {
        if similarity >= self.high {
            Confidence::High
        } else if similarity >= self.medium {
            Confidence::Medium
        } else {
            Confidence::Low
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine() {
        let a = Embedding(vec![1.0, 0.0]);
        assert!((a.cosine(&Embedding(vec![2.0, 0.0])) - 1.0).abs() < 1e-6);
        assert!(a.cosine(&Embedding(vec![0.0, 1.0])).abs() < 1e-6);
        assert_eq!(a.cosine(&Embedding(vec![1.0])), 0.0);
        assert_eq!(a.cosine(&Embedding(vec![0.0, 0.0])), 0.0);
    }

    #[test]
    fn bands() {
        let t = Thresholds::default();
        assert_eq!(t.classify(0.82), Confidence::High);
        assert_eq!(t.classify(0.6), Confidence::Medium);
        assert_eq!(t.classify(0.12), Confidence::Low);
    }
}
