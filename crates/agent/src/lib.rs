//! The coding-CLI agent (M4): switchscribe hosts an MCP server that the user's coding
//! CLI (Claude Code, Codex, Gemini CLI) calls to read and act on meetings, and runs that
//! CLI with summary templates after every call. There is no built-in LLM.

pub use switchscribe_store::CliKind;

/// The MCP server's name, as the CLI sees it.
pub const MCP_SERVER_NAME: &str = "switchscribe";

/// The MCP tools the server exposes.
pub const MCP_TOOLS: [&str; 7] = [
    "list_meetings",
    "get_transcript",
    "search_meetings",
    "get_action_items",
    "update_summary",
    "create_jira_issue",
    "export_meeting",
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
}
