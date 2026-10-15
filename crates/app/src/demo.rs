//! Demo data (`switchscribe --demo` or `SWITCHSCRIBE_DEMO=1`): the prototype's meetings,
//! people and transcript in an in-memory store, plus the content later milestones will
//! produce for real (summaries, action items, notes, Ask answers). Nothing here touches
//! the user's data folder.

use chrono::{Days, Local, NaiveDate, NaiveTime, TimeZone as _};
use switchscribe_integrations::calendar;
use switchscribe_pipeline::identify::{Cluster, SpeakersFile};
use switchscribe_store::{
    Bullet, MeetingStatus, NewActionItem, NewMeeting, NewSegment, Store, StoredVoiceprint, Summary,
};

/// Whether this run shows the demo data.
pub fn requested(args: &[String]) -> bool {
    args.iter().any(|a| a == "--demo")
        || std::env::var("SWITCHSCRIBE_DEMO").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// The demo meeting with a transcript, summary and notes.
pub const FEATURED: &str = "aks";

/// Name, color index; the first is the user.
pub const PEOPLE: [(&str, u8); 5] = [
    ("Jeremie", 0),
    ("Priya Nair", 1),
    ("Marcus Chen", 2),
    ("Dana Ortiz", 3),
    ("Sam Lee", 4),
];

/// The featured meeting's transcript: seconds, speaker (index into [`PEOPLE`], `None` for
/// the unnamed Speaker 3), text, low confidence.
const SEGMENTS: [(i64, Option<usize>, &str, bool); 15] = [
    (
        14,
        Some(0),
        "Okay, let's start. Main thing today is the AKS migration cutover. Priya, where are we on the node pools?",
        false,
    ),
    (
        31,
        Some(1),
        "Two of three pools are on the new cluster. The GPU pool is the holdout, we're waiting on quota in West Europe.",
        false,
    ),
    (
        74,
        Some(2),
        "I raised the quota ticket Monday. Microsoft said five business days, so realistically next Wednesday.",
        false,
    ),
    (
        118,
        None,
        "Can we cut over without the GPU pool and leave transcoding on the old cluster for a week?",
        true,
    ),
    (
        171,
        Some(1),
        "Technically yes. The ingress split is easy, it's the Cerberus auth tokens that worry me.",
        false,
    ),
    (
        236,
        Some(0),
        "Then let's decide that now: we cut over the two pools on the 22nd and keep GPU workloads on the old cluster.",
        false,
    ),
    (
        302,
        Some(2),
        "Fine by me. I'll need a rollback runbook before I sign off though.",
        false,
    ),
    (
        366,
        Some(0),
        "Agreed. I'll write the runbook by Friday and share it in the platform channel.",
        false,
    ),
    (
        512,
        None,
        "On EZBook, the booking service still pins the old Redis endpoint. That breaks on cutover.",
        false,
    ),
    (
        640,
        Some(1),
        "I can move EZBook to the config-driven endpoint. Half a day, I'll have it by Tuesday.",
        false,
    ),
    (
        905,
        Some(2),
        "Open question: do we tell the vendor before or after the cutover? Their webhook IPs change.",
        true,
    ),
    (
        1260,
        None,
        "Before. Their allow-list change takes them 48 hours.",
        false,
    ),
    (
        1810,
        Some(0),
        "Okay, Marcus, can you own the vendor email? Let's aim for Monday.",
        false,
    ),
    (
        2380,
        Some(1),
        "Last thing, monitoring. I want Datadog dashboards on the new cluster before anyone touches DNS.",
        false,
    ),
    (
        2640,
        Some(0),
        "Good. Let's wrap. I'll send the summary and the Jira tickets after this.",
        false,
    ),
];

/// Meetings: id, title, days before today, start time, length in seconds, status,
/// attendees (indices into [`PEOPLE`]).
type DemoMeeting = (
    &'static str,
    &'static str,
    u64,
    (u32, u32),
    u32,
    MeetingStatus,
    &'static [usize],
);

const MEETINGS: [DemoMeeting; 7] = [
    (
        "aks",
        "AKS migration planning",
        0,
        (10, 0),
        2832,
        MeetingStatus::NeedsLabels,
        &[0, 1, 2],
    ),
    (
        "standup",
        "Platform standup",
        0,
        (9, 15),
        843,
        MeetingStatus::Ready,
        &[0, 1, 2],
    ),
    (
        "vendor",
        "Vendor call — Datadog",
        1,
        (15, 30),
        3160,
        MeetingStatus::Processing,
        &[0, 4],
    ),
    (
        "1on1",
        "1:1 Priya",
        1,
        (11, 0),
        1699,
        MeetingStatus::Ready,
        &[0, 1],
    ),
    (
        "cerb",
        "Cerberus incident review",
        2,
        (14, 0),
        2331,
        MeetingStatus::Ready,
        &[0, 2, 3, 4],
    ),
    (
        "ezb",
        "EZBook roadmap sync",
        3,
        (13, 0),
        2702,
        MeetingStatus::Ready,
        &[0, 3],
    ),
    (
        "q4",
        "Q4 planning",
        13,
        (10, 0),
        4844,
        MeetingStatus::Ready,
        &[0, 1, 2, 3],
    ),
];

fn at(today: NaiveDate, days_ago: u64, (h, m): (u32, u32)) -> i64 {
    let day = today.checked_sub_days(Days::new(days_ago)).unwrap_or(today);
    let time = NaiveTime::from_hms_opt(h, m, 0).unwrap_or_default();
    Local
        .from_local_datetime(&day.and_time(time))
        .earliest()
        .map_or(0, |t| t.timestamp())
}

/// An in-memory store holding the demo meetings, dated relative to `today`.
/// Lines the demo's live captions read back.
pub fn caption_lines() -> Vec<&'static str> {
    SEGMENTS.iter().map(|s| s.2).collect()
}

