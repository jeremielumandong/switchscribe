//! Integrations (M5): the meeting's calendar event (Microsoft Graph, or a published ICS
//! calendar as the fallback), Jira issues from action items, and exports; and (M6) update
//! checks against GitHub Releases.
//!
//! Network calls are blocking (`ureq`); the app runs them on worker threads, never on
//! the UI thread. Request bodies and URLs are built by pure functions so tests cover
//! them without a network.

pub mod calendar;
pub mod export;
mod http;
pub mod jira;
pub mod update;

use serde::{Deserialize, Serialize};

pub use calendar::{
    Attendee, CalendarEvent, DeviceCode, GraphAuth, Poll, Tokens, calendar_view, event_at,
    fetch_ics, parse_ics, poll_device_code, refresh, start_device_code,
};
pub use export::{MeetingExport, export, file_name};
pub use jira::{CreatedIssue, JiraClient, JiraConfig, NewIssue, parse_due};

/// Errors from the integrations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The service could not be reached (offline, DNS, TLS, proxy, timeout).
    #[error("could not reach {service}: {detail}")]
    Network {
        /// "Jira", "Microsoft", "the calendar URL".
        service: &'static str,
        /// What the transport said.
        detail: String,
    },
    /// Credentials were refused or have expired: the user needs to sign in again (or,
    /// for Graph, the caller should refresh the access token first).
    #[error("{0}")]
    Auth(String),
    /// The service refused the request; the message is written for the user.
    #[error("{0}")]
    Rejected(String),
    /// The service answered with something this build does not understand.
    #[error("unexpected answer from {service}: {detail}")]
    BadResponse {
        /// The service.
        service: &'static str,
        /// What was wrong with it.
        detail: String,
    },
    /// An export could not be written.
    #[error("export: {0}")]
    Export(String),
    /// An update could not be downloaded or verified; the message is written for the user.
    #[error("update: {0}")]
    Update(String),
}

/// Integrations result.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Export formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExportFormat {
    /// Markdown (summary, decisions, action items, transcript).
    Markdown,
    /// Word document.
    Word,
    /// SubRip captions.
    Srt,
    /// WebVTT captions.
    Vtt,
    /// The meeting as JSON.
    Json,
}

impl ExportFormat {
    /// Every format, in the Export menu's order.
    pub const ALL: [ExportFormat; 5] = [
        ExportFormat::Markdown,
        ExportFormat::Word,
        ExportFormat::Srt,
        ExportFormat::Vtt,
        ExportFormat::Json,
    ];

    /// Menu label.
    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Markdown => "Markdown",
            ExportFormat::Word => "Word",
            ExportFormat::Srt => "SRT captions",
            ExportFormat::Vtt => "VTT captions",
            ExportFormat::Json => "JSON",
        }
    }

    /// File extension.
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Markdown => "md",
            ExportFormat::Word => "docx",
            ExportFormat::Srt => "srt",
            ExportFormat::Vtt => "vtt",
            ExportFormat::Json => "json",
        }
    }
}

/// A transcript timestamp as SRT writes it (`HH:MM:SS,mmm`).
pub fn srt_timestamp(ms: i64) -> String {
    let ms = ms.max(0);
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// A moment in a meeting as the app shows it: `mm:ss`, or `h:mm:ss` past an hour.
pub fn clock(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    let (h, m, x) = (s / 3600, s % 3600 / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{x:02}")
    } else {
        format!("{m:02}:{x:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_times() {
        assert_eq!(srt_timestamp(0), "00:00:00,000");
        assert_eq!(srt_timestamp(3_723_045), "01:02:03,045");
        assert_eq!(srt_timestamp(-5), "00:00:00,000");
    }

    #[test]
    fn clocks() {
        assert_eq!(clock(14_000), "00:14");
        assert_eq!(clock(252_900), "04:12");
        assert_eq!(clock(4_844_000), "1:20:44");
    }

    #[test]
    fn extensions_are_unique() {
        let mut exts: Vec<&str> = ExportFormat::ALL.iter().map(|f| f.extension()).collect();
        exts.sort_unstable();
        exts.dedup();
        assert_eq!(exts.len(), ExportFormat::ALL.len());
    }
}
