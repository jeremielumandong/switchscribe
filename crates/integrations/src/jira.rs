//! Jira Cloud: action items become issues through REST API v3, signed in with the user's
//! Atlassian email and an API token (basic auth).
//!
//! Request URLs and bodies are built by the pure functions in this module
//! ([`issue_body`], [`description_adf`], [`user_search_url`] …) and sent by
//! [`JiraClient`], so tests check what would be sent without a network.

use base64::Engine as _;
use chrono::{Datelike as _, Days, NaiveDate, Weekday};
use serde_json::{Value, json};
use switchscribe_store::ActionItem;

use crate::http::{self, Reply, enc, first_line};
use crate::{Error, Result, clock};

/// The Jira summary field's limit.
const SUMMARY_MAX: usize = 255;

/// Where and as whom to create issues.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JiraConfig {
    /// The Jira Cloud site, `https://example.atlassian.net` (a bare host also works).
    pub site: String,
    /// The Atlassian account email.
    pub email: String,
    /// An API token for that account (id.atlassian.com → Security → API tokens).
    pub token: String,
    /// Project key ("OPS").
    pub project: String,
    /// Issue type name; "Task" when empty.
    pub issue_type: String,
}

impl JiraConfig {
    /// The site as a base URL: `https://` added when missing, no trailing slash.
    pub fn base_url(&self) -> String {
        let s = self.site.trim().trim_end_matches('/');
        if s.starts_with("https://") || s.starts_with("http://") {
            s.to_owned()
        } else {
            format!("https://{s}")
        }
    }

    /// The issue type to use.
    pub fn issue_type(&self) -> &str {
        let t = self.issue_type.trim();
        if t.is_empty() { "Task" } else { t }
    }

    /// `Basic base64(email:token)`.
    pub fn authorization(&self) -> String {
        let raw = format!("{}:{}", self.email.trim(), self.token.trim());
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    }

    /// An issue's web link: `https://example.atlassian.net/browse/OPS-7`.
    pub fn browse_url(&self, key: &str) -> String {
        format!("{}/browse/{key}", self.base_url())
    }
}

/// An issue to create from an action item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewIssue {
    /// The issue's summary (one line; cut to Jira's 255 characters).
    pub summary: String,
    /// The action item's full text, the description's first paragraph.
    pub description_text: String,
    /// The meeting it came from.
    pub meeting_title: String,
    /// The meeting's (local) day.
    pub meeting_date: NaiveDate,
    /// Where in the meeting it came up, milliseconds.
    pub at_ms: i64,
    /// The transcript line it came from, quoted in the description.
    pub quote: Option<String>,
    /// A link back to the moment (for example a `switchscribe://` deep link), shown as
    /// the description's last paragraph.
    pub link: Option<String>,
    /// Who owns it, as the meeting named them.
    pub owner: Option<String>,
    /// When it is due, as said ("Friday").
    pub due_said: Option<String>,
    /// The due date understood from [`NewIssue::due_said`]; sets the `duedate` field.
    pub due_date: Option<NaiveDate>,
    /// The assignee's Atlassian account id (see [`JiraClient::find_user`]).
    pub assignee: Option<String>,
}

impl NewIssue {
    /// An issue for a stored action item: summary and description from its text, owner
    /// and due date as said, the due date parsed against the meeting's day. Quote, link
    /// and assignee are the caller's to add.
    pub fn from_action(item: &ActionItem) -> NewIssue {
        let meeting_date = chrono::DateTime::from_timestamp(item.meeting_started_at, 0)
            .unwrap_or_default()
            .with_timezone(&chrono::Local)
            .date_naive();
        let due_said = item.due.clone().filter(|d| !d.trim().is_empty());
        NewIssue {
            summary: item.text.clone(),
            description_text: item.text.clone(),
            meeting_title: item.meeting_title.clone(),
            meeting_date,
            at_ms: item.at_ms,
            quote: None,
            link: None,
            owner: item.owner_name.clone().filter(|o| !o.trim().is_empty()),
            due_date: due_said.as_deref().and_then(|d| parse_due(d, meeting_date)),
            due_said,
            assignee: None,
        }
    }
}

/// An issue Jira created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedIssue {
    /// Its key, "OPS-7".
    pub key: String,
    /// Its web link.
    pub url: String,
}

/// `GET /rest/api/3/myself`.
pub fn myself_url(config: &JiraConfig) -> String {
    format!("{}/rest/api/3/myself", config.base_url())
}

