//! Rows the store hands out, and how the sidebar groups meetings by date.

use chrono::{Datelike as _, Local, NaiveDate};

/// Where a meeting is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MeetingStatus {
    /// Audio is being captured.
    Recording,
    /// Transcription, speaker naming or the summary is running.
    Processing,
    /// Done.
    Ready,
    /// Done, but some speakers have no name yet.
    NeedsLabels,
}

impl MeetingStatus {
    /// The value stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            MeetingStatus::Recording => "recording",
            MeetingStatus::Processing => "processing",
            MeetingStatus::Ready => "ready",
            MeetingStatus::NeedsLabels => "needs-labels",
        }
    }

    /// Parse a stored value; unknown values read as [`MeetingStatus::Ready`].
    pub fn parse(s: &str) -> MeetingStatus {
        match s {
            "recording" => MeetingStatus::Recording,
            "processing" => MeetingStatus::Processing,
            "needs-labels" => MeetingStatus::NeedsLabels,
            _ => MeetingStatus::Ready,
        }
    }
}

/// Someone heard in meetings. Speaker colors are indices into the app's 8-color palette
/// and stay with the person across meetings; `None` is an unnamed speaker (neutral gray).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    /// Row id.
    pub id: i64,
    /// Display name.
    pub name: String,
    /// Speaker color index, 0..8.
    pub color: Option<u8>,
    /// The user (the mic track).
    pub is_me: bool,
}

impl Person {
    /// Up to two initials: "Priya Nair" → "PN", "Jeremie" → "JE".
    pub fn initials(&self) -> String {
        let words: Vec<&str> = self.name.split_whitespace().collect();
        let s: String = match words.as_slice() {
            [] => String::new(),
            [one] => one.chars().take(2).collect(),
            [first, .., last] => first.chars().take(1).chain(last.chars().take(1)).collect(),
        };
        s.to_uppercase()
    }
}

/// A meeting in the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meeting {
    /// Stable id, also the meeting's folder name.
    pub id: String,
    /// Title (from the calendar, or typed).
    pub title: String,
    /// Start, seconds since the Unix epoch.
    pub started_at: i64,
    /// Length in seconds.
    pub duration_secs: u32,
    /// The app the call ran in ("Teams").
    pub source_app: String,
    /// Status.
    pub status: MeetingStatus,
    /// Attendees in display order.
    pub attendees: Vec<Person>,
}

impl Meeting {
    /// The local calendar day the meeting started on.
    pub fn local_date(&self) -> NaiveDate {
        chrono::DateTime::from_timestamp(self.started_at, 0)
            .unwrap_or_default()
            .with_timezone(&Local)
            .date_naive()
    }
}

/// A meeting to add.
#[derive(Clone, Debug)]
pub struct NewMeeting {
    /// See [`Meeting::id`].
    pub id: String,
    /// See [`Meeting::title`].
    pub title: String,
    /// See [`Meeting::started_at`].
    pub started_at: i64,
    /// See [`Meeting::duration_secs`].
    pub duration_secs: u32,
    /// See [`Meeting::source_app`].
    pub source_app: String,
    /// See [`Meeting::status`].
    pub status: MeetingStatus,
    /// Person ids, in display order.
    pub attendees: Vec<i64>,
}

/// One stretch of speech in a transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// Row id.
    pub id: i64,
    /// Start, milliseconds from the start of the meeting.
    pub start_ms: i64,
    /// End, milliseconds from the start of the meeting.
    pub end_ms: i64,
    /// The named speaker, if any.
    pub person_id: Option<i64>,
    /// The speaker's label when unnamed ("Speaker 3").
    pub speaker_label: String,
    /// What was said.
    pub text: String,
    /// The speaker match is uncertain.
    pub low_confidence: bool,
}

/// A segment to add.
#[derive(Clone, Debug, PartialEq)]
pub struct NewSegment {
    /// See [`Segment::start_ms`].
    pub start_ms: i64,
    /// See [`Segment::end_ms`].
    pub end_ms: i64,
    /// See [`Segment::person_id`].
    pub person_id: Option<i64>,
    /// See [`Segment::speaker_label`].
    pub speaker_label: String,
    /// See [`Segment::text`].
    pub text: String,
    /// See [`Segment::low_confidence`].
    pub low_confidence: bool,
}

