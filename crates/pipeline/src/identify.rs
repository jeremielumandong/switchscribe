//! Identifying speakers after transcription: diarize the Teams track into anonymous
//! clusters, give each cluster a voiceprint sample, match the samples against the
//! library, and label every transcript line. Mic lines are always the user.

use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use switchscribe_capture::Track;
use switchscribe_speakers::{Confidence, Embedding, Thresholds, Voiceprint, match_clusters};
use switchscribe_store::NewSegment;

use crate::job::{TranscriptFile, TranscriptLine, track_label};
use crate::{Diarizer, Embedder, Error, RATE, Result, Turn, merge_tracks};

/// The speaker file in a meeting folder.
pub const SPEAKERS_FILE: &str = "speakers.json";
/// Speech used for one voiceprint sample, at most.
const SAMPLE_MS: i64 = 30_000;
/// Turns shorter than this make poor samples (used only when nothing longer exists).
const MIN_TURN_MS: i64 = 1_000;
/// Clips offered on the Name-a-speaker card, and their longest length.
const CLIPS: usize = 3;
const CLIP_MS: i64 = 6_000;

/// One voice heard on the Teams track.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cluster {
    /// Its label in the transcript ("Speaker 2").
    pub label: String,
    /// Voiceprint sample (empty when there was too little speech).
    pub embedding: Vec<f32>,
    /// How long they spoke, milliseconds.
    pub speech_ms: i64,
    /// Clips to play when naming them: start and end, milliseconds.
    pub clips: Vec<(i64, i64)>,
    /// Who they are, when known or suggested.
    pub person_id: Option<i64>,
    /// The voiceprint match behind `person_id`, if it came from one.
    pub similarity: Option<f32>,
    /// Named by the user, or matched with high confidence.
    pub confirmed: bool,
}

/// `speakers.json`: the clusters of one meeting, kept so a speaker can be named later.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpeakersFile {
    /// Clusters in order of first appearance.
    pub clusters: Vec<Cluster>,
    /// A sample of the user's own voice from the mic track.
    pub me: Option<Vec<f32>>,
}

