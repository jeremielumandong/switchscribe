//! Exports: a meeting as Markdown, Word (.docx), SRT / VTT captions or JSON.
//!
//! Every writer takes the same [`MeetingExport`] and returns the file's bytes; the caller
//! picks where to save them (see [`file_name`]). The .docx is a minimal OOXML package
//! written by hand (four parts in a zip), so no document library is needed.

use std::io::Write as _;

use chrono::{DateTime, Local, Utc};
use serde::Serialize;
use switchscribe_store::{ActionItem, ActionStatus, Bullet, Meeting, Person, Segment, Summary};

use crate::{Error, ExportFormat, Result, clock, srt_timestamp};

/// Everything an export needs about one meeting.
#[derive(Clone, Copy, Debug)]
pub struct MeetingExport<'a> {
    /// The meeting (title, start, length, attendees).
    pub meeting: &'a Meeting,
    /// Its transcript, in time order.
    pub segments: &'a [Segment],
    /// Everyone in the library, to name segments by `person_id`.
    pub people: &'a [Person],
    /// Its summary, if it has one.
    pub summary: Option<&'a Summary>,
    /// Its action items.
    pub action_items: &'a [ActionItem],
    /// The user's notes (plain text; may be empty).
    pub notes: &'a str,
}

impl MeetingExport<'_> {
    /// Who said `seg`: the person's name when the segment is named, else its label.
    pub fn speaker(&self, seg: &Segment) -> String {
        seg.person_id
            .and_then(|id| self.people.iter().find(|p| p.id == id))
            .map(|p| p.name.clone())
            .unwrap_or_else(|| seg.speaker_label.clone())
    }

    fn attendee_names(&self) -> Vec<String> {
        self.meeting
            .attendees
            .iter()
            .map(|p| p.name.clone())
            .collect()
    }

    /// "Thu 15 Oct 2026, 10:00 · 47m · Priya Nair, Jeremie".
    fn meta_parts(&self) -> (String, String, String) {
        let start = local_start(self.meeting);
        let date = start.format("%a %-d %b %Y, %H:%M").to_string();
        let mins = self.meeting.duration_secs / 60;
        let dur = match (mins / 60, mins % 60) {
            (0, 0) => "<1m".to_owned(),
            (0, m) => format!("{m}m"),
            (h, 0) => format!("{h}h"),
            (h, m) => format!("{h}h {m}m"),
        };
        (date, dur, self.attendee_names().join(", "))
    }
}

fn local_start(m: &Meeting) -> DateTime<Local> {
    DateTime::from_timestamp(m.started_at, 0)
        .unwrap_or_default()
        .with_timezone(&Local)
}

/// Write `m` in `format`.
pub fn export(m: &MeetingExport, format: ExportFormat) -> Result<Vec<u8>> {
    Ok(match format {
        ExportFormat::Markdown => markdown(m).into_bytes(),
        ExportFormat::Word => docx(m)?,
        ExportFormat::Srt => srt(m).into_bytes(),
        ExportFormat::Vtt => vtt(m).into_bytes(),
        ExportFormat::Json => {
            serde_json::to_vec_pretty(&json(m)).map_err(|e| Error::Export(e.to_string()))?
        }
    })
}

/// A file name for the export: `2026-10-15 Cerberus rollout sync.md`. Characters Windows
/// does not allow in file names are dropped, whitespace is collapsed, the title is capped
/// at 80 characters and a missing title reads "Meeting".
pub fn file_name(meeting: &Meeting, format: ExportFormat) -> String {
    let cleaned: String = meeting
        .title
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let mut title: String = cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect();
    // Windows drops trailing dots and spaces, which would change the name.
    while title.ends_with(['.', ' ']) {
        title.pop();
    }
    if title.is_empty() {
        title = "Meeting".to_owned();
    }
    format!(
        "{} {title}.{}",
        meeting.local_date().format("%Y-%m-%d"),
        format.extension()
    )
}

