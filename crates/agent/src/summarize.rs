//! Summaries: the prompt the CLI gets after a call, and reading its answer back into a
//! summary whose every line points at a moment of the meeting.

use serde::Deserialize;
use switchscribe_store::{Bullet, Meeting, NewActionItem, Person, Segment, Summary};

use crate::{Error, Result, Template};

impl Template {
    /// Every template, in the order Settings lists them.
    pub const ALL: [Template; 5] = [
        Template::Standup,
        Template::OneOnOne,
        Template::Planning,
        Template::VendorCall,
        Template::General,
    ];

    /// What this kind of meeting's notes should focus on. Settings can replace it.
    pub fn guidance(self) -> &'static str {
        match self {
            Template::Standup => {
                "A standup: per person, what they did, what they will do next and what blocks \
                 them. Blockers are open questions; commitments are action items."
            }
            Template::OneOnOne => {
                "A one-on-one: topics discussed, feedback given either way, and commitments \
                 each person made. Keep personal matters out of the summary."
            }
            Template::Planning => {
                "A planning meeting: the plan agreed, scope and dates, risks raised, and who \
                 owns each piece of work."
            }
            Template::VendorCall => {
                "A call with a vendor: what they offered or changed, prices and dates they \
                 gave, and what each side agreed to do next."
            }
            Template::General => {
                "A general meeting: the main points, what was decided, who will do what, and \
                 what is still open."
            }
        }
    }
}

/// `mm:ss`, or `h:mm:ss` past an hour.
pub fn clock(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    let (h, m, x) = (s / 3600, s % 3600 / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{x:02}")
    } else {
        format!("{m:02}:{x:02}")
    }
}

/// Parse `mm:ss` / `h:mm:ss` / plain seconds into milliseconds.
pub fn parse_clock(s: &str) -> Option<i64> {
    let parts: Vec<i64> = s
        .trim()
        .split(':')
        .map(|p| p.trim().parse::<f64>().ok().map(|v| v as i64))
        .collect::<Option<_>>()?;
    let secs = match parts.as_slice() {
        [s] => *s,
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => return None,
    };
    (secs >= 0).then_some(secs * 1000)
}

/// A speaker's name for a transcript line.
pub fn speaker_name(s: &Segment, people: &[Person]) -> String {
    s.person_id
        .and_then(|id| people.iter().find(|p| p.id == id))
        .map_or_else(|| s.speaker_label.clone(), |p| p.name.clone())
}