/// `GET /rest/api/3/user/search?query=…`.
pub fn user_search_url(config: &JiraConfig, name: &str) -> String {
    format!(
        "{}/rest/api/3/user/search?query={}",
        config.base_url(),
        enc(name.trim())
    )
}

/// `POST /rest/api/3/issue`.
pub fn create_issue_url(config: &JiraConfig) -> String {
    format!("{}/rest/api/3/issue", config.base_url())
}

/// The issue summary: one line, at most 255 characters.
fn summary_line(s: &str) -> String {
    let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= SUMMARY_MAX {
        return line;
    }
    let mut cut: String = line.chars().take(SUMMARY_MAX - 1).collect();
    cut.push('…');
    cut
}

fn text_node(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn strong(text: &str) -> Value {
    json!({ "type": "text", "text": text, "marks": [{ "type": "strong" }] })
}

fn paragraph(content: Vec<Value>) -> Value {
    json!({ "type": "paragraph", "content": content })
}

/// The issue description in Atlassian Document Format: the item's text, its owner and
/// due date as said, where it came from in the meeting, the quoted transcript line, and
/// the link back. ADF text nodes must not be empty, so empty parts are left out.
pub fn description_adf(issue: &NewIssue) -> Value {
    let mut content = Vec::new();
    let body = issue.description_text.trim();
    if !body.is_empty() {
        content.push(paragraph(vec![text_node(body)]));
    }
    if let Some(owner) = issue
        .owner
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        content.push(paragraph(vec![strong("Owner: "), text_node(owner)]));
    }
    if let Some(due) = issue
        .due_said
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let mut due = due.to_owned();
        if let Some(d) = issue.due_date {
            due.push_str(&format!(" ({})", d.format("%a %-d %b %Y")));
        }
        content.push(paragraph(vec![strong("Due: "), text_node(&due)]));
    }
    let title = if issue.meeting_title.trim().is_empty() {
        "a meeting"
    } else {
        issue.meeting_title.trim()
    };
    content.push(paragraph(vec![text_node(&format!(
        "From {title} on {} at {}",
        issue.meeting_date.format("%a %-d %b %Y"),
        clock(issue.at_ms)
    ))]));
    if let Some(q) = issue
        .quote
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        content.push(json!({ "type": "blockquote", "content": [paragraph(vec![text_node(q)])] }));
    }
    if let Some(link) = issue
        .link
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        content.push(paragraph(vec![json!({
            "type": "text",
            "text": link,
            "marks": [{ "type": "link", "attrs": { "href": link } }]
        })]));
    }
    json!({ "type": "doc", "version": 1, "content": content })
}

/// The `POST /rest/api/3/issue` body. `duedate` is set only for a parsed date and
/// `assignee` only for a known account id.
pub fn issue_body(config: &JiraConfig, issue: &NewIssue) -> Value {
    let summary = if issue.summary.trim().is_empty() {
        &issue.description_text
    } else {
        &issue.summary
    };
    let mut fields = json!({
        "project": { "key": config.project.trim() },
        "issuetype": { "name": config.issue_type() },
        "summary": summary_line(summary),
        "description": description_adf(issue),
    });
    if let Some(d) = issue.due_date {
        fields["duedate"] = json!(d.format("%Y-%m-%d").to_string());
    }
    if let Some(a) = issue.assignee.as_deref().filter(|a| !a.is_empty()) {
        fields["assignee"] = json!({ "accountId": a });
    }
    json!({ "fields": fields })
}

/// What Jira says went wrong, as one line: `errorMessages` and `errors` joined.
fn jira_messages(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return first_line(body);
    };
    let mut parts: Vec<String> = v["errorMessages"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if let Some(errors) = v["errors"].as_object() {
        for (field, msg) in errors {
            parts.push(format!("{field}: {}", msg.as_str().unwrap_or_default()));
        }
    }
    parts.join("; ")
}

/// The fields Jira complained about (`errors` keys), sorted.
fn error_fields(body: &str) -> Vec<String> {
    let mut fields: Vec<String> = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["errors"].as_object().map(|o| o.keys().cloned().collect()))
        .unwrap_or_default();
    fields.sort();
    fields
}