/// One section's lines: (stamp, text).
fn bullets(b: &[Bullet]) -> Vec<(String, String)> {
    b.iter().map(|b| (clock(b.at_ms), b.text.clone())).collect()
}

/// "Text — Priya, due Friday (OPS-7)".
fn action_line(a: &ActionItem) -> String {
    let mut s = a.text.clone();
    let mut extra = Vec::new();
    if let Some(o) = a.owner_name.as_deref().filter(|o| !o.trim().is_empty()) {
        extra.push(o.to_owned());
    }
    if let Some(d) = a.due.as_deref().filter(|d| !d.trim().is_empty()) {
        extra.push(format!("due {d}"));
    }
    if !extra.is_empty() {
        s.push_str(" — ");
        s.push_str(&extra.join(", "));
    }
    s
}

// ---------------------------------------------------------------------------------------
// Markdown

fn markdown(m: &MeetingExport) -> String {
    let mut out = String::new();
    let title = if m.meeting.title.trim().is_empty() {
        "Meeting"
    } else {
        m.meeting.title.as_str()
    };
    out.push_str(&format!("# {title}\n\n"));
    let (date, dur, people) = m.meta_parts();
    out.push_str(&format!("**Date:** {date} · **Duration:** {dur}"));
    if !people.is_empty() {
        out.push_str(&format!(" · **Attendees:** {people}"));
    }
    out.push_str("\n\n");

    let section = |out: &mut String, heading: &str, lines: Vec<(String, String)>| {
        if lines.is_empty() {
            return;
        }
        out.push_str(&format!("## {heading}\n\n"));
        for (at, text) in lines {
            out.push_str(&format!("- [{at}] {}\n", one_line(&text)));
        }
        out.push('\n');
    };
    if let Some(s) = m.summary {
        section(&mut out, "Summary", bullets(&s.summary));
        section(&mut out, "Decisions", bullets(&s.decisions));
    }
    if !m.action_items.is_empty() {
        out.push_str("## Action items\n\n");
        for a in m.action_items {
            let check = if a.status == ActionStatus::Done {
                "x"
            } else {
                " "
            };
            out.push_str(&format!(
                "- [{check}] [{}] {}",
                clock(a.at_ms),
                one_line(&action_line(a))
            ));
            if let (Some(key), Some(url)) = (&a.jira_key, &a.jira_url) {
                out.push_str(&format!(" ([{key}]({url}))"));
            }
            out.push('\n');
        }
        out.push('\n');
    }
    if let Some(s) = m.summary {
        section(&mut out, "Open questions", bullets(&s.open_questions));
    }
    if !m.notes.trim().is_empty() {
        out.push_str("## Notes\n\n");
        out.push_str(m.notes.trim_end());
        out.push_str("\n\n");
    }
    out.push_str("## Transcript\n\n");
    if m.segments.is_empty() {
        out.push_str("_No transcript._\n");
    }
    for seg in m.segments {
        out.push_str(&format!(
            "**[{}] {}:** {}\n\n",
            clock(seg.start_ms),
            m.speaker(seg),
            one_line(&seg.text)
        ));
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// Text on one line (captions and list items cannot hold line breaks).
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------------------
// Captions

fn srt(m: &MeetingExport) -> String {
    let mut out = String::new();
    for (i, seg) in m.segments.iter().enumerate() {
        let end = seg.end_ms.max(seg.start_ms);
        out.push_str(&format!(
            "{}\n{} --> {}\n{}: {}\n\n",
            i + 1,
            srt_timestamp(seg.start_ms),
            srt_timestamp(end),
            one_line(&m.speaker(seg)),
            one_line(&seg.text)
        ));
    }
    out
}

/// `HH:MM:SS.mmm`, as WebVTT writes it.
fn vtt_timestamp(ms: i64) -> String {
    srt_timestamp(ms).replace(',', ".")
}

/// Escape text for a VTT cue (`&`, `<`, `>`; the last also keeps `-->` out of cues).
fn vtt_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn vtt(m: &MeetingExport) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for seg in m.segments {
        let end = seg.end_ms.max(seg.start_ms);
        out.push_str(&format!(
            "{} --> {}\n<v {}>{}\n\n",
            vtt_timestamp(seg.start_ms),
            vtt_timestamp(end),
            vtt_escape(&one_line(&m.speaker(seg))),
            vtt_escape(&one_line(&seg.text))
        ));
    }
    out
}

// ---------------------------------------------------------------------------------------
// JSON

/// The JSON export's shape (`"format": "switchscribe.meeting/1"`). Fields are only ever
/// added; times are milliseconds from the meeting start (`*_ms`) plus the same moment as
/// `mm:ss` (`at`), and the start is both Unix seconds and RFC 3339 UTC.
#[derive(Serialize)]
struct JsonMeeting<'a> {
    format: &'static str,
    id: &'a str,
    title: &'a str,
    started_at: i64,
    started_at_utc: String,
    duration_secs: u32,
    source_app: &'a str,
    attendees: Vec<String>,
    summary: Option<JsonSummary<'a>>,
    action_items: Vec<JsonAction<'a>>,
    notes: &'a str,
    transcript: Vec<JsonSegment<'a>>,
}

