//! Audio capture: Teams through WASAPI process loopback and the microphone through WASAPI
//! capture, each written to its own mono Opus track in the meeting folder.
//!
//! [`Recorder`] runs a recording; [`Backend::system`] opens the real devices on Windows and
//! [`Backend::fake`] two synthetic voices anywhere else (tests, `SWITCHSCRIBE_FAKE_AUDIO`).
//! [`detect`] watches for Teams calls.

pub mod audio;
pub mod detect;
pub mod ogg_opus;
mod playback;
mod recorder;
mod source;
mod stretch;
#[cfg(windows)]
mod windows;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use playback::{Player, envelope};
pub use recorder::{
    Backend, CAPTURE_FILE, Monitor, Notice, Recorder, Recording, Tapped, TrackReport,
};
pub use source::{FakeSource, FakeVoice, OpenFn, Opened, Source};

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

/// Where the remote track's audio comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RemoteMode {
    /// Teams' process tree only (falls back to the whole system if Teams is not found).
    #[default]
    TeamsOnly,
    /// Everything the default playback device plays.
    WholeSystem,
}

/// An audio input device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDevice {
    /// The endpoint id, stable across restarts; saved in settings.
    pub id: String,
    /// The name Windows shows.
    pub name: String,
}

/// The microphones Windows reports, default communications device first. Empty where
/// capture is not supported. Blocks briefly; fine before the window opens, otherwise call
/// it off the UI thread.
pub fn input_devices() -> Vec<InputDevice> {
    #[cfg(windows)]
    {
        // On a thread of its own: joining COM's multithreaded apartment on the UI thread
        // would clash with the OLE apartment GPUI sets up there.
        let listed = std::thread::spawn(windows::input_devices)
            .join()
            .unwrap_or_else(|_| Err(Error::Device("device listing panicked".into())));
        listed.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not list input devices");
            Vec::new()
        })
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

impl Backend {
    /// The real devices: the chosen (or default communications) microphone, and Teams'
    /// process tree or the whole system per `mode`. Fails where capture is unsupported.
    pub fn system(mode: RemoteMode, mic_device: Option<String>) -> Result<Self> {
        #[cfg(windows)]
        {
            Ok(windows::backend(mode, mic_device))
        }
        #[cfg(not(windows))]
        {
            let _ = (mode, mic_device);
            Err(Error::Unsupported)
        }
    }
}

/// Capture errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Audio capture is not implemented on this platform yet.
    #[error("audio capture needs Windows; Linux capture through PipeWire comes after v1")]
    Unsupported,
    /// A file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// Writing to an open track failed.
    #[error("writing audio: {0}")]
    Write(#[from] std::io::Error),
    /// The Opus codec failed.
    #[error("opus: {0}")]
    Opus(#[from] opus::Error),
    /// The Ogg container could not be read.
    #[error("ogg: {0}")]
    Ogg(#[from] ogg::OggReadError),
    /// A file was not what it should be.
    #[error("{path}: {what}")]
    Format {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        what: String,
    },
    /// The audio device failed or went away.
    #[error("{0}")]
    Device(String),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_have_distinct_files() {
        assert_ne!(Track::Mic.file_name(), Track::Remote.file_name());
        assert_eq!(Track::Remote.meter_label(), "Teams");
    }

    #[cfg(not(windows))]
    #[test]
    fn system_capture_is_windows_only() {
        assert!(matches!(
            Backend::system(RemoteMode::TeamsOnly, None),
            Err(Error::Unsupported)
        ));
        assert!(input_devices().is_empty());
    }
}
