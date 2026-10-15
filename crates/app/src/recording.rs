//! The app's side of a recording: the session the main window and the mini recorder
//! share, the amber notices, and the meeting folder's notes file.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use gpui_kit::component::input::TextareaState;
use gpui_kit::{Entity, Subscription, Task};
use switchscribe_capture::{Monitor, Notice, Recorder, Recording, Track};

use crate::format;
use crate::workspace::Screen;

/// The notes file in a meeting folder.
pub const NOTES_FILE: &str = "notes.md";

/// A recording in progress.
pub struct Session {
    /// The meeting row it records into.
    pub meeting_id: String,
    /// Its title, as shown in the live view and the mini recorder.
    pub title: String,
    /// The meeting folder.
    pub dir: PathBuf,
    /// The running capture; taken when stopping.
    pub recorder: Option<Recorder>,
    /// Levels, time and notices.
    pub monitor: Monitor,
    /// The private notes editor.
    pub notes: Entity<TextareaState>,
    /// Bookmarked moments, milliseconds.
    pub bookmarks: Vec<u64>,
    /// Bumped on every notes edit, so only the latest debounced save writes.
    pub notes_generation: u64,
    /// A test recording (never stops on its own).
    pub test: bool,
    /// When the Teams track last had sound.
    pub quiet_since: std::time::Instant,
    /// Live captions, when they are on.
    pub captions: Option<crate::captions::Live>,
    pub(crate) _tick: Task<()>,
    pub(crate) _notes_changed: Subscription,
}

impl Session {
    /// Recorded time, seconds.
    pub fn elapsed_secs(&self) -> i64 {
        self.monitor.elapsed().as_secs() as i64
    }

    /// Whether recording is paused.
    pub fn paused(&self) -> bool {
        self.monitor.is_paused()
    }

    /// Opacity of the pulsing red dot: a 1.4 s breath, steady at half while paused.
    pub fn pulse(&self) -> f32 {
        if self.paused() {
            return 0.5;
        }
        let t = self.monitor.elapsed().as_secs_f32();
        0.7 + 0.3 * (t * std::f32::consts::TAU / 1.4).cos()
    }
}

/// An amber strip above the main pane.
#[derive(Clone, Debug, PartialEq)]
pub struct Banner {
    /// What happened.
    pub text: String,
    /// A link on the right and where it goes.
    pub action: Option<(&'static str, Screen)>,
}

impl Banner {
    /// A banner without an action.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            action: None,
        }
    }
}

/// How a capture notice reads in the window, as the prototype words them.
pub fn banner_for(notice: &Notice) -> Banner {
    let settings = Some(("Audio settings", Screen::Settings));
    match notice {
        Notice::Fallback {
            track: Track::Remote,
            reason,
        } => Banner {
            text: reason.clone(),
            action: settings,
        },
        Notice::Fallback {
            track: Track::Mic,
            reason,
        } => Banner {
            text: reason.clone(),
            action: Some(("Choose another mic", Screen::Settings)),
        },
        Notice::TrackLost {
            track: Track::Mic,
            at_ms,
            error,
        } => Banner {
            text: if *at_ms < 1000 {
                format!("Microphone unavailable ({error}). Recording continues on the Teams track.")
            } else {
                format!(
                    "Microphone disconnected at {}. Recording continues on the Teams track.",
                    format::clock((*at_ms / 1000) as i64)
                )
            },
            action: Some(("Choose another mic", Screen::Settings)),
        },
        Notice::TrackLost {
            track: Track::Remote,
            at_ms,
            error,
        } => Banner {
            text: if *at_ms < 1000 {
                format!("Could not capture Teams audio ({error}). Recording continues on your mic.")
            } else {
                format!(
                    "Teams audio stopped at {} ({error}). Recording continues on your mic.",
                    format::clock((*at_ms / 1000) as i64)
                )
            },
            action: settings,
        },
    }
}

/// A banner for a finished recording that holds no audio at all.
pub fn empty_recording(rec: &Recording) -> Option<Banner> {
    rec.tracks
        .iter()
        .all(|t| t.file.is_none() || t.duration_ms == 0)
        .then(|| Banner {
            text: "Nothing was recorded: neither the microphone nor Teams could be captured."
                .into(),
            action: Some(("Audio settings", Screen::Settings)),
        })
}

/// A meeting id from its start time: `20261015-140507`. Sorts by time and doubles as the
/// folder name.
pub fn meeting_id(now: DateTime<Local>) -> String {
    now.format("%Y%m%d-%H%M%S").to_string()
}

/// The title a recording starts with until a calendar names it (M5).
pub fn default_title(now: DateTime<Local>) -> String {
    format!("Teams call {}", now.format("%H:%M"))
}

