//! Audio capture (M1): Teams through WASAPI process loopback, the microphone through
//! WASAPI capture, each written to its own Opus track, plus Teams meeting detection.
//!
//! M0 holds only the shared vocabulary; the Windows implementation lands in M1.

use serde::{Deserialize, Serialize};

/// The two tracks every recording has. Keeping them apart is what makes "who is
/// speaking" reliable later: everything on [`Track::Mic`] is the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Track {
    /// The user's microphone.
    Mic,
    /// Teams' output (process loopback, or system loopback as a fallback).
    Remote,
}

impl Track {
    /// Both tracks.
    pub const ALL: [Track; 2] = [Track::Mic, Track::Remote];

    /// The track's file name inside a meeting folder.
    pub fn file_name(self) -> &'static str {
        match self {
            Track::Mic => "mic.opus",
            Track::Remote => "remote.opus",
        }
    }

    /// Label on the mini recorder's level meters.
    pub fn meter_label(self) -> &'static str {
        match self {
            Track::Mic => "You",
            Track::Remote => "Teams",
        }
    }
}

/// What the recorder is doing; the tray icon mirrors it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RecorderState {
    /// Nothing detected.
    #[default]
    Idle,
    /// A Teams meeting was detected and recording is offered.
    Armed,
    /// Capturing audio.
    Recording,
    /// Recording is paused.
    Paused,
    /// The recording finished and the pipeline is running.
    Processing,
}

/// Capture errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Audio capture is not implemented on this platform yet.
    #[error("audio capture is not available on this platform yet")]
    Unsupported,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_have_distinct_files() {
        assert_ne!(Track::Mic.file_name(), Track::Remote.file_name());
        assert_eq!(Track::Remote.meter_label(), "Teams");
    }
}