/// The transcript as the CLI reads it: `[mm:ss] Name: text`, one line each.
pub fn transcript_text(segments: &[Segment], people: &[Person]) -> String {
    segments
        .iter()
        .map(|s| {
            format!(
                "[{}] {}: {}",
                clock(s.start_ms),
                speaker_name(s, people),
                s.text.trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The prompt for a meeting's summary. `guidance` is the template's (or the user's
/// replacement for it).
pub fn summary_prompt(
    meeting: &Meeting,
    segments: &[Segment],
    people: &[Person],
    guidance: &str,
) -> String {
    let attendees: Vec<&str> = meeting.attendees.iter().map(|a| a.name.as_str()).collect();
    let date = chrono::DateTime::from_timestamp(meeting.started_at, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    format!(
        "You are writing the notes for a recorded meeting. Do not use any tools; everything \
you need is below.

{guidance}

Meeting: \"{title}\" on {date}, {minutes} minutes. Attendees: {attendees}.
Transcript, one line per stretch of speech, as [time] Speaker: words:

{transcript}

Reply with only one JSON object and nothing else, in this shape:
{{\"summary\": [{{\"text\": \"...\", \"at\": \"mm:ss\"}}],
 \"decisions\": [{{\"text\": \"...\", \"at\": \"mm:ss\"}}],
 \"action_items\": [{{\"text\": \"...\", \"owner\": \"name or null\", \"due\": \"as said, or null\", \"at\": \"mm:ss\"}}],
 \"open_questions\": [{{\"text\": \"...\", \"at\": \"mm:ss\"}}]}}

Rules: \"at\" is the time of the transcript line the item comes from, copied exactly. \
Each item is one plain sentence. Use people's names as the transcript gives them; an \
unnamed \"Speaker N\" stays \"Speaker N\". Only include what was actually said. Write in \
the language of the transcript. Summary has at most six items; any list may be empty.",
        title = meeting.title,
        minutes = meeting.duration_secs.div_ceil(60),
        attendees = if attendees.is_empty() {
            "unknown".to_owned()
        } else {
            attendees.join(", ")
        },
        transcript = transcript_text(segments, people),
    )
}

#[derive(Deserialize)]
struct RawItem {
    text: String,
    #[serde(default)]
    at: serde_json::Value,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    due: Option<String>,
}

#[derive(Deserialize)]
struct RawSummary {
    #[serde(default)]
    summary: Vec<RawItem>,
    #[serde(default)]
    decisions: Vec<RawItem>,
    #[serde(default)]
    action_items: Vec<RawItem>,
    #[serde(default)]
    open_questions: Vec<RawItem>,
}

/// The JSON object in a CLI's answer: the outermost `{...}`, past any prose or code
/// fences around it.
pub fn json_object(answer: &str) -> Option<&str> {
    let start = answer.find('{')?;
    let end = answer.rfind('}')?;
    (end > start).then(|| &answer[start..=end])
}

fn at_ms(v: &serde_json::Value, duration_ms: i64) -> i64 {
    let ms = match v {
        serde_json::Value::String(s) => parse_clock(s),
        serde_json::Value::Number(n) => n.as_f64().map(|f| (f * 1000.0) as i64),
        _ => None,
    }
    .unwrap_or(0);
    if duration_ms > 0 {
        ms.clamp(0, duration_ms)
    } else {
        ms.max(0)
    }
}

/// Someone in the library by the name the summary used: exact, then by first name.
pub fn match_person(name: &str, people: &[Person]) -> Option<i64> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let first = |s: &str| s.split_whitespace().next().unwrap_or("").to_lowercase();
    people
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .or_else(|| {
            let wanted = first(name);
            let mut same = people.iter().filter(|p| first(&p.name) == wanted);
            // Only an unambiguous first name counts.
            match (same.next(), same.next()) {
                (Some(p), None) => Some(p),
                _ => None,
            }
        })
        .map(|p| p.id)
}

/// Read a CLI's answer into a summary and its action items.
pub fn parse_summary(
    answer: &str,
    duration_ms: i64,
    people: &[Person],
) -> Result<(Summary, Vec<NewActionItem>)> {
    let json = json_object(answer).ok_or_else(|| Error::Answer("no JSON object".into()))?;
    let raw: RawSummary = serde_json::from_str(json).map_err(|e| Error::Answer(e.to_string()))?;
    let bullets = |items: Vec<RawItem>| -> Vec<Bullet> {
        items
            .into_iter()
            .filter(|i| !i.text.trim().is_empty())
            .map(|i| Bullet {
                text: i.text.trim().to_owned(),
                at_ms: at_ms(&i.at, duration_ms),
            })
            .collect()
    };
    let nonempty = |s: Option<String>| {
        s.map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("null"))
    };
    let items = raw
        .action_items
        .into_iter()
        .filter(|i| !i.text.trim().is_empty())
        .map(|i| {
            let owner_name = nonempty(i.owner);
            NewActionItem {
                text: i.text.trim().to_owned(),
                owner_id: owner_name.as_deref().and_then(|n| match_person(n, people)),
                owner_name,
                due: nonempty(i.due),
                at_ms: at_ms(&i.at, duration_ms),
            }
        })
        .collect();
    Ok((
        Summary {
            template: String::new(),
            cli: String::new(),
            generated_at: chrono::Utc::now().timestamp(),
            summary: bullets(raw.summary),
            decisions: bullets(raw.decisions),
            open_questions: bullets(raw.open_questions),
        },
        items,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(id: i64, name: &str) -> Person {
        Person {
            id,
            name: name.into(),
            color: None,
            is_me: false,
        }
    }

    #[test]
    fn clocks_round_trip() {
        assert_eq!(clock(236_000), "03:56");
        assert_eq!(clock(4_844_000), "1:20:44");
        assert_eq!(parse_clock("03:56"), Some(236_000));
        assert_eq!(parse_clock("1:20:44"), Some(4_844_000));
        assert_eq!(parse_clock("42"), Some(42_000));
        assert_eq!(parse_clock("soon"), None);
    }

    #[test]
    fn names_match_people() {
        let people = [
            person(1, "Priya Nair"),
            person(2, "Marcus Chen"),
            person(3, "Marcus Lee"),
        ];
        assert_eq!(match_person("priya nair", &people), Some(1));
        assert_eq!(match_person("Priya", &people), Some(1));
        // Two Marcuses: a first name alone is not enough.
        assert_eq!(match_person("Marcus", &people), None);
        assert_eq!(match_person("Dana", &people), None);
    }

    #[test]
    fn answers_parse_with_moments() {
        let answer = r#"Here are the notes:
```json
{"summary": [{"text": "Two pools moved.", "at": "00:31"}],
 "decisions": [{"text": "Cut over Oct 22.", "at": "03:56"}],
 "action_items": [{"text": "Write the runbook", "owner": "Priya", "due": "Friday", "at": "6:06"},
                  {"text": "Email the vendor", "owner": null, "due": "null", "at": 99999}],
 "open_questions": []}
```"#;
        let people = [person(1, "Priya Nair")];
        let (s, items) = parse_summary(answer, 600_000, &people).unwrap();
        assert_eq!(s.summary[0].at_ms, 31_000);
        assert_eq!(s.decisions[0].text, "Cut over Oct 22.");
        assert_eq!(items[0].owner_id, Some(1));
        assert_eq!(items[0].due.as_deref(), Some("Friday"));
        assert_eq!(items[0].at_ms, 366_000);
        assert_eq!(items[1].owner_name, None);
        assert_eq!(items[1].due, None);
        // Past the end of the meeting: clamped.
        assert_eq!(items[1].at_ms, 600_000);
        assert!(parse_summary("I can't do that.", 1, &people).is_err());
    }

    #[test]
    fn prompts_carry_the_transcript() {
        let meeting = Meeting {
            id: "m".into(),
            title: "AKS planning".into(),
            started_at: 0,
            duration_secs: 125,
            source_app: "Teams".into(),
            status: switchscribe_store::MeetingStatus::Ready,
            attendees: vec![person(1, "Priya Nair")],
        };
        let segs = [Segment {
            id: 1,
            start_ms: 31_000,
            end_ms: 35_000,
            person_id: Some(1),
            speaker_label: String::new(),
            text: "Two pools are done.".into(),
            low_confidence: false,
        }];
        let p = summary_prompt(&meeting, &segs, &[person(1, "Priya Nair")], "Be brief.");
        assert!(p.contains("[00:31] Priya Nair: Two pools are done."));
        assert!(p.contains("3 minutes"));
        assert!(p.starts_with("You are writing"));
    }
}