impl SpeakersFile {
    /// Read a meeting's speaker file; missing or unreadable reads as `None`.
    pub fn load(dir: &Path) -> Option<SpeakersFile> {
        let text = std::fs::read_to_string(dir.join(SPEAKERS_FILE)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Write it (atomically, so a crash never leaves half a file).
    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(SPEAKERS_FILE);
        let tmp = dir.join(format!("{SPEAKERS_FILE}.tmp"));
        let json = serde_json::to_string_pretty(self).map_err(|e| Error::Engine(e.to_string()))?;
        std::fs::write(&tmp, json).map_err(|e| Error::io(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::io(&path, e))
    }

    /// Clusters still waiting for a name (unmatched, or only suggested).
    pub fn unnamed(&self) -> impl Iterator<Item = &Cluster> {
        self.clusters.iter().filter(|c| !c.confirmed)
    }
}

/// What identification produced.
#[derive(Clone, Debug, PartialEq)]
pub struct Identified {
    /// The whole transcript, labelled.
    pub segments: Vec<NewSegment>,
    /// The clusters, for `speakers.json`.
    pub file: SpeakersFile,
}

/// Who might be speaking: the library, and the meeting's attendees to try first.
pub struct Known<'a> {
    /// Stored voiceprints.
    pub library: &'a [Voiceprint],
    /// Attendee person ids.
    pub candidates: &'a [i64],
    /// The user.
    pub me: Option<i64>,
    /// Match thresholds.
    pub thresholds: Thresholds,
}

/// Identify the speakers in the meeting in `dir`, whose transcript is `transcript`.
pub fn identify_speakers(
    dir: &Path,
    transcript: &TranscriptFile,
    diarizer: &dyn Diarizer,
    embedder: &dyn Embedder,
    known: &Known<'_>,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> Result<Identified> {
    progress(0.0);
    let remote: Vec<&TranscriptLine> = transcript
        .lines
        .iter()
        .filter(|l| l.track == Track::Remote)
        .collect();
    let mic: Vec<&TranscriptLine> = transcript
        .lines
        .iter()
        .filter(|l| l.track == Track::Mic)
        .collect();

    let decode = |track: Track| {
        let path = dir.join(track.file_name());
        if path.exists() {
            switchscribe_capture::ogg_opus::decode_file_at(&path, RATE).map_err(Error::from)
        } else {
            Ok(Vec::new())
        }
    };

    let mut clusters = Vec::new();
    let mut line_cluster: Vec<usize> = Vec::new();
    if !remote.is_empty() {
        let audio = decode(Track::Remote)?;
        let mut turns = diarizer.diarize(&audio)?;
        if turns.is_empty() {
            // Too little for the diarizer: one voice.
            turns = remote
                .iter()
                .map(|l| Turn {
                    start_ms: l.start_ms,
                    end_ms: l.end_ms,
                    cluster: 0,
                })
                .collect();
        }
        progress(0.6);
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let assigned: Vec<usize> = remote.iter().map(|l| nearest_cluster(l, &turns)).collect();
        // Clusters that own a line, numbered by first appearance.
        let mut order: Vec<usize> = Vec::new();
        for c in &assigned {
            if !order.contains(c) {
                order.push(*c);
            }
        }
        for (i, &c) in order.iter().enumerate() {
            let own: Vec<&Turn> = turns.iter().filter(|t| t.cluster == c).collect();
            let embedding = sample(&audio, &own)
                .map(|s| embedder.embed(&s))
                .transpose()
                .unwrap_or_else(|e| {
                    tracing::warn!("no voiceprint sample for a speaker: {e}");
                    None
                })
                .unwrap_or_default();
            clusters.push(Cluster {
                label: format!("Speaker {}", i + 1),
                embedding,
                speech_ms: own.iter().map(|t| t.end_ms - t.start_ms).sum(),
                clips: clips(&own),
                person_id: None,
                similarity: None,
                confirmed: false,
            });
            progress(0.6 + 0.3 * (i + 1) as f32 / order.len() as f32);
        }
        line_cluster = assigned
            .iter()
            .map(|c| order.iter().position(|o| o == c).unwrap_or(0))
            .collect();
    }

    // Name what the library recognizes.
    let samples: Vec<Embedding> = clusters
        .iter()
        .map(|c| Embedding(c.embedding.clone()))
        .collect();
    let matches = match_clusters(&samples, known.library, known.candidates, &known.thresholds);
    for (c, m) in clusters.iter_mut().zip(matches) {
        if let Some(m) = m {
            c.person_id = Some(m.person_id);
            c.similarity = Some(m.similarity);
            c.confirmed = m.confidence == Confidence::High;
        }
    }

    // A sample of the user's voice, to keep their voiceprint current.
    let me = if mic.is_empty() {
        None
    } else {
        let audio = decode(Track::Mic)?;
        let turns: Vec<Turn> = mic
            .iter()
            .map(|l| Turn {
                start_ms: l.start_ms,
                end_ms: l.end_ms,
                cluster: 0,
            })
            .collect();
        let own: Vec<&Turn> = turns.iter().collect();
        sample(&audio, &own).and_then(|s| embedder.embed(&s).ok())
    };
    progress(1.0);

    let mic_segments = mic
        .iter()
        .map(|l| NewSegment {
            start_ms: l.start_ms,
            end_ms: l.end_ms,
            person_id: known.me,
            speaker_label: track_label(Track::Mic).to_owned(),
            text: l.text.clone(),
            low_confidence: false,
        })
        .collect();
    let remote_segments = remote
        .iter()
        .zip(&line_cluster)
        .map(|(l, &c)| {
            let cluster = &clusters[c];
            NewSegment {
                start_ms: l.start_ms,
                end_ms: l.end_ms,
                person_id: cluster.person_id,
                speaker_label: cluster.label.clone(),
                text: l.text.clone(),
                low_confidence: cluster.person_id.is_some() && !cluster.confirmed,
            }
        })
        .collect();
    Ok(Identified {
        segments: merge_tracks(mic_segments, remote_segments),
        file: SpeakersFile { clusters, me },
    })
}

/// The cluster that overlaps a line most, or the nearest one when none overlaps.
fn nearest_cluster(line: &TranscriptLine, turns: &[Turn]) -> usize {
    let overlap = |t: &Turn| (line.end_ms.min(t.end_ms) - line.start_ms.max(t.start_ms)).max(0);
    let gap = |t: &Turn| {
        if t.end_ms < line.start_ms {
            line.start_ms - t.end_ms
        } else {
            (t.start_ms - line.end_ms).max(0)
        }
    };
    let mut by_cluster: Vec<(usize, i64)> = Vec::new();
    for t in turns {
        match by_cluster.iter_mut().find(|(c, _)| *c == t.cluster) {
            Some((_, o)) => *o += overlap(t),
            None => by_cluster.push((t.cluster, overlap(t))),
        }
    }
    match by_cluster.iter().max_by_key(|(_, o)| *o) {
        Some(&(c, o)) if o > 0 => c,
        _ => turns.iter().min_by_key(|t| gap(t)).map_or(0, |t| t.cluster),
    }
}

/// Up to [`SAMPLE_MS`] of a speaker's longest turns, joined.
fn sample(audio: &[f32], turns: &[&Turn]) -> Option<Vec<f32>> {
    let mut by_len: Vec<&&Turn> = turns.iter().collect();
    by_len.sort_by_key(|t| std::cmp::Reverse(t.end_ms - t.start_ms));
    let long: Vec<&&Turn> = by_len
        .iter()
        .copied()
        .filter(|t| t.end_ms - t.start_ms >= MIN_TURN_MS)
        .collect();
    let picked = if long.is_empty() { by_len } else { long };
    let mut out = Vec::new();
    let mut ms = 0;
    for t in picked {
        if ms >= SAMPLE_MS {
            break;
        }
        let r = samples_of(
            t.start_ms,
            t.end_ms.min(t.start_ms + SAMPLE_MS - ms),
            audio.len(),
        );
        ms += (r.end - r.start) as i64 * 1000 / RATE as i64;
        out.extend_from_slice(&audio[r]);
    }
    (!out.is_empty()).then_some(out)
}

fn samples_of(start_ms: i64, end_ms: i64, len: usize) -> Range<usize> {
    let at = |ms: i64| ((ms.max(0) as usize) * RATE as usize / 1000).min(len);
    at(start_ms)..at(end_ms).max(at(start_ms))
}

/// The longest turns as playable clips, in time order.
fn clips(turns: &[&Turn]) -> Vec<(i64, i64)> {
    let mut by_len: Vec<&&Turn> = turns.iter().collect();
    by_len.sort_by_key(|t| std::cmp::Reverse(t.end_ms - t.start_ms));
    let mut out: Vec<(i64, i64)> = by_len
        .into_iter()
        .take(CLIPS)
        .map(|t| (t.start_ms, t.end_ms.min(t.start_ms + CLIP_MS)))
        .collect();
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use switchscribe_capture::{Backend, Recorder};

    use super::*;

    /// Two "speakers" on the Teams track: before and after 2 s.
    struct SplitDiarizer;
    impl Diarizer for SplitDiarizer {
        fn diarize(&self, audio: &[f32]) -> Result<Vec<Turn>> {
            let ms = (audio.len() * 1000 / RATE as usize) as i64;
            Ok(vec![
                Turn {
                    start_ms: 0,
                    end_ms: 2_000,
                    cluster: 7,
                },
                Turn {
                    start_ms: 2_000,
                    end_ms: ms,
                    cluster: 3,
                },
            ])
        }
    }

    /// Embeds by when the audio is loud: the fake far voice talks 4–7 s, so the second
    /// cluster's sample is louder than the first's.
    struct LevelEmbedder;
    impl Embedder for LevelEmbedder {
        fn embed(&self, audio: &[f32]) -> Result<Vec<f32>> {
            let rms = (audio.iter().map(|s| s * s).sum::<f32>() / audio.len().max(1) as f32).sqrt();
            Ok(if rms > 0.01 {
                vec![0.0, 1.0]
            } else {
                vec![1.0, 0.0]
            })
        }
    }

    fn line(start_ms: i64, end_ms: i64, track: Track) -> TranscriptLine {
        TranscriptLine {
            start_ms,
            end_ms,
            track,
            text: format!("{start_ms}"),
        }
    }

    #[test]
    fn labels_clusters_and_names_known_voices() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::start(dir.path(), Backend::fake()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(6500));
        rec.stop().unwrap();
        let transcript = TranscriptFile {
            language: None,
            lines: vec![
                line(200, 3_800, Track::Mic),
                line(500, 1_500, Track::Remote),
                line(4_200, 6_000, Track::Remote),
            ],
        };
        let library = [Voiceprint {
            person_id: 42,
            embedding: Embedding(vec![0.0, 1.0]),
            samples: 3,
        }];
        let known = Known {
            library: &library,
            candidates: &[],
            me: Some(1),
            thresholds: Thresholds::default(),
        };
        let mut seen = Vec::new();
        let out = identify_speakers(
            dir.path(),
            &transcript,
            &SplitDiarizer,
            &LevelEmbedder,
            &known,
            &mut |p| seen.push(p),
            &AtomicBool::new(false),
        )
        .unwrap();
        let labels: Vec<&str> = out
            .segments
            .iter()
            .map(|s| s.speaker_label.as_str())
            .collect();
        assert_eq!(labels, ["You", "Speaker 1", "Speaker 2"]);
        assert_eq!(out.segments[0].person_id, Some(1));
        // Speaker 1 (quiet before 2 s) is unknown; Speaker 2 matches person 42.
        assert_eq!(out.segments[1].person_id, None);
        assert_eq!(out.segments[2].person_id, Some(42));
        assert!(!out.segments[2].low_confidence);
        let c = &out.file.clusters;
        assert_eq!(c.len(), 2);
        assert!(c[1].confirmed && !c[0].confirmed);
        assert_eq!(out.file.unnamed().count(), 1);
        assert!(!c[0].clips.is_empty());
        assert!(out.file.me.is_some());
        assert_eq!(seen.last(), Some(&1.0));

        out.file.save(dir.path()).unwrap();
        assert_eq!(SpeakersFile::load(dir.path()), Some(out.file));
    }

    #[test]
    fn lines_go_to_the_overlapping_or_nearest_turn() {
        let t = |s, e, c| Turn {
            start_ms: s,
            end_ms: e,
            cluster: c,
        };
        let turns = [t(0, 1000, 0), t(1000, 5000, 1), t(8000, 9000, 2)];
        assert_eq!(nearest_cluster(&line(500, 3000, Track::Remote), &turns), 1);
        assert_eq!(nearest_cluster(&line(7000, 7500, Track::Remote), &turns), 2);
        assert_eq!(nearest_cluster(&line(0, 100, Track::Remote), &[]), 0);
        let refs: Vec<&Turn> = turns.iter().collect();
        assert_eq!(clips(&refs), vec![(0, 1000), (1000, 5000), (8000, 9000)]);
    }
}