#[derive(Serialize)]
struct JsonSummary<'a> {
    template: &'a str,
    cli: &'a str,
    generated_at: i64,
    summary: Vec<JsonBullet<'a>>,
    decisions: Vec<JsonBullet<'a>>,
    open_questions: Vec<JsonBullet<'a>>,
}

#[derive(Serialize)]
struct JsonBullet<'a> {
    text: &'a str,
    at_ms: i64,
    at: String,
}

#[derive(Serialize)]
struct JsonAction<'a> {
    text: &'a str,
    owner: Option<&'a str>,
    due: Option<&'a str>,
    at_ms: i64,
    at: String,
    status: ActionStatus,
    jira_key: Option<&'a str>,
    jira_url: Option<&'a str>,
}

#[derive(Serialize)]
struct JsonSegment<'a> {
    start_ms: i64,
    end_ms: i64,
    at: String,
    speaker: String,
    person_id: Option<i64>,
    text: &'a str,
}

fn json<'a>(m: &MeetingExport<'a>) -> JsonMeeting<'a> {
    let jb = |b: &'a [Bullet]| -> Vec<JsonBullet<'a>> {
        b.iter()
            .map(|b| JsonBullet {
                text: &b.text,
                at_ms: b.at_ms,
                at: clock(b.at_ms),
            })
            .collect()
    };
    JsonMeeting {
        format: "switchscribe.meeting/1",
        id: &m.meeting.id,
        title: &m.meeting.title,
        started_at: m.meeting.started_at,
        started_at_utc: DateTime::<Utc>::from_timestamp(m.meeting.started_at, 0)
            .unwrap_or_default()
            .to_rfc3339(),
        duration_secs: m.meeting.duration_secs,
        source_app: &m.meeting.source_app,
        attendees: m.attendee_names(),
        summary: m.summary.map(|s| JsonSummary {
            template: &s.template,
            cli: &s.cli,
            generated_at: s.generated_at,
            summary: jb(&s.summary),
            decisions: jb(&s.decisions),
            open_questions: jb(&s.open_questions),
        }),
        action_items: m
            .action_items
            .iter()
            .map(|a| JsonAction {
                text: &a.text,
                owner: a.owner_name.as_deref(),
                due: a.due.as_deref(),
                at_ms: a.at_ms,
                at: clock(a.at_ms),
                status: a.status,
                jira_key: a.jira_key.as_deref(),
                jira_url: a.jira_url.as_deref(),
            })
            .collect(),
        notes: m.notes,
        transcript: m
            .segments
            .iter()
            .map(|s| JsonSegment {
                start_ms: s.start_ms,
                end_ms: s.end_ms,
                at: clock(s.start_ms),
                speaker: m.speaker(s),
                person_id: s.person_id,
                text: &s.text,
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------------------
// Word

/// Escape text for XML, dropping characters XML 1.0 cannot hold.
pub(crate) fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// A run of text; bold when asked.
fn run(text: &str, bold: bool) -> String {
    let rpr = if bold { "<w:rPr><w:b/></w:rPr>" } else { "" };
    format!(
        "<w:r>{rpr}<w:t xml:space=\"preserve\">{}</w:t></w:r>",
        xml_escape(text)
    )
}

/// A paragraph of runs, in a style (or the default).
fn para(style: Option<&str>, runs: &[String]) -> String {
    let ppr = style
        .map(|s| format!("<w:pPr><w:pStyle w:val=\"{s}\"/></w:pPr>"))
        .unwrap_or_default();
    format!("<w:p>{ppr}{}</w:p>", runs.concat())
}

fn document_xml(m: &MeetingExport) -> String {
    let mut body = Vec::new();
    let title = if m.meeting.title.trim().is_empty() {
        "Meeting"
    } else {
        m.meeting.title.as_str()
    };
    body.push(para(Some("Title"), &[run(title, false)]));
    let (date, dur, people) = m.meta_parts();
    let mut meta = vec![
        run("Date: ", true),
        run(&date, false),
        run("   Duration: ", true),
        run(&dur, false),
    ];
    if !people.is_empty() {
        meta.push(run("   Attendees: ", true));
        meta.push(run(&people, false));
    }
    body.push(para(None, &meta));

    let section = |body: &mut Vec<String>, heading: &str, lines: Vec<(String, String)>| {
        if lines.is_empty() {
            return;
        }
        body.push(para(Some("Heading1"), &[run(heading, false)]));
        for (at, text) in lines {
            body.push(para(
                Some("ListParagraph"),
                &[run(&format!("[{at}] "), true), run(&one_line(&text), false)],
            ));
        }
    };
    if let Some(s) = m.summary {
        section(&mut body, "Summary", bullets(&s.summary));
        section(&mut body, "Decisions", bullets(&s.decisions));
    }
    if !m.action_items.is_empty() {
        body.push(para(Some("Heading1"), &[run("Action items", false)]));
        for a in m.action_items {
            let mut runs = vec![
                run(&format!("[{}] ", clock(a.at_ms)), true),
                run(&one_line(&action_line(a)), false),
            ];
            if a.status == ActionStatus::Done {
                runs.push(run(" (done)", false));
            }
            if let Some(key) = &a.jira_key {
                runs.push(run(&format!(" ({key})"), false));
            }
            body.push(para(Some("ListParagraph"), &runs));
        }
    }
    if let Some(s) = m.summary {
        section(&mut body, "Open questions", bullets(&s.open_questions));
    }
    if !m.notes.trim().is_empty() {
        body.push(para(Some("Heading1"), &[run("Notes", false)]));
        for line in m.notes.trim_end().lines() {
            body.push(para(None, &[run(line, false)]));
        }
    }
    body.push(para(Some("Heading1"), &[run("Transcript", false)]));
    for seg in m.segments {
        body.push(para(
            None,
            &[
                run(
                    &format!("[{}] {}: ", clock(seg.start_ms), m.speaker(seg)),
                    true,
                ),
                run(&one_line(&seg.text), false),
            ],
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
         <w:body>{}<w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/>\
         <w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\" \
         w:header=\"708\" w:footer=\"708\" w:gutter=\"0\"/></w:sectPr></w:body></w:document>",
        body.concat()
    )
}

const CONTENT_TYPES: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
<Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/>\
</Types>";

const ROOT_RELS: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
</Relationships>";

const DOCUMENT_RELS: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>\
</Relationships>";

const STYLES: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii=\"Calibri\" w:hAnsi=\"Calibri\" w:cs=\"Calibri\"/>\
<w:sz w:val=\"22\"/></w:rPr></w:rPrDefault>\
<w:pPrDefault><w:pPr><w:spacing w:after=\"120\" w:line=\"264\" w:lineRule=\"auto\"/></w:pPr></w:pPrDefault></w:docDefaults>\
<w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/><w:qFormat/></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Title\"><w:name w:val=\"Title\"/><w:basedOn w:val=\"Normal\"/>\
<w:next w:val=\"Normal\"/><w:qFormat/><w:pPr><w:spacing w:after=\"200\"/></w:pPr>\
<w:rPr><w:b/><w:sz w:val=\"44\"/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Heading1\"><w:name w:val=\"heading 1\"/><w:basedOn w:val=\"Normal\"/>\
<w:next w:val=\"Normal\"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before=\"360\" w:after=\"120\"/>\
<w:outlineLvl w:val=\"0\"/></w:pPr><w:rPr><w:b/><w:sz w:val=\"30\"/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"ListParagraph\"><w:name w:val=\"List Paragraph\"/>\
<w:basedOn w:val=\"Normal\"/><w:qFormat/><w:pPr><w:ind w:left=\"360\"/></w:pPr></w:style>\
</w:styles>";

fn docx(m: &MeetingExport) -> Result<Vec<u8>> {
    let zerr = |e: zip::result::ZipError| Error::Export(format!("word: {e}"));
    let ioerr = |e: std::io::Error| Error::Export(format!("word: {e}"));
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let document = document_xml(m);
    let parts: [(&str, &str); 5] = [
        ("[Content_Types].xml", CONTENT_TYPES),
        ("_rels/.rels", ROOT_RELS),
        ("word/_rels/document.xml.rels", DOCUMENT_RELS),
        ("word/document.xml", &document),
        ("word/styles.xml", STYLES),
    ];
    for (name, content) in parts {
        zip.start_file(name, opts).map_err(zerr)?;
        zip.write_all(content.as_bytes()).map_err(ioerr)?;
    }
    Ok(zip.finish().map_err(zerr)?.into_inner())
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use switchscribe_store::MeetingStatus;

    use super::*;

    fn person(id: i64, name: &str) -> Person {
        Person {
            id,
            name: name.into(),
            color: None,
            is_me: id == 1,
        }
    }

    fn seg(start_s: i64, person_id: Option<i64>, label: &str, text: &str) -> Segment {
        Segment {
            id: start_s,
            start_ms: start_s * 1000,
            end_ms: start_s * 1000 + 3_500,
            person_id,
            speaker_label: label.into(),
            text: text.into(),
            low_confidence: false,
        }
    }

    struct Fixture {
        meeting: Meeting,
        segments: Vec<Segment>,
        people: Vec<Person>,
        summary: Summary,
        items: Vec<ActionItem>,
    }

    fn fixture() -> Fixture {
        let people = vec![person(1, "Jeremie"), person(2, "Priya Nair")];
        let meeting = Meeting {
            id: "m1".into(),
            title: "AKS cutover: R&D <sync>".into(),
            started_at: 1_792_051_200, // 2026-10-15T08:00:00Z
            duration_secs: 47 * 60 + 12,
            source_app: "Teams".into(),
            status: MeetingStatus::Ready,
            attendees: people.clone(),
        };
        let segments = vec![
            seg(14, Some(1), "", "Main thing today is the AKS cutover."),
            seg(
                252,
                None,
                "Speaker 3",
                "Can we cut over without the GPU pool & keep <prod> up?",
            ),
            seg(300, Some(2), "Speaker 2", "Yes --> on the 22nd."),
        ];
        let summary = Summary {
            template: "planning".into(),
            cli: "Claude Code".into(),
            generated_at: 1_792_054_000,
            summary: vec![Bullet {
                text: "Cutover moves to Oct 22.".into(),
                at_ms: 300_000,
            }],
            decisions: vec![Bullet {
                text: "Keep the GPU pool.".into(),
                at_ms: 252_000,
            }],
            open_questions: vec![Bullet {
                text: "Who tells support?".into(),
                at_ms: 14_000,
            }],
        };
        let items = vec![ActionItem {
            id: 1,
            meeting_id: "m1".into(),
            meeting_title: meeting.title.clone(),
            meeting_started_at: meeting.started_at,
            text: "Draft the rollback plan".into(),
            owner_id: Some(2),
            owner_name: Some("Priya Nair".into()),
            due: Some("Friday".into()),
            at_ms: 305_000,
            status: ActionStatus::InJira,
            jira_key: Some("OPS-7".into()),
            jira_url: Some("https://x.atlassian.net/browse/OPS-7".into()),
        }];
        Fixture {
            meeting,
            segments,
            people,
            summary,
            items,
        }
    }

    fn with<R>(f: &Fixture, notes: &str, run: impl FnOnce(&MeetingExport) -> R) -> R {
        run(&MeetingExport {
            meeting: &f.meeting,
            segments: &f.segments,
            people: &f.people,
            summary: Some(&f.summary),
            action_items: &f.items,
            notes,
        })
    }

    fn text(f: &Fixture, format: ExportFormat) -> String {
        let bytes = with(f, "Ask Dana about quotas.", |m| export(m, format)).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn markdown_has_every_section() {
        let f = fixture();
        let md = text(&f, ExportFormat::Markdown);
        assert!(md.starts_with("# AKS cutover: R&D <sync>\n"));
        assert!(md.contains("**Duration:** 47m"));
        assert!(md.contains("**Attendees:** Jeremie, Priya Nair"));
        assert!(md.contains("## Summary\n\n- [05:00] Cutover moves to Oct 22."));
        assert!(md.contains("## Decisions\n\n- [04:12] Keep the GPU pool."));
        assert!(md.contains(
            "- [ ] [05:05] Draft the rollback plan — Priya Nair, due Friday \
             ([OPS-7](https://x.atlassian.net/browse/OPS-7))"
        ));
        assert!(md.contains("## Open questions\n\n- [00:14] Who tells support?"));
        assert!(md.contains("## Notes\n\nAsk Dana about quotas."));
        assert!(md.contains("**[00:14] Jeremie:** Main thing today"));
        assert!(md.contains("**[04:12] Speaker 3:** Can we cut over"));
        assert!(md.contains("**[05:00] Priya Nair:** Yes"));
        let order = [
            "## Summary",
            "## Decisions",
            "## Action items",
            "## Open",
            "## Notes",
        ];
        let pos: Vec<usize> = order.iter().map(|h| md.find(h).unwrap()).collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]));
        assert!(md.find("## Notes").unwrap() < md.find("## Transcript").unwrap());
    }

    #[test]
    fn markdown_without_summary_skips_empty_sections() {
        let f = fixture();
        let md = String::from_utf8(
            export(
                &MeetingExport {
                    meeting: &f.meeting,
                    segments: &[],
                    people: &f.people,
                    summary: None,
                    action_items: &[],
                    notes: "  ",
                },
                ExportFormat::Markdown,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(!md.contains("## Summary"));
        assert!(!md.contains("## Action items"));
        assert!(!md.contains("## Notes"));
        assert!(md.contains("## Transcript\n\n_No transcript._"));
    }

    #[test]
    fn srt_cues() {
        let f = fixture();
        let srt = text(&f, ExportFormat::Srt);
        assert!(srt.starts_with(
            "1\n00:00:14,000 --> 00:00:17,500\nJeremie: Main thing today is the AKS cutover.\n\n2\n"
        ));
        assert!(
            srt.contains("3\n00:05:00,000 --> 00:05:03,500\nPriya Nair: Yes --> on the 22nd.\n")
        );
    }

    #[test]
    fn vtt_cues_use_voice_spans_and_escape() {
        let f = fixture();
        let vtt = text(&f, ExportFormat::Vtt);
        assert!(vtt.starts_with("WEBVTT\n\n00:00:14.000 --> 00:00:17.500\n<v Jeremie>Main"));
        assert!(vtt.contains(
            "<v Speaker 3>Can we cut over without the GPU pool &amp; keep &lt;prod&gt; up?"
        ));
        // `-->` never appears inside a cue's text.
        assert!(vtt.contains("<v Priya Nair>Yes --&gt; on the 22nd."));
    }

    #[test]
    fn json_shape() {
        let f = fixture();
        let v: serde_json::Value = serde_json::from_str(&text(&f, ExportFormat::Json)).unwrap();
        assert_eq!(v["format"], "switchscribe.meeting/1");
        assert_eq!(v["title"], "AKS cutover: R&D <sync>");
        assert_eq!(v["started_at_utc"], "2026-10-15T08:00:00+00:00");
        assert_eq!(v["attendees"], serde_json::json!(["Jeremie", "Priya Nair"]));
        assert_eq!(v["summary"]["decisions"][0]["at"], "04:12");
        assert_eq!(v["action_items"][0]["owner"], "Priya Nair");
        assert_eq!(v["action_items"][0]["status"], "in-jira");
        assert_eq!(v["action_items"][0]["jira_key"], "OPS-7");
        assert_eq!(v["notes"], "Ask Dana about quotas.");
        assert_eq!(v["transcript"][1]["speaker"], "Speaker 3");
        assert_eq!(v["transcript"][2]["start_ms"], 300_000);
        assert_eq!(v["transcript"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn word_document_is_a_valid_package() {
        let f = fixture();
        let bytes = with(&f, "Line one\nLine & two", |m| {
            export(m, ExportFormat::Word)
        })
        .unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        for name in [
            "[Content_Types].xml",
            "_rels/.rels",
            "word/_rels/document.xml.rels",
            "word/styles.xml",
        ] {
            assert!(zip.by_name(name).is_ok(), "{name} missing");
        }
        let mut doc = String::new();
        zip.by_name("word/document.xml")
            .unwrap()
            .read_to_string(&mut doc)
            .unwrap();
        assert!(doc.contains("AKS cutover: R&amp;D &lt;sync&gt;"));
        assert!(doc.contains("GPU pool &amp; keep &lt;prod&gt; up?"));
        assert!(doc.contains(">Line &amp; two<"));
        assert!(doc.contains("[05:00] Priya Nair: "));
        assert!(doc.contains("Draft the rollback plan — Priya Nair, due Friday"));
        assert!(doc.contains("<w:pStyle w:val=\"Heading1\"/>"));
        // Well-formed enough: no raw `&` or `<` outside markup, and balanced paragraphs.
        assert!(!doc.contains("R&D"));
        assert_eq!(doc.matches("<w:p>").count(), doc.matches("</w:p>").count());
        assert_eq!(doc.matches("<w:r>").count(), doc.matches("</w:r>").count());
        assert!(doc.ends_with("</w:document>"));
    }

    #[test]
    fn xml_escape_drops_control_characters() {
        assert_eq!(xml_escape("a\u{1}b\t\"c\"'"), "ab\t&quot;c&quot;&apos;");
    }

    #[test]
    fn file_names_are_safe() {
        let mut m = fixture().meeting;
        let day = m.local_date().format("%Y-%m-%d").to_string();
        assert_eq!(
            file_name(&m, ExportFormat::Markdown),
            format!("{day} AKS cutover R&D sync.md")
        );
        m.title = "  a/b\\c|d?e*f \"g\"...  ".into();
        assert_eq!(
            file_name(&m, ExportFormat::Word),
            format!("{day} a b c d e f g.docx")
        );
        m.title = "???".into();
        assert_eq!(
            file_name(&m, ExportFormat::Srt),
            format!("{day} Meeting.srt")
        );
        m.title = "x".repeat(200);
        assert_eq!(file_name(&m, ExportFormat::Json).len(), 11 + 80 + 5);
    }
}
