//! Where switchscribe keeps its files.
//!
//! ```text
//! <data>/                    %APPDATA%\switchscribe on Windows
//!   settings.json            Settings
//!   switchscribe.db          SQLite index (meetings, people, segments, FTS5)
//!   meetings/<id>/           one folder per meeting: the two Opus tracks,
//!                            transcript.json, summary.md, notes.md (from M1 on)
//!   logs/
//! ```

use std::path::PathBuf;

use crate::{Result, io_err};

/// The data folder and the files in it.
#[derive(Clone, Debug)]
pub struct Paths {
    /// The data folder.
    pub data: PathBuf,
}

impl Paths {
    /// `SWITCHSCRIBE_DATA` (tests, screenshots) wins; otherwise the platform's per-user
    /// data folder plus `switchscribe` (`%APPDATA%\switchscribe` on Windows,
    /// `~/.local/share/switchscribe` on Linux, `~/Library/Application Support/switchscribe`
    /// on macOS). `None` when the platform has no home directory.
    pub fn resolve() -> Option<Self> {
        if let Some(d) = std::env::var_os("SWITCHSCRIBE_DATA").filter(|d| !d.is_empty()) {
            return Some(Self { data: d.into() });
        }
        let base = directories::BaseDirs::new()?;
        Some(Self {
            data: base.data_dir().join("switchscribe"),
        })
    }

    /// A data folder at `data` (tests).
    pub fn at(data: impl Into<PathBuf>) -> Self {
        Self { data: data.into() }
    }

    /// The settings file.
    pub fn settings_file(&self) -> PathBuf {
        self.data.join("settings.json")
    }

    /// The SQLite index.
    pub fn db_file(&self) -> PathBuf {
        self.data.join("switchscribe.db")
    }

    /// The folder holding one folder per meeting.
    pub fn meetings_dir(&self) -> PathBuf {
        self.data.join("meetings")
    }

    /// One meeting's folder.
    pub fn meeting_dir(&self, id: &str) -> PathBuf {
        self.meetings_dir().join(id)
    }

    /// Downloaded speech models.
    pub fn models_dir(&self) -> PathBuf {
        self.data.join("models")
    }

    /// Log files.
    pub fn logs_dir(&self) -> PathBuf {
        self.data.join("logs")
    }

    /// Create the data folder and `meetings/` if they are missing.
    pub fn ensure(&self) -> Result<()> {
        let dir = self.meetings_dir();
        std::fs::create_dir_all(&dir).map_err(io_err(&dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_under_the_data_folder() {
        let p = Paths::at("/data");
        assert_eq!(p.settings_file(), PathBuf::from("/data/settings.json"));
        assert_eq!(p.db_file(), PathBuf::from("/data/switchscribe.db"));
        assert_eq!(p.meeting_dir("m1"), PathBuf::from("/data/meetings/m1"));
    }

    #[test]
    fn ensure_creates_the_meetings_folder() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::at(dir.path().join("nested"));
        p.ensure().unwrap();
        assert!(p.meetings_dir().is_dir());
    }
}