pub fn store(today: NaiveDate) -> switchscribe_store::Result<Store> {
    let mut store = Store::open_in_memory()?;
    let mut ids = Vec::new();
    for (i, (name, color)) in PEOPLE.iter().enumerate() {
        ids.push(store.add_person(name, Some(*color), i == 0)?);
    }
    for (id, title, days_ago, time, secs, status, att) in MEETINGS {
        store.insert_meeting(&NewMeeting {
            id: id.into(),
            title: title.into(),
            started_at: at(today, days_ago, time),
            duration_secs: secs,
            source_app: "Teams".into(),
            status,
            attendees: att.iter().map(|i| ids[*i]).collect(),
        })?;
    }
    let segments: Vec<NewSegment> = SEGMENTS
        .iter()
        .enumerate()
        .map(|(i, (s, who, text, low))| {
            let end = SEGMENTS.get(i + 1).map_or(2832, |n| n.0).min(s + 60);
            NewSegment {
                start_ms: s * 1000,
                end_ms: end * 1000,
                person_id: who.map(|w| ids[w]),
                speaker_label: if who.is_some() {
                    String::new()
                } else {
                    "Speaker 3".into()
                },
                text: (*text).into(),
                low_confidence: *low,
            }
        })
        .collect();
    store.add_segments(FEATURED, &segments)?;
    let (sum, items) = summary(&ids);
    store.set_summary(FEATURED, &sum, &items)?;
    // Its calendar invite: Lena is invited but not in the library, so naming Speaker 3
    // offers her.
    let invite = calendar::CalendarEvent {
        subject: "AKS migration planning".into(),
        start: at(today, 0, (10, 0)),
        end: at(today, 0, (10, 0)) + 3600,
        attendees: ["Jeremie", "Priya Nair", "Marcus Chen", "Lena Park"]
            .iter()
            .map(|n| calendar::Attendee {
                name: (*n).into(),
                email: None,
            })
            .collect(),
        organizer: Some("Jeremie".into()),
        online_meeting: true,
    };
    if let Ok(json) = serde_json::to_string(&invite) {
        store.set_meeting_calendar(FEATURED, Some(&json))?;
    }
    // A finished item from an earlier meeting, for the Action items screen.
    store.set_summary(
        "standup",
        &Summary {
            template: "standup".into(),
            cli: "Claude Code".into(),
            ..Summary::default()
        },
        &[NewActionItem {
            text: "Rotate the staging TLS certificates".into(),
            owner_id: Some(ids[2]),
            owner_name: Some(PEOPLE[2].0.into()),
            due: Some("Today".into()),
            at_ms: 120_000,
        }],
    )?;
    // Everyone but the unnamed speaker has a voiceprint: one axis each.
    for (i, id) in ids.iter().enumerate().skip(1) {
        store.set_voiceprint(&StoredVoiceprint {
            person_id: *id,
            embedding: axis(i),
            samples: [0, 12, 12, 6, 4][i],
            // Their first line in the featured meeting, if they speak in it.
            sample: SEGMENTS
                .iter()
                .find(|s| s.1 == Some(i))
                .map(|s| (FEATURED.into(), s.0 * 1000, s.0 * 1000 + 5_000)),
        })?;
    }
    // A change the agent proposed, waiting on the Ask panel's approve/deny card.
    store.add_pending(
        "update_summary",
        Some(FEATURED),
        r#"{"meeting_id":"aks","open_questions":[{"text":"Do the Cerberus auth tokens survive the ingress split?","at":"02:51"},{"text":"Who approves the rollback runbook?","at":"05:02"},{"text":"Who tells the vendor about the webhook IP change?","at":"21:00"}]}"#,
    )?;
    Ok(store)
}

fn axis(i: usize) -> Vec<f32> {
    let mut v = vec![0.0; PEOPLE.len() + 3];
    v[i] = 1.0;
    v
}

