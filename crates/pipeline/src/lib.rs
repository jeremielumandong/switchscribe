//! The post-call pipeline (M2, M3): voice activity detection, per-track transcription,
//! diarization of the Teams track, and the merge of both tracks into one transcript.
//!
//! The speech engine and the diarizer sit behind traits so models can be swapped
//! (whisper-rs, sherpa-onnx, polyvoice) without touching the pipeline.

pub mod job;
pub mod models;
pub mod vad;
#[cfg(feature = "whisper")]
pub mod whisper;

use std::path::{Path, PathBuf};

use switchscribe_store::NewSegment;

pub use job::{JobInput, TranscriptFile, transcribe_meeting};
pub use vad::{EnergyVad, Vad};

/// Sample rate the speech models take.
pub const RATE: u32 = 16_000;

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

/// One recognized stretch of speech, times in milliseconds from the audio's start.
#[derive(Clone, Debug, PartialEq)]
pub struct Recognized {
    /// Start.
    pub start_ms: i64,
    /// End.
    pub end_ms: i64,
    /// Text.
    pub text: String,
}

/// How to transcribe.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Options {
    /// Vocabulary hint (glossary and attendee names), Whisper's initial prompt.
    pub prompt: String,
    /// Language code (`en`), or `None` to detect it.
    pub language: Option<String>,
}

/// An engine's output for one piece of audio.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Transcribed {
    /// The recognized speech.
    pub segments: Vec<Recognized>,
    /// The language used or detected.
    pub language: Option<String>,
}

/// Speech to text on 16 kHz mono audio.
pub trait SpeechEngine: Send + Sync {
    /// Transcribe a piece of audio (at most about 30 seconds).
    fn transcribe(&self, audio: &[f32], opts: &Options) -> Result<Transcribed>;
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
    /// Split 16 kHz audio into speaker turns.
    fn diarize(&self, audio: &[f32]) -> Result<Vec<Turn>>;
}

/// Pipeline errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No speech model is installed.
    #[error("no speech model is installed")]
    NoModel,
    /// A model file could not be loaded.
    #[error("could not load the model {0}")]
    Model(String),
    /// The engine failed.
    #[error("speech engine: {0}")]
    Engine(String),
    /// The meeting has no recorded tracks.
    #[error("the meeting has no recorded audio")]
    NoAudio,
    /// A track could not be read.
    #[error(transparent)]
    Audio(#[from] switchscribe_capture::Error),
    /// A file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// A download failed.
    #[error("download failed: {0}")]
    Download(String),
    /// A download did not match its checksum.
    #[error("the download is corrupt (SHA-256 {got}, expected {expected})")]
    Verify {
        /// What the server published.
        expected: String,
        /// What arrived.
        got: String,
    },
    /// Stopped on request.
    #[error("cancelled")]
    Cancelled,
}

impl Error {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Error::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

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
