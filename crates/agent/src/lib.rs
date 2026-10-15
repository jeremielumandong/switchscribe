//! The coding-CLI agent (M4): switchscribe hosts an MCP server that the user's coding
//! CLI (Claude Code, Codex, Gemini CLI) calls to read and act on meetings, and runs that
//! CLI with summary templates after every call. There is no built-in LLM.

pub mod ask;
pub mod cli;
pub mod mcp;
pub mod process;
pub mod summarize;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

pub use switchscribe_store::CliKind;
use switchscribe_store::Store;

pub use cli::{Cli, McpServer};

/// Errors from the agent.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The CLI is not installed (or not where Settings says).
    #[error("{0} was not found; install it or set its path in Settings")]
    CliNotFound(String),
    /// The CLI failed.
    #[error("{0}")]
    Cli(String),
    /// The CLI's answer could not be read.
    #[error("the answer could not be read: {0}")]
    Answer(String),
    /// A tool call was malformed.
    #[error("{0}")]
    Tool(String),
    /// Stopped by the user.
    #[error("cancelled")]
    Cancelled,
    /// The store failed.
    #[error(transparent)]
    Store(#[from] switchscribe_store::Error),
    /// JSON could not be written.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// A file could not be read or written.
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
}

/// Agent result.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The MCP server's name, as the CLI sees it.
pub const MCP_SERVER_NAME: &str = "switchscribe";

/// The MCP tools the server exposes (`export_meeting` arrives with exports in M5).
pub const MCP_TOOLS: [&str; 7] = [
    "list_meetings",
    "get_transcript",
    "get_summary",
    "search_meetings",
    "get_action_items",
    "update_summary",
    "create_jira_issue",
];

/// Tools that change something; the Ask panel shows an approve/deny card before each.
pub fn needs_approval(tool: &str) -> bool {
    matches!(tool, "update_summary" | "create_jira_issue")
}

/// Built-in summary templates, picked from the calendar title.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Template {
    /// Daily standup.
    Standup,
    /// One-on-one.
    OneOnOne,
    /// Planning.
    Planning,
    /// Vendor call.
    VendorCall,
    /// Anything else.
    General,
}

impl Template {
    /// Template name as shown in the summary header ("template: planning").
    pub fn key(self) -> &'static str {
        match self {
            Template::Standup => "standup",
            Template::OneOnOne => "1:1",
            Template::Planning => "planning",
            Template::VendorCall => "vendor call",
            Template::General => "general",
        }
    }

    /// Pick a template from a meeting title.
    pub fn for_title(title: &str) -> Template {
        let t = title.to_lowercase();
        if t.contains("standup") || t.contains("stand-up") {
            Template::Standup
        } else if t.contains("1:1") || t.contains("1-1") || t.contains("one on one") {
            Template::OneOnOne
        } else if t.contains("planning") || t.contains("roadmap") {
            Template::Planning
        } else if t.contains("vendor") {
            Template::VendorCall
        } else {
            Template::General
        }
    }
}