/// A failed reply as a readable error.
fn jira_error(config: &JiraConfig, reply: &Reply, creating: bool) -> Error {
    let detail = jira_messages(&reply.body);
    let with = |s: String| {
        if detail.is_empty() {
            s
        } else {
            format!("{s} ({detail})")
        }
    };
    match reply.status {
        401 => Error::Auth("Jira rejected the email or API token".to_owned()),
        403 => Error::Rejected(with(format!(
            "Jira refused: your account cannot create issues in project {}",
            config.project.trim()
        ))),
        404 if creating => Error::Rejected(with(format!(
            "Jira could not find project {} (check the project key and site)",
            config.project.trim()
        ))),
        404 => Error::Rejected(format!(
            "Jira was not found at {} (check the site URL)",
            config.base_url()
        )),
        400 => {
            let fields = error_fields(&reply.body);
            if fields.iter().any(|f| f == "project") {
                Error::Rejected(with(format!(
                    "Jira has no project {} you can use",
                    config.project.trim()
                )))
            } else if fields.iter().any(|f| f == "issuetype") {
                Error::Rejected(with(format!(
                    "Project {} has no issue type \"{}\"",
                    config.project.trim(),
                    config.issue_type()
                )))
            } else {
                Error::Rejected(with("Jira rejected the issue".to_owned()))
            }
        }
        429 => Error::Rejected("Jira is limiting requests; try again in a minute".to_owned()),
        s if s >= 500 => Error::Rejected(format!("Jira had a server error ({s}); try again later")),
        s => Error::Rejected(with(format!("Jira answered {s}"))),
    }
}

fn bad(detail: impl Into<String>) -> Error {
    Error::BadResponse {
        service: "Jira",
        detail: detail.into(),
    }
}

