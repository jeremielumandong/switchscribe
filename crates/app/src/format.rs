//! Text for times and durations, as the prototype writes them.

use chrono::{Datelike as _, Local, NaiveDate, Timelike as _};
use switchscribe_store::{DateGroup, Meeting};

/// A playback position: `04:12`, or `1:20:44` past an hour.
pub fn clock(secs: i64) -> String {
    let s = secs.max(0);
    let (h, m, x) = (s / 3600, s % 3600 / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{x:02}")
    } else {
        format!("{m:02}:{x:02}")
    }
}

/// A meeting length for the sidebar: `47m`, `1h 20m`, `<1m`.
pub fn duration_short(secs: u32) -> String {
    let m = secs / 60;
    match (m / 60, m % 60) {
        (0, 0) => "<1m".to_owned(),
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

fn local(m: &Meeting) -> chrono::DateTime<Local> {
    chrono::DateTime::from_timestamp(m.started_at, 0)
        .unwrap_or_default()
        .with_timezone(&Local)
}

/// The sidebar row's time: `10:00` today and yesterday, the weekday this week, else the
/// date (`Oct 2`).
pub fn row_time(m: &Meeting, today: NaiveDate) -> String {
    let t = local(m);
    match DateGroup::of(t.date_naive(), today) {
        DateGroup::Today | DateGroup::Yesterday => format!("{:02}:{:02}", t.hour(), t.minute()),
        DateGroup::ThisWeek => weekday(t.weekday()).to_owned(),
        DateGroup::Older => format!("{} {}", month(t.month()), t.day()),
    }
}

/// The meeting header's date: `Thu, Oct 15, 2026 · 10:00`.
pub fn date_long(m: &Meeting) -> String {
    let t = local(m);
    format!(
        "{}, {} {}, {} · {:02}:{:02}",
        weekday(t.weekday()),
        month(t.month()),
        t.day(),
        t.year(),
        t.hour(),
        t.minute()
    )
}

/// A day for the People screen: `Today`, `Yesterday`, the weekday this week, else the
/// date (`Oct 2`).
pub fn day(secs: i64, today: NaiveDate) -> String {
    let t = chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_default()
        .with_timezone(&Local);
    match DateGroup::of(t.date_naive(), today) {
        DateGroup::Today => "Today".to_owned(),
        DateGroup::Yesterday => "Yesterday".to_owned(),
        DateGroup::ThisWeek => weekday(t.weekday()).to_owned(),
        DateGroup::Older => format!("{} {}", month(t.month()), t.day()),
    }
}

/// A clip length: `6s`, `1m 4s`.
pub fn seconds(ms: i64) -> String {
    let s = (ms.max(0) + 500) / 1000;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m {}s", s / 60, s % 60)
    }
}

fn weekday(d: chrono::Weekday) -> &'static str {
    ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][d.num_days_from_monday() as usize]
}

fn month(m: u32) -> &'static str {
    const M: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    M[(m.clamp(1, 12) - 1) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clocks() {
        assert_eq!(clock(0), "00:00");
        assert_eq!(clock(236), "03:56");
        assert_eq!(clock(2832), "47:12");
        assert_eq!(clock(4844), "1:20:44");
        assert_eq!(clock(-3), "00:00");
    }

    #[test]
    fn short_durations() {
        assert_eq!(duration_short(20), "<1m");
        assert_eq!(duration_short(2832), "47m");
        assert_eq!(duration_short(3600), "1h");
        assert_eq!(duration_short(4844), "1h 20m");
    }

    #[test]
    fn seconds_and_days() {
        assert_eq!(seconds(5_600), "6s");
        assert_eq!(seconds(252_000), "4m 12s");
        let today = Local::now().date_naive();
        assert_eq!(day(Local::now().timestamp(), today), "Today");
    }
}