/// Summarize a meeting with `cli`: build the prompt from the store, run the CLI in
/// `workdir`, and store the summary and action items. `guidance` overrides the template's
/// own. Blocks for as long as the CLI takes; call it from a worker thread.
pub fn summarize_meeting(
    store: &std::sync::Mutex<Store>,
    meeting_id: &str,
    cli: &Cli,
    guidance: Option<&str>,
    workdir: &Path,
    cancel: &AtomicBool,
) -> Result<()> {
    let unavailable = || Error::Tool("the index is unavailable".into());
    let (meeting, segments, people) = {
        let s = store.lock().map_err(|_| unavailable())?;
        let m = s
            .meeting(meeting_id)?
            .ok_or_else(|| Error::Tool(format!("no meeting {meeting_id}")))?;
        (m, s.segments(meeting_id)?, s.people()?)
    };
    if segments.is_empty() {
        return Err(Error::Tool("the meeting has no transcript".into()));
    }
    let template = Template::for_title(&meeting.title);
    let prompt = summarize::summary_prompt(
        &meeting,
        &segments,
        &people,
        guidance.unwrap_or(template.guidance()),
    );
    let answer = cli.run(&prompt, workdir, None, cli::SUMMARY_TIMEOUT, cancel)?;
    let duration = i64::from(meeting.duration_secs) * 1000;
    let end = segments.iter().map(|s| s.end_ms).max().unwrap_or(0);
    let (mut summary, items) = summarize::parse_summary(&answer, duration.max(end), &people)?;
    summary.template = template.key().to_owned();
    summary.cli = cli.kind.label().to_owned();
    store
        .lock()
        .map_err(|_| unavailable())?
        .set_summary(meeting_id, &summary, &items)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_from_titles() {
        assert_eq!(Template::for_title("Platform standup"), Template::Standup);
        assert_eq!(Template::for_title("1:1 Priya"), Template::OneOnOne);
        assert_eq!(
            Template::for_title("AKS migration planning"),
            Template::Planning
        );
        assert_eq!(
            Template::for_title("Vendor call — Datadog"),
            Template::VendorCall
        );
        assert_eq!(
            Template::for_title("Cerberus incident review"),
            Template::General
        );
    }

    #[test]
    fn writes_need_approval() {
        assert!(needs_approval("create_jira_issue"));
        assert!(!needs_approval("search_meetings"));
        assert!(MCP_TOOLS.contains(&"update_summary"));
    }

    /// The real thing: `SWITCHSCRIBE_TEST_CLI=claude` (or codex, gemini) summarizes a
    /// short meeting with that CLI. Skipped otherwise; it costs a model call.
    #[test]
    fn summarizes_with_a_real_cli() {
        use switchscribe_store::{MeetingStatus, NewMeeting, NewSegment};

        let Ok(kind) = std::env::var("SWITCHSCRIBE_TEST_CLI") else {
            return;
        };
        let kind = match kind.as_str() {
            "codex" => CliKind::Codex,
            "gemini" => CliKind::GeminiCli,
            _ => CliKind::ClaudeCode,
        };
        let mut s = Store::open_in_memory().unwrap();
        let priya = s.add_person("Priya Nair", None, false).unwrap();
        let me = s.add_person("Jeremie", None, true).unwrap();
        s.insert_meeting(&NewMeeting {
            id: "m".into(),
            title: "AKS migration planning".into(),
            started_at: 0,
            duration_secs: 120,
            source_app: "Teams".into(),
            status: MeetingStatus::Ready,
            attendees: vec![me, priya],
        })
        .unwrap();
        let line = |start_ms, person_id, text: &str| NewSegment {
            start_ms,
            end_ms: start_ms + 8_000,
            person_id: Some(person_id),
            speaker_label: String::new(),
            text: text.into(),
            low_confidence: false,
        };
        s.add_segments(
            "m",
            &[
                line(
                    5_000,
                    me,
                    "Let's decide the cutover date for the AKS migration.",
                ),
                line(
                    14_000,
                    priya,
                    "Two of three pools are moved. I propose October 22.",
                ),
                line(
                    25_000,
                    me,
                    "Agreed, October 22. Priya, can you write the rollback runbook by Friday?",
                ),
                line(
                    36_000,
                    priya,
                    "Yes, I'll have the runbook by Friday. Do we tell the vendor first?",
                ),
            ],
        )
        .unwrap();
        let store = std::sync::Mutex::new(s);
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::find(kind, None).unwrap();
        summarize_meeting(&store, "m", &cli, None, dir.path(), &AtomicBool::new(false)).unwrap();
        let s = store.lock().unwrap();
        let sum = s.summary("m").unwrap().unwrap();
        assert!(
            !sum.summary.is_empty() && !sum.decisions.is_empty(),
            "{sum:?}"
        );
        let items = s.action_items(Some("m")).unwrap();
        assert!(items.iter().any(|i| i.owner_id == Some(priya)), "{items:?}");
        println!("{sum:#?}\n{items:#?}");
    }

    #[cfg(unix)]
    #[test]
    fn summarizes_a_meeting_with_a_fake_cli() {
        use std::os::unix::fs::PermissionsExt as _;
        use switchscribe_store::{MeetingStatus, NewMeeting, NewSegment};

        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("claude");
        // Answers with prose around the JSON, as CLIs do.
        std::fs::write(
            &fake,
            "#!/bin/sh\ncat > /dev/null\necho 'Sure:'\necho '{\"summary\":[{\"text\":\"Pools moved.\",\"at\":\"00:31\"}],\"action_items\":[{\"text\":\"Runbook\",\"owner\":\"Priya\",\"at\":\"00:40\"}]}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut s = Store::open_in_memory().unwrap();
        let priya = s.add_person("Priya Nair", None, false).unwrap();
        s.insert_meeting(&NewMeeting {
            id: "m".into(),
            title: "Platform standup".into(),
            started_at: 0,
            duration_secs: 60,
            source_app: "Teams".into(),
            status: MeetingStatus::Ready,
            attendees: vec![priya],
        })
        .unwrap();
        s.add_segments(
            "m",
            &[NewSegment {
                start_ms: 31_000,
                end_ms: 41_000,
                person_id: Some(priya),
                speaker_label: String::new(),
                text: "Pools are moved; I'll write the runbook.".into(),
                low_confidence: false,
            }],
        )
        .unwrap();
        let store = std::sync::Mutex::new(s);
        let cli = Cli::find(CliKind::ClaudeCode, Some(&fake)).unwrap();
        summarize_meeting(
            &store,
            "m",
            &cli,
            None,
            &dir.path().join("run"),
            &AtomicBool::new(false),
        )
        .unwrap();
        let s = store.lock().unwrap();
        let sum = s.summary("m").unwrap().unwrap();
        assert_eq!(sum.template, "standup");
        assert_eq!(sum.cli, "Claude Code");
        assert_eq!(sum.summary[0].at_ms, 31_000);
        assert_eq!(s.action_items(Some("m")).unwrap()[0].owner_id, Some(priya));
    }
}