/// The display name in a `/myself` reply.
fn parse_myself(body: &str) -> Result<String> {
    let v: Value = serde_json::from_str(body).map_err(|e| bad(e.to_string()))?;
    v["displayName"]
        .as_str()
        .or_else(|| v["emailAddress"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| bad("no displayName"))
}

/// The account id of the one active user a search found. Several matches count only when
/// exactly one display name equals `name` (ignoring case); app (bot) accounts are skipped.
fn parse_user_search(body: &str, name: &str) -> Result<Option<String>> {
    let v: Value = serde_json::from_str(body).map_err(|e| bad(e.to_string()))?;
    let users: Vec<&Value> = v
        .as_array()
        .ok_or_else(|| bad("user search is not a list"))?
        .iter()
        .filter(|u| u["active"].as_bool().unwrap_or(true))
        .filter(|u| u["accountType"].as_str().is_none_or(|t| t == "atlassian"))
        .collect();
    let id = |u: &Value| u["accountId"].as_str().map(str::to_owned);
    if let [one] = users.as_slice() {
        return Ok(id(one));
    }
    let wanted = name.trim().to_lowercase();
    let exact: Vec<&&Value> = users
        .iter()
        .filter(|u| {
            u["displayName"]
                .as_str()
                .is_some_and(|d| d.trim().to_lowercase() == wanted)
        })
        .collect();
    Ok(match exact.as_slice() {
        [one] => id(one),
        _ => None,
    })
}

/// The key and link in a create reply.
fn parse_created(config: &JiraConfig, body: &str) -> Result<CreatedIssue> {
    let v: Value = serde_json::from_str(body).map_err(|e| bad(e.to_string()))?;
    let key = v["key"].as_str().ok_or_else(|| bad("no issue key"))?;
    Ok(CreatedIssue {
        key: key.to_owned(),
        url: config.browse_url(key),
    })
}

/// A blocking Jira client; call it from a worker thread.
pub struct JiraClient {
    config: JiraConfig,
    agent: ureq::Agent,
}

impl JiraClient {
    /// A client for `config`. Nothing is sent until a method is called.
    pub fn new(config: JiraConfig) -> JiraClient {
        JiraClient {
            config,
            agent: http::agent(),
        }
    }

    /// The configuration.
    pub fn config(&self) -> &JiraConfig {
        &self.config
    }

    fn get(&self, url: &str) -> Result<Reply> {
        http::finish(
            "Jira",
            self.agent
                .get(url)
                .header("Authorization", self.config.authorization())
                .header("Accept", "application/json")
                .call(),
        )
    }

    /// The signed-in user's display name (the Test connection button).
    pub fn myself(&self) -> Result<String> {
        let reply = self.get(&myself_url(&self.config))?;
        if !reply.ok() {
            return Err(jira_error(&self.config, &reply, false));
        }
        parse_myself(&reply.body)
    }

    /// The account id of the one user called `name`; `None` when nobody or several
    /// people match.
    pub fn find_user(&self, name: &str) -> Result<Option<String>> {
        if name.trim().is_empty() {
            return Ok(None);
        }
        let reply = self.get(&user_search_url(&self.config, name))?;
        if !reply.ok() {
            return Err(jira_error(&self.config, &reply, false));
        }
        parse_user_search(&reply.body, name)
    }

    fn post_issue(&self, body: &Value) -> Result<Reply> {
        http::finish(
            "Jira",
            self.agent
                .post(&create_issue_url(&self.config))
                .header("Authorization", self.config.authorization())
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .send(body.to_string()),
        )
    }

    /// Create an issue. When Jira refuses only the due date or assignee (fields missing
    /// from the project's create screen, or a user who cannot be assigned), it tries once
    /// more without them.
    pub fn create_issue(&self, issue: &NewIssue) -> Result<CreatedIssue> {
        let mut reply = self.post_issue(&issue_body(&self.config, issue))?;
        if reply.status == 400 {
            let fields = error_fields(&reply.body);
            let optional = ["duedate", "assignee"];
            if !fields.is_empty() && fields.iter().all(|f| optional.contains(&f.as_str())) {
                tracing::warn!(
                    ?fields,
                    "Jira refused optional fields; retrying without them"
                );
                let mut retry = issue.clone();
                if fields.iter().any(|f| f == "duedate") {
                    retry.due_date = None;
                }
                if fields.iter().any(|f| f == "assignee") {
                    retry.assignee = None;
                }
                reply = self.post_issue(&issue_body(&self.config, &retry))?;
            }
        }
        if !reply.ok() {
            return Err(jira_error(&self.config, &reply, true));
        }
        let created = parse_created(&self.config, &reply.body)?;
        tracing::info!(key = %created.key, "Jira issue created");
        Ok(created)
    }
}

/// A due date as said in a meeting, read against the meeting's day:
///
/// - "today", "EOD" → the meeting day; "tomorrow" → the day after;
/// - a weekday ("Friday", "fri", "next Tuesday", "this Thursday") → the next such day
///   strictly after the meeting day ("next" is read the same way: people say "next
///   Tuesday" on a Thursday for the coming one);
/// - "end of week" / "EOW" / "end of the week" → that week's Friday (the meeting day
///   itself on a Friday; the following Friday on a weekend);
/// - "Oct 22", "October 22nd", "22 Oct", "22nd of October", optionally with a year →
///   that date, in the next year when it has already passed and no year is given;
/// - ISO dates, "2026-10-22".
///
/// Leading "by", "on", "due", "until", "before" are ignored. Anything else is `None`.
pub fn parse_due(said: &str, meeting_day: NaiveDate) -> Option<NaiveDate> {
    let lower = said.trim().to_lowercase();
    let mut words: Vec<String> = lower
        .split(|c: char| c.is_whitespace() || c == ',')
        .map(|w| w.trim_matches(|c: char| matches!(c, '.' | '!' | '?' | ';' | '(' | ')')))
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect();
    while words.first().is_some_and(|w| {
        matches!(
            w.as_str(),
            "by" | "on" | "due" | "until" | "till" | "before"
        )
    }) {
        words.remove(0);
    }
    let words: Vec<&str> = words
        .iter()
        .map(String::as_str)
        .filter(|w| !matches!(*w, "the" | "of"))
        .collect();
    match words.as_slice() {
        [] => None,
        ["today"] | ["eod"] | ["end", "day"] | ["end", "today"] => Some(meeting_day),
        ["tomorrow"] | ["eod", "tomorrow"] => meeting_day.succ_opt(),
        ["eow"] | ["end", "week"] | ["end", "this", "week"] => Some(end_of_week(meeting_day)),
        ["end", "next", "week"] => end_of_week(meeting_day).checked_add_days(Days::new(7)),
        [w] | ["next" | "this" | "coming", w] if weekday(w).is_some() => {
            weekday(w).map(|wd| next_weekday(meeting_day, wd))
        }
        [iso] if iso.len() == 10 && iso.as_bytes()[4] == b'-' => {
            NaiveDate::parse_from_str(iso, "%Y-%m-%d").ok()
        }
        [a, b] => month_day(a, b, None, meeting_day),
        [a, b, y] => month_day(a, b, Some(y), meeting_day),
        _ => None,
    }
}

/// The next `wd` strictly after `day`.
fn next_weekday(day: NaiveDate, wd: Weekday) -> NaiveDate {
    let ahead = (wd.num_days_from_monday() + 7 - day.weekday().num_days_from_monday()) % 7;
    let ahead = if ahead == 0 { 7 } else { ahead };
    day + Days::new(u64::from(ahead))
}

/// Friday of `day`'s week (the next Friday from a weekend).
fn end_of_week(day: NaiveDate) -> NaiveDate {
    if day.weekday() == Weekday::Fri {
        day
    } else {
        next_weekday(day, Weekday::Fri)
    }
}

fn weekday(w: &str) -> Option<Weekday> {
    Some(match w {
        "monday" | "mon" => Weekday::Mon,
        "tuesday" | "tue" | "tues" => Weekday::Tue,
        "wednesday" | "wed" => Weekday::Wed,
        "thursday" | "thu" | "thur" | "thurs" => Weekday::Thu,
        "friday" | "fri" => Weekday::Fri,
        "saturday" | "sat" => Weekday::Sat,
        "sunday" | "sun" => Weekday::Sun,
        _ => return None,
    })
}

fn month(w: &str) -> Option<u32> {
    const NAMES: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let w = w.trim_end_matches('.');
    if w.len() < 3 {
        return None;
    }
    NAMES
        .iter()
        .position(|n| *n == w || (n.starts_with(w) && (w.len() == 3 || w == "sept")))
        .map(|i| i as u32 + 1)
}

/// "22", "22nd", "1st" → the day number.
fn day_number(w: &str) -> Option<u32> {
    let digits = w
        .strip_suffix("st")
        .or_else(|| w.strip_suffix("nd"))
        .or_else(|| w.strip_suffix("rd"))
        .or_else(|| w.strip_suffix("th"))
        .unwrap_or(w);
    digits.parse::<u32>().ok().filter(|d| (1..=31).contains(d))
}

fn month_day(a: &str, b: &str, year: Option<&str>, meeting_day: NaiveDate) -> Option<NaiveDate> {
    let (m, d) = match (month(a), month(b)) {
        (Some(m), None) => (m, day_number(b)?),
        (None, Some(m)) => (m, day_number(a)?),
        _ => return None,
    };
    if let Some(y) = year {
        let y: i32 = y.parse().ok().filter(|y| (2000..=2100).contains(y))?;
        return NaiveDate::from_ymd_opt(y, m, d);
    }
    let this_year = NaiveDate::from_ymd_opt(meeting_day.year(), m, d);
    match this_year {
        Some(date) if date >= meeting_day => Some(date),
        _ => NaiveDate::from_ymd_opt(meeting_day.year() + 1, m, d),
    }
}

#[cfg(test)]
mod tests {
    use switchscribe_store::ActionStatus;

    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn config() -> JiraConfig {
        JiraConfig {
            site: "acme.atlassian.net/".into(),
            email: "jeremie@example.com".into(),
            token: "tok".into(),
            project: "OPS".into(),
            issue_type: String::new(),
        }
    }

    fn issue() -> NewIssue {
        NewIssue {
            summary: "Draft the rollback plan".into(),
            description_text: "Draft the rollback plan for the AKS cutover".into(),
            meeting_title: "AKS sync".into(),
            meeting_date: d(2026, 10, 15),
            at_ms: 252_000,
            quote: Some("Priya: I'll draft the rollback plan by Friday.".into()),
            link: Some("switchscribe://meeting/m1?t=252".into()),
            owner: Some("Priya Nair".into()),
            due_said: Some("Friday".into()),
            due_date: Some(d(2026, 10, 16)),
            assignee: Some("acc-1".into()),
        }
    }

    #[test]
    fn urls_and_auth() {
        let c = config();
        assert_eq!(c.base_url(), "https://acme.atlassian.net");
        assert_eq!(
            myself_url(&c),
            "https://acme.atlassian.net/rest/api/3/myself"
        );
        assert_eq!(
            user_search_url(&c, "Priya Nair"),
            "https://acme.atlassian.net/rest/api/3/user/search?query=Priya%20Nair"
        );
        assert_eq!(
            create_issue_url(&c),
            "https://acme.atlassian.net/rest/api/3/issue"
        );
        assert_eq!(
            c.browse_url("OPS-7"),
            "https://acme.atlassian.net/browse/OPS-7"
        );
        // base64("jeremie@example.com:tok")
        assert_eq!(c.authorization(), "Basic amVyZW1pZUBleGFtcGxlLmNvbTp0b2s=");
        assert_eq!(c.issue_type(), "Task");
    }

    #[test]
    fn issue_body_fields() {
        let b = issue_body(&config(), &issue());
        let f = &b["fields"];
        assert_eq!(f["project"]["key"], "OPS");
        assert_eq!(f["issuetype"]["name"], "Task");
        assert_eq!(f["summary"], "Draft the rollback plan");
        assert_eq!(f["duedate"], "2026-10-16");
        assert_eq!(f["assignee"]["accountId"], "acc-1");
        let mut bare = issue();
        bare.due_date = None;
        bare.assignee = None;
        bare.summary = "  multi\nline   summary ".into();
        let f = issue_body(&config(), &bare)["fields"].clone();
        assert!(f.get("duedate").is_none());
        assert!(f.get("assignee").is_none());
        assert_eq!(f["summary"], "multi line summary");
        bare.summary = "x".repeat(400);
        let f = issue_body(&config(), &bare)["fields"].clone();
        assert_eq!(f["summary"].as_str().unwrap().chars().count(), 255);
    }

    #[test]
    fn description_is_adf() {
        let v = description_adf(&issue());
        assert_eq!(v["type"], "doc");
        assert_eq!(v["version"], 1);
        let c = v["content"].as_array().unwrap();
        assert_eq!(
            c[0]["content"][0]["text"],
            "Draft the rollback plan for the AKS cutover"
        );
        assert_eq!(c[1]["content"][0]["text"], "Owner: ");
        assert_eq!(c[1]["content"][0]["marks"][0]["type"], "strong");
        assert_eq!(c[1]["content"][1]["text"], "Priya Nair");
        assert_eq!(c[2]["content"][1]["text"], "Friday (Fri 16 Oct 2026)");
        assert_eq!(
            c[3]["content"][0]["text"],
            "From AKS sync on Thu 15 Oct 2026 at 04:12"
        );
        assert_eq!(c[4]["type"], "blockquote");
        assert_eq!(
            c[4]["content"][0]["content"][0]["text"],
            "Priya: I'll draft the rollback plan by Friday."
        );
        let link = &c[5]["content"][0];
        assert_eq!(
            link["marks"][0]["attrs"]["href"],
            "switchscribe://meeting/m1?t=252"
        );
        // Empty parts never become empty text nodes.
        let mut sparse = issue();
        sparse.owner = Some(" ".into());
        sparse.quote = None;
        sparse.link = None;
        sparse.due_said = None;
        let c = description_adf(&sparse)["content"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(c.len(), 2);
        let s = serde_json::to_string(&c).unwrap();
        assert!(!s.contains(r#""text":"""#));
    }

    #[test]
    fn from_action_parses_the_due_date() {
        let item = ActionItem {
            id: 1,
            meeting_id: "m".into(),
            meeting_title: "AKS sync".into(),
            meeting_started_at: 1_792_065_600, // Thu 15 Oct 2026 12:00 UTC
            text: "Draft the plan".into(),
            owner_id: None,
            owner_name: Some("Priya".into()),
            due: Some("Oct 22".into()),
            at_ms: 1_000,
            status: ActionStatus::Open,
            jira_key: None,
            jira_url: None,
        };
        let i = NewIssue::from_action(&item);
        assert_eq!(i.due_date, Some(d(2026, 10, 22)));
        assert_eq!(i.owner.as_deref(), Some("Priya"));
        assert_eq!(i.summary, "Draft the plan");
    }

    #[test]
    fn errors_read_well() {
        let c = config();
        let r = |status, body: &str| Reply {
            status,
            body: body.into(),
        };
        let e = jira_error(&c, &r(401, ""), false);
        assert!(matches!(e, Error::Auth(_)));
        assert_eq!(e.to_string(), "Jira rejected the email or API token");
        let e = jira_error(&c, &r(404, r#"{"errorMessages":["No project"]}"#), true);
        assert!(e.to_string().contains("project OPS"));
        assert!(e.to_string().contains("No project"));
        let e = jira_error(
            &c,
            &r(
                400,
                r#"{"errorMessages":[],"errors":{"issuetype":"invalid"}}"#,
            ),
            true,
        );
        assert!(e.to_string().contains("no issue type \"Task\""));
        let e = jira_error(
            &c,
            &r(400, r#"{"errors":{"project":"valid project is required"}}"#),
            true,
        );
        assert!(e.to_string().contains("no project OPS"));
        assert!(
            jira_error(&c, &r(503, "<html>"), true)
                .to_string()
                .contains("server error")
        );
        assert!(
            jira_error(&c, &r(404, ""), false)
                .to_string()
                .contains("site URL")
        );
        assert_eq!(
            error_fields(r#"{"errors":{"duedate":"x","assignee":"y"}}"#),
            ["assignee", "duedate"]
        );
    }

    #[test]
    fn parses_replies() {
        let c = config();
        assert_eq!(
            parse_myself(r#"{"accountId":"a","displayName":"Jeremie L"}"#).unwrap(),
            "Jeremie L"
        );
        assert_eq!(
            parse_created(&c, r#"{"id":"10000","key":"OPS-7","self":"…"}"#).unwrap(),
            CreatedIssue {
                key: "OPS-7".into(),
                url: "https://acme.atlassian.net/browse/OPS-7".into()
            }
        );
        assert!(parse_created(&c, "{}").is_err());
        let one = r#"[{"accountId":"a1","displayName":"Priya Nair","active":true,"accountType":"atlassian"}]"#;
        assert_eq!(
            parse_user_search(one, "Priya").unwrap().as_deref(),
            Some("a1")
        );
        let two = r#"[{"accountId":"a1","displayName":"Priya Nair","active":true},
                      {"accountId":"a2","displayName":"Priya Shah","active":true}]"#;
        assert_eq!(parse_user_search(two, "Priya").unwrap(), None);
        assert_eq!(
            parse_user_search(two, "priya shah").unwrap().as_deref(),
            Some("a2")
        );
        let inactive = r#"[{"accountId":"a1","displayName":"Priya","active":false},
                           {"accountId":"b","displayName":"Priya bot","accountType":"app"},
                           {"accountId":"a3","displayName":"Priya N","active":true}]"#;
        assert_eq!(
            parse_user_search(inactive, "Priya").unwrap().as_deref(),
            Some("a3")
        );
        assert_eq!(parse_user_search("[]", "x").unwrap(), None);
    }

    #[test]
    fn due_dates() {
        let thu = d(2026, 10, 15);
        let p = |s: &str| parse_due(s, thu);
        assert_eq!(p("Friday"), Some(d(2026, 10, 16)));
        assert_eq!(p("by fri."), Some(d(2026, 10, 16)));
        assert_eq!(p("next Tuesday"), Some(d(2026, 10, 20)));
        assert_eq!(p("Thursday"), Some(d(2026, 10, 22)));
        assert_eq!(p("Monday"), Some(d(2026, 10, 19)));
        assert_eq!(p("today"), Some(thu));
        assert_eq!(p("Tomorrow"), Some(d(2026, 10, 16)));
        assert_eq!(p("end of week"), Some(d(2026, 10, 16)));
        assert_eq!(p("end of the week"), Some(d(2026, 10, 16)));
        assert_eq!(p("end of next week"), Some(d(2026, 10, 23)));
        assert_eq!(
            parse_due("end of week", d(2026, 10, 16)),
            Some(d(2026, 10, 16))
        );
        assert_eq!(parse_due("EOW", d(2026, 10, 17)), Some(d(2026, 10, 23)));
        assert_eq!(p("Oct 22"), Some(d(2026, 10, 22)));
        assert_eq!(p("October 22nd"), Some(d(2026, 10, 22)));
        assert_eq!(p("22 Oct"), Some(d(2026, 10, 22)));
        assert_eq!(p("the 22nd of October"), Some(d(2026, 10, 22)));
        assert_eq!(p("Sept 3"), Some(d(2027, 9, 3)));
        assert_eq!(p("Jan 5, 2027"), Some(d(2027, 1, 5)));
        assert_eq!(p("2026-11-02"), Some(d(2026, 11, 2)));
        assert_eq!(p("Feb 30"), None);
        assert_eq!(p("soon"), None);
        assert_eq!(p("next sprint"), None);
        assert_eq!(p("may"), None);
        assert_eq!(p(""), None);
        assert_eq!(p("2026-13-01"), None);
    }
}