/// `SWITCHSCRIBE_FAKE_AUDIO=1` records two synthetic voices instead of the devices, so
/// the recorder can be tried (and screenshotted) on any machine.
pub fn fake_audio_requested() -> bool {
    std::env::var("SWITCHSCRIBE_FAKE_AUDIO").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Write the notes file atomically.
pub fn save_notes(dir: &Path, text: &str) -> std::io::Result<()> {
    let path = dir.join(NOTES_FILE);
    let tmp = dir.join(format!("{NOTES_FILE}.tmp"));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

/// A finished meeting's notes and bookmarks, read from its folder.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NotesDoc {
    /// `notes.md`, or empty.
    pub text: String,
    /// Bookmarks from `capture.json`, seconds.
    pub bookmarks: Vec<i64>,
}

impl NotesDoc {
    /// Read a meeting folder (blocking; call off the UI thread).
    pub fn load(dir: &Path) -> Self {
        let text = std::fs::read_to_string(dir.join(NOTES_FILE)).unwrap_or_default();
        let bookmarks = Recording::load(dir)
            .map(|r| r.bookmarks.iter().map(|ms| (ms / 1000) as i64).collect())
            .unwrap_or_default();
        Self { text, bookmarks }
    }

    /// Nothing to show.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.bookmarks.is_empty()
    }
}

/// The line a quick note starts: `- [12:04] `.
pub fn anchor_line(secs: i64) -> String {
    format!("- [{}] ", format::clock(secs))
}

/// Split a notes line into its timestamp anchor (seconds) and the rest, if it has one:
/// `- [12:04] ask Marcus` or `[1:02:03] ...`.
pub fn parse_anchor(line: &str) -> Option<(i64, &str)> {
    let rest = line
        .trim_start()
        .trim_start_matches(['-', '*'])
        .trim_start();
    let inner = rest.strip_prefix('[')?;
    let (stamp, after) = inner.split_once(']')?;
    let mut secs = 0i64;
    let parts: Vec<&str> = stamp.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    for part in parts {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        secs = secs * 60 + part.parse::<i64>().ok()?;
    }
    Some((secs, after.trim_start()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use switchscribe_capture::TrackReport;

    #[test]
    fn ids_and_titles_come_from_the_start_time() {
        let t = Local.with_ymd_and_hms(2026, 10, 15, 14, 5, 7).unwrap();
        assert_eq!(meeting_id(t), "20261015-140507");
        assert_eq!(default_title(t), "Teams call 14:05");
    }

    #[test]
    fn anchors_parse() {
        assert_eq!(
            parse_anchor("- [12:04] ask Marcus"),
            Some((724, "ask Marcus"))
        );
        assert_eq!(parse_anchor("[1:02:03] x"), Some((3723, "x")));
        assert_eq!(parse_anchor("* [00:48]"), Some((48, "")));
        assert_eq!(parse_anchor("- Cerberus prod tomorrow"), None);
        assert_eq!(parse_anchor("- [todo] x"), None);
        assert_eq!(anchor_line(724), "- [12:04] ");
    }

    #[test]
    fn notices_read_like_the_prototype() {
        let b = banner_for(&Notice::TrackLost {
            track: Track::Mic,
            at_ms: 724_000,
            error: "gone".into(),
        });
        assert_eq!(
            b.text,
            "Microphone disconnected at 12:04. Recording continues on the Teams track."
        );
        assert_eq!(b.action.map(|a| a.0), Some("Choose another mic"));
        let b = banner_for(&Notice::Fallback {
            track: Track::Remote,
            reason: "Teams not detected. Capturing whole-system audio instead.".into(),
        });
        assert!(b.text.starts_with("Teams not detected"));
    }

    #[test]
    fn empty_recordings_are_flagged() {
        let track = |file: Option<&str>, ms| TrackReport {
            track: Track::Mic,
            file: file.map(Into::into),
            source: None,
            duration_ms: ms,
            error: None,
        };
        let mut rec = Recording {
            duration_ms: 5000,
            tracks: vec![track(None, 0), track(Some("remote.opus"), 0)],
            bookmarks: vec![],
        };
        assert!(empty_recording(&rec).is_some());
        rec.tracks[1].duration_ms = 5000;
        assert!(empty_recording(&rec).is_none());
    }

    #[test]
    fn notes_round_trip_through_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert!(NotesDoc::load(dir.path()).is_empty());
        save_notes(dir.path(), "- [00:48] thresholds\n").unwrap();
        let doc = NotesDoc::load(dir.path());
        assert_eq!(doc.text, "- [00:48] thresholds\n");
        assert!(!dir.path().join("notes.md.tmp").exists());
    }
}
