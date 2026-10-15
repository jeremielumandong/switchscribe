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

    /// Delete the audio tracks (`*.opus`) of finished meetings that started more than
    /// `keep_days` days before `now` (Unix seconds). Transcripts, notes and summaries stay.
    /// Meetings still recording, processing or waiting for speaker names keep their audio
    /// (naming plays clips from it). Returns how many meetings lost their audio.
    pub fn prune_audio(&self, meetings: &[crate::Meeting], keep_days: u32, now: i64) -> usize {
        let cutoff = now - i64::from(keep_days) * 86_400;
        let mut pruned = 0;
        for m in meetings
            .iter()
            .filter(|m| m.status == crate::MeetingStatus::Ready && m.started_at < cutoff)
        {
            let Ok(entries) = std::fs::read_dir(self.meeting_dir(&m.id)) else {
                continue;
            };
            let mut any = false;
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().is_some_and(|x| x == "opus") {
                    match std::fs::remove_file(&path) {
                        Ok(()) => any = true,
                        Err(err) => {
                            tracing::warn!(path = %path.display(), error = %err, "could not delete old audio");
                        }
                    }
                }
            }
            pruned += usize::from(any);
        }
        pruned
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
    fn old_finished_audio_is_pruned() {
        use crate::{Meeting, MeetingStatus};
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::at(dir.path());
        let now = 100 * 86_400;
        let meeting = |id: &str, days_ago: i64, status| Meeting {
            id: id.into(),
            title: id.into(),
            started_at: now - days_ago * 86_400,
            duration_secs: 60,
            source_app: "Teams".into(),
            status,
            attendees: Vec::new(),
        };
        let all = [
            meeting("old", 40, MeetingStatus::Ready),
            meeting("new", 5, MeetingStatus::Ready),
            meeting("unnamed", 40, MeetingStatus::NeedsLabels),
        ];
        for m in &all {
            let d = p.meeting_dir(&m.id);
            std::fs::create_dir_all(&d).unwrap();
            for f in ["mic.opus", "remote.opus", "transcript.json"] {
                std::fs::write(d.join(f), b"x").unwrap();
            }
        }
        assert_eq!(p.prune_audio(&all, 30, now), 1);
        let old = p.meeting_dir("old");
        assert!(!old.join("mic.opus").exists() && !old.join("remote.opus").exists());
        assert!(old.join("transcript.json").exists());
        assert!(p.meeting_dir("new").join("mic.opus").exists());
        assert!(p.meeting_dir("unnamed").join("mic.opus").exists());
        // Nothing left to prune.
        assert_eq!(p.prune_audio(&all, 30, now), 0);
    }

    #[test]
    fn ensure_creates_the_meetings_folder() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::at(dir.path().join("nested"));
        p.ensure().unwrap();
        assert!(p.meetings_dir().is_dir());
    }
}
