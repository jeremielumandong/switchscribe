//! The post-call pipeline (M2, M3): voice activity detection, per-track transcription,
//! diarization of the Teams track, and the merge of both tracks into one transcript.
//!
//! The speech engine and the diarizer sit behind traits so models can be swapped
//! (whisper-rs, sherpa-onnx, polyvoice) without touching the pipeline.

use switchscribe_store::NewSegment;

/// The pipeline's steps, as the meeting view's processing stepper shows them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Step {
    /// Writing the recorded tracks.
    Capture,
    /// Speech to text.
    Transcribe,
    /// Diarization and voiceprint matching.
    IdentifySpeakers,
    /// The coding CLI writes the summary.
    Summarize,
}

impl Step {
    /// Every step, in order.
    pub const ALL: [Step; 4] = [
        Step::Capture,
        Step::Transcribe,
        Step::IdentifySpeakers,
        Step::Summarize,
    ];

    /// Label.
    pub fn label(self) -> &'static str {
        match self {
            Step::Capture => "Capture",
            Step::Transcribe => "Transcribe",
            Step::IdentifySpeakers => "Identify speakers",
            Step::Summarize => "Summarize",
        }
    }
}

/// 16 kHz mono samples, the speech models' input.
pub type Samples<'a> = &'a [f32];

/// One recognized stretch of speech, times in milliseconds from the track's start.
#[derive(Clone, Debug, PartialEq)]
pub struct Recognized {
    /// Start.
    pub start_ms: i64,
    /// End.
    pub end_ms: i64,
    /// Text.
    pub text: String,
}

/// Speech to text.
pub trait SpeechEngine: Send + Sync {
    /// Transcribe a track. `prompt` is the vocabulary hint (glossary and attendee names).
    fn transcribe(&self, audio: Samples<'_>, prompt: &str) -> Result<Vec<Recognized>, Error>;
}

/// A speaker cluster's span on the Teams track.
#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    /// Start, milliseconds.
    pub start_ms: i64,
    /// End, milliseconds.
    pub end_ms: i64,
    /// Anonymous cluster index (Speaker 1, 2, …).
    pub cluster: usize,
}

/// Who spoke when, on the Teams track.
pub trait Diarizer: Send + Sync {
    /// Split a track into speaker turns.
    fn diarize(&self, audio: Samples<'_>) -> Result<Vec<Turn>, Error>;
}

/// Pipeline errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No speech model is installed.
    #[error("no speech model is installed")]
    NoModel,
}

/// Interleave both tracks' segments by start time into one transcript (stable, so the
/// mic segment comes first on a tie).
pub fn merge_tracks(mic: Vec<NewSegment>, remote: Vec<NewSegment>) -> Vec<NewSegment> {
    let mut all: Vec<NewSegment> = mic.into_iter().chain(remote).collect();
    all.sort_by_key(|s| s.start_ms);
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(start_ms: i64, text: &str) -> NewSegment {
        NewSegment {
            start_ms,
            end_ms: start_ms + 1000,
            person_id: None,
            speaker_label: String::new(),
            text: text.into(),
            low_confidence: false,
        }
    }

    #[test]
    fn merge_interleaves_by_start() {
        let merged = merge_tracks(
            vec![s(0, "a"), s(5000, "c"), s(9000, "e")],
            vec![s(2000, "b"), s(5000, "d")],
        );
        let texts: Vec<&str> = merged.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn steps_in_order() {
        assert_eq!(Step::ALL[0], Step::Capture);
        assert_eq!(Step::ALL[3].label(), "Summarize");
    }
}