/// The featured meeting's `speakers.json`: Speaker 3 sounds most like Dana (82%), then
/// Sam (41%) and Priya (12%).
pub fn speakers() -> SpeakersFile {
    let mut embedding = vec![0.0; PEOPLE.len() + 3];
    for (i, w) in [(3, 0.82), (4, 0.41), (1, 0.12)] {
        embedding[i] = w;
    }
    // The rest of the unit vector, on an axis nobody has.
    embedding[PEOPLE.len() + 2] = (1.0f32 - 0.82 * 0.82 - 0.41 * 0.41 - 0.12 * 0.12).sqrt();
    let spans: Vec<(i64, i64)> = SEGMENTS
        .iter()
        .enumerate()
        .filter(|(_, s)| s.1.is_none())
        .map(|(i, s)| {
            let end = SEGMENTS.get(i + 1).map_or(2832, |n| n.0).min(s.0 + 60);
            (s.0 * 1000, end * 1000)
        })
        .collect();
    SpeakersFile {
        clusters: vec![Cluster {
            label: "Speaker 3".into(),
            embedding,
            speech_ms: spans.iter().map(|(a, b)| b - a).sum(),
            clips: spans
                .iter()
                .take(3)
                .map(|(a, b)| (*a, (*b).min(a + 6_000)))
                .collect(),
            person_id: None,
            similarity: None,
            confirmed: false,
        }],
        me: None,
    }
}

/// The featured meeting's summary and action items, as the CLI would have written them.
fn summary(ids: &[i64]) -> (Summary, Vec<NewActionItem>) {
    let b = |text: &str, at: i64| Bullet {
        text: text.into(),
        at_ms: at * 1000,
    };
    let item = |text: &str, owner: usize, due: Option<&str>, at: i64| NewActionItem {
        text: text.into(),
        owner_id: Some(ids[owner]),
        owner_name: Some(PEOPLE[owner].0.into()),
        due: due.map(Into::into),
        at_ms: at * 1000,
    };
    (
        Summary {
            template: "planning".into(),
            cli: "Claude Code".into(),
            generated_at: 0,
            summary: vec![
                b(
                    "Two of three AKS node pools are migrated; the GPU pool is blocked on West Europe quota, expected next Wednesday.",
                    31,
                ),
                b(
                    "EZBook still pins the old Redis endpoint and would break on cutover.",
                    512,
                ),
                b(
                    "Monitoring on the new cluster must be in place before DNS changes.",
                    2380,
                ),
            ],
            decisions: vec![
                b(
                    "Cut over the two CPU pools on Oct 22; GPU transcoding stays on the old cluster for a week.",
                    236,
                ),
                b(
                    "Notify the vendor before cutover, since their allow-list change takes 48 hours.",
                    1260,
                ),
            ],
            open_questions: vec![
                b(
                    "Do the Cerberus auth tokens survive the ingress split?",
                    171,
                ),
                b("Who approves the rollback runbook?", 302),
            ],
        },
        vec![
            item(
                "Write cutover rollback runbook and share in #platform",
                0,
                Some("Fri"),
                366,
            ),
            item(
                "Move EZBook to config-driven Redis endpoint",
                1,
                Some("Tue"),
                640,
            ),
            item("Email vendor about webhook IP change", 2, Some("Mon"), 1810),
            item(
                "Datadog dashboards live on new cluster before DNS",
                1,
                None,
                2380,
            ),
        ],
    )
}

/// The featured meeting's notes: moment, text.
pub fn notes() -> Vec<(i64, &'static str)> {
    vec![
        (118, "GPU pool → keep on old cluster?"),
        (366, "me: runbook by Fri"),
        (905, "vendor timing — ask Marcus"),
        (2380, "Datadog before DNS!"),
    ]
}

/// The Ask panel's opening exchange: (from the user, text, cited moments in
/// [`FEATURED`], seconds).
pub fn conversation() -> Vec<(bool, &'static str, Vec<i64>)> {
    vec![
        (true, "What did we decide about the AKS cutover?", vec![]),
        (
            false,
            "Cut over the two CPU node pools on Oct 22 and keep GPU transcoding on the old cluster until the West Europe quota lands. Marcus wants a rollback runbook before signing off.",
            vec![236, 302],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchscribe_store::{DateGroup, group_by_date};

    #[test]
    fn demo_store_fills_every_group() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        let store = store(today).unwrap();
        let meetings = store.meetings().unwrap();
        assert_eq!(meetings.len(), MEETINGS.len());
        let groups: Vec<DateGroup> = group_by_date(&meetings, today)
            .into_iter()
            .map(|g| g.0)
            .collect();
        assert_eq!(groups, DateGroup::ALL);
        assert_eq!(store.segments(FEATURED).unwrap().len(), SEGMENTS.len());
        assert!(!store.search("cutover", 10).unwrap().is_empty());
        assert_eq!(store.action_items(None).unwrap().len(), 5);
        assert!(store.summary(FEATURED).unwrap().is_some());
    }
}
