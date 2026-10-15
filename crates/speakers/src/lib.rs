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

/// Similarity thresholds, for NeMo TitaNet-small embeddings: on sherpa-onnx's
/// four-speaker sample, single turns of one speaker scored 0.51–0.74 against each other
/// and different speakers at most 0.47. Averaged voiceprints score higher.
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
            high: 0.6,
            medium: 0.45,
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

/// A person's stored voice: the running mean of every embedding they were named with.
#[derive(Clone, Debug, PartialEq)]
pub struct Voiceprint {
    /// The person.
    pub person_id: i64,
    /// Mean embedding.
    pub embedding: Embedding,
    /// How many clusters were averaged into it.
    pub samples: u32,
}

impl Voiceprint {
    /// Fold another embedding into the mean. A length mismatch (a different model)
    /// replaces the voiceprint instead.
    pub fn absorb(&mut self, e: &Embedding) {
        if self.embedding.0.len() != e.0.len() || self.samples == 0 {
            self.embedding = e.clone();
            self.samples = 1;
            return;
        }
        let n = self.samples as f32;
        for (m, x) in self.embedding.0.iter_mut().zip(&e.0) {
            *m = (*m * n + x) / (n + 1.0);
        }
        self.samples += 1;
    }
}

/// A cluster's best match in the library.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Match {
    /// Who.
    pub person_id: i64,
    /// Cosine similarity.
    pub similarity: f32,
    /// What to do with it.
    pub confidence: Confidence,
}

/// Every voiceprint scored against `e`, best first; `candidates` (the meeting's
/// attendees) win ties and are listed before others of equal score.
pub fn rank(e: &Embedding, library: &[Voiceprint], candidates: &[i64]) -> Vec<(i64, f32)> {
    let mut scored: Vec<(i64, f32)> = library
        .iter()
        .map(|v| (v.person_id, e.cosine(&v.embedding)))
        .collect();
    scored.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| candidates.contains(&b.0).cmp(&candidates.contains(&a.0)))
    });
    scored
}

/// Match each cluster to at most one person and each person to at most one cluster,
/// strongest pairs first. Attendees are tried first: a non-attendee only wins a cluster
/// with a high-confidence score. Clusters left below the medium threshold get `None`.
pub fn match_clusters(
    clusters: &[Embedding],
    library: &[Voiceprint],
    candidates: &[i64],
    t: &Thresholds,
) -> Vec<Option<Match>> {
    let mut pairs: Vec<(usize, i64, f32)> = Vec::new();
    for (ci, e) in clusters.iter().enumerate() {
        for v in library {
            let sim = e.cosine(&v.embedding);
            let attendee = candidates.is_empty() || candidates.contains(&v.person_id);
            let enough = if attendee { t.medium } else { t.high };
            if sim >= enough {
                pairs.push((ci, v.person_id, sim));
            }
        }
    }
    pairs.sort_by(|a, b| b.2.total_cmp(&a.2));
    let mut out = vec![None; clusters.len()];
    let mut taken: Vec<i64> = Vec::new();
    for (ci, person_id, similarity) in pairs {
        if out[ci].is_some() || taken.contains(&person_id) {
            continue;
        }
        taken.push(person_id);
        out[ci] = Some(Match {
            person_id,
            similarity,
            confidence: t.classify(similarity),
        });
    }
    out
}

/// The first unused color of the 8-color palette, or a repeat once all are used.
pub fn next_color(used: impl IntoIterator<Item = Option<u8>>) -> u8 {
    let used: Vec<u8> = used.into_iter().flatten().collect();
    (0..8u8)
        .find(|c| !used.contains(c))
        .unwrap_or((used.len() % 8) as u8)
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
        assert_eq!(t.classify(0.5), Confidence::Medium);
        assert_eq!(t.classify(0.12), Confidence::Low);
    }

    fn vp(id: i64, v: Vec<f32>) -> Voiceprint {
        Voiceprint {
            person_id: id,
            embedding: Embedding(v),
            samples: 1,
        }
    }

    #[test]
    fn absorbing_averages() {
        let mut v = vp(1, vec![1.0, 0.0]);
        v.absorb(&Embedding(vec![0.0, 1.0]));
        assert_eq!(v.embedding.0, vec![0.5, 0.5]);
        assert_eq!(v.samples, 2);
        v.absorb(&Embedding(vec![1.0, 1.0, 1.0]));
        assert_eq!(v.embedding.0.len(), 3);
        assert_eq!(v.samples, 1);
    }

    #[test]
    fn clusters_match_one_person_each() {
        let lib = [vp(1, vec![1.0, 0.0, 0.0]), vp(2, vec![0.0, 1.0, 0.0])];
        let clusters = [
            Embedding(vec![0.9, 0.1, 0.0]),
            Embedding(vec![1.0, 0.05, 0.0]),
            Embedding(vec![0.0, 0.0, 1.0]),
        ];
        let m = match_clusters(&clusters, &lib, &[], &Thresholds::default());
        // The closer cluster wins person 1; the other gets nobody.
        assert_eq!(m[1].map(|m| m.person_id), Some(1));
        assert!(m[0].is_none());
        assert!(m[2].is_none());
    }

    #[test]
    fn non_attendees_need_a_strong_match() {
        let lib = [vp(1, vec![1.0, 0.0]), vp(2, vec![0.0, 1.0])];
        let t = Thresholds::default();
        // 0.8 to person 2, who is not an attendee: high enough to win anyway.
        let strong = Embedding(vec![0.6, 0.8]);
        let r = match_clusters(&[strong], &lib, &[1], &t);
        assert_eq!(r[0].map(|m| m.person_id), Some(2));
        // 0.53 to person 2: a medium match, kept for an attendee, dropped otherwise.
        let medium = Embedding(vec![0.85, 0.53]);
        let r = match_clusters(std::slice::from_ref(&medium), &lib[1..], &[2], &t);
        assert_eq!(r[0].map(|m| m.confidence), Some(Confidence::Medium));
        assert!(match_clusters(&[medium], &lib[1..], &[1], &t)[0].is_none());
        assert_eq!(rank(&Embedding(vec![1.0, 0.0]), &lib, &[])[0].0, 1);
    }

    #[test]
    fn colors_fill_gaps() {
        assert_eq!(next_color([Some(0), Some(1), None, Some(3)]), 2);
        assert_eq!(next_color((0..8).map(Some)), 0);
    }
}