/// A full-text search match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    /// The meeting.
    pub meeting_id: String,
    /// Its title.
    pub meeting_title: String,
    /// The segment's start in the meeting, milliseconds.
    pub start_ms: i64,
    /// The named speaker, if any.
    pub person_id: Option<i64>,
    /// The speaker's label.
    pub speaker_label: String,
    /// The segment text around the match, matched terms wrapped in
    /// [`crate::db::MATCH_START`] / [`crate::db::MATCH_END`].
    pub snippet: String,
}

/// The sidebar's date groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DateGroup {
    /// Started today.
    Today,
    /// Started yesterday.
    Yesterday,
    /// Earlier in the current week (weeks start on Monday).
    ThisWeek,
    /// Anything before that (or in the future, from a clock change).
    Older,
}

impl DateGroup {
    /// Every group, in sidebar order.
    pub const ALL: [DateGroup; 4] = [
        DateGroup::Today,
        DateGroup::Yesterday,
        DateGroup::ThisWeek,
        DateGroup::Older,
    ];

    /// Heading.
    pub fn label(self) -> &'static str {
        match self {
            DateGroup::Today => "Today",
            DateGroup::Yesterday => "Yesterday",
            DateGroup::ThisWeek => "This week",
            DateGroup::Older => "Older",
        }
    }

    /// The group for a meeting on `day`, seen on `today`.
    pub fn of(day: NaiveDate, today: NaiveDate) -> DateGroup {
        if day == today {
            return DateGroup::Today;
        }
        if today.pred_opt() == Some(day) {
            return DateGroup::Yesterday;
        }
        let monday = today - chrono::Days::new(u64::from(today.weekday().num_days_from_monday()));
        if day >= monday && day < today {
            DateGroup::ThisWeek
        } else {
            DateGroup::Older
        }
    }
}

/// Meetings (newest first) split into non-empty date groups, in sidebar order.
pub fn group_by_date(meetings: &[Meeting], today: NaiveDate) -> Vec<(DateGroup, Vec<&Meeting>)> {
    DateGroup::ALL
        .into_iter()
        .filter_map(|g| {
            let items: Vec<&Meeting> = meetings
                .iter()
                .filter(|m| DateGroup::of(m.local_date(), today) == g)
                .collect();
            (!items.is_empty()).then_some((g, items))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn date_groups() {
        let thu = d(2026, 10, 15);
        assert_eq!(DateGroup::of(thu, thu), DateGroup::Today);
        assert_eq!(DateGroup::of(d(2026, 10, 14), thu), DateGroup::Yesterday);
        assert_eq!(DateGroup::of(d(2026, 10, 12), thu), DateGroup::ThisWeek);
        assert_eq!(DateGroup::of(d(2026, 10, 11), thu), DateGroup::Older);
        assert_eq!(DateGroup::of(d(2026, 10, 2), thu), DateGroup::Older);
        // On a Monday, Sunday is yesterday and nothing else is "this week".
        let mon = d(2026, 10, 12);
        assert_eq!(DateGroup::of(d(2026, 10, 11), mon), DateGroup::Yesterday);
        assert_eq!(DateGroup::of(d(2026, 10, 10), mon), DateGroup::Older);
        // A future date (clock change) is never "today".
        assert_eq!(DateGroup::of(d(2026, 10, 16), thu), DateGroup::Older);
    }

    #[test]
    fn initials() {
        let p = |name: &str| Person {
            id: 1,
            name: name.into(),
            color: None,
            is_me: false,
        };
        assert_eq!(p("Priya Nair").initials(), "PN");
        assert_eq!(p("Jean Luc Picard").initials(), "JP");
        assert_eq!(p("jeremie").initials(), "JE");
        assert_eq!(p("  ").initials(), "");
    }

    #[test]
    fn status_round_trips() {
        for s in [
            MeetingStatus::Recording,
            MeetingStatus::Processing,
            MeetingStatus::Ready,
            MeetingStatus::NeedsLabels,
        ] {
            assert_eq!(MeetingStatus::parse(s.as_str()), s);
        }
        assert_eq!(MeetingStatus::parse("??"), MeetingStatus::Ready);
    }
}
