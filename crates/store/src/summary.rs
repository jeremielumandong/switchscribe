//! Summaries and action items: what the coding CLI writes after a call, and the changes
//! the Ask panel's agent proposes for the user to approve.

use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

use crate::{Result, Store};

/// One summary line and the moment it came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bullet {
    /// The line.
    pub text: String,
    /// Where in the meeting it was said, milliseconds.
    pub at_ms: i64,
}

/// A meeting's summary (its action items are stored separately, see [`ActionItem`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// The template it was written with ("planning").
    pub template: String,
    /// The CLI that wrote it ("Claude Code").
    pub cli: String,
    /// When, seconds since the epoch.
    pub generated_at: i64,
    /// What happened.
    pub summary: Vec<Bullet>,
    /// What was decided.
    pub decisions: Vec<Bullet>,
    /// What is still open.
    pub open_questions: Vec<Bullet>,
}

/// Where an action item stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionStatus {
    /// Not done.
    Open,
    /// Sent to Jira (M5).
    InJira,
    /// Done.
    Done,
}

impl ActionStatus {
    /// The stored value.
    pub fn as_str(self) -> &'static str {
        match self {
            ActionStatus::Open => "open",
            ActionStatus::InJira => "in-jira",
            ActionStatus::Done => "done",
        }
    }

    /// Parse a stored value; unknown values read as open.
    pub fn parse(s: &str) -> ActionStatus {
        match s {
            "in-jira" => ActionStatus::InJira,
            "done" => ActionStatus::Done,
            _ => ActionStatus::Open,
        }
    }

    /// As the status dropdown shows it.
    pub fn label(self) -> &'static str {
        match self {
            ActionStatus::Open => "Open",
            ActionStatus::InJira => "In Jira",
            ActionStatus::Done => "Done",
        }
    }
}

/// An action item to store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewActionItem {
    /// What to do.
    pub text: String,
    /// Who, when they are someone in the library.
    pub owner_id: Option<i64>,
    /// Who, as the summary named them (kept when nobody matches).
    pub owner_name: Option<String>,
    /// When, as said ("Friday", "Oct 22").
    pub due: Option<String>,
    /// Where in the meeting it came up, milliseconds.
    pub at_ms: i64,
}

/// A stored action item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionItem {
    /// Row id.
    pub id: i64,
    /// Its meeting.
    pub meeting_id: String,
    /// Its meeting's title.
    pub meeting_title: String,
    /// Its meeting's start, seconds since the epoch.
    pub meeting_started_at: i64,
    /// What to do.
    pub text: String,
    /// Who, when they are someone in the library.
    pub owner_id: Option<i64>,
    /// Who, as the summary named them.
    pub owner_name: Option<String>,
    /// When.
    pub due: Option<String>,
    /// Where in the meeting it came up, milliseconds.
    pub at_ms: i64,
    /// Where it stands.
    pub status: ActionStatus,
    /// The Jira issue it became ("OPS-123"), once sent.
    pub jira_key: Option<String>,
    /// That issue's web link.
    pub jira_url: Option<String>,
}

/// A change the Ask panel's agent asked for, waiting for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingAction {
    /// Row id.
    pub id: i64,
    /// The MCP tool that asked ("update_summary").
    pub tool: String,
    /// The meeting it changes.
    pub meeting_id: Option<String>,
    /// The tool's arguments, JSON.
    pub payload: String,
}

/// The JSON kept in `summaries.body`.
#[derive(Serialize, Deserialize)]
struct Body {
    summary: Vec<Bullet>,
    decisions: Vec<Bullet>,
    open_questions: Vec<Bullet>,
}

impl Store {
    /// Store a meeting's summary and action items, replacing earlier ones. Action items
    /// that keep their text keep their status and Jira issue.
    pub fn set_summary(
        &mut self,
        meeting_id: &str,
        summary: &Summary,
        items: &[NewActionItem],
    ) -> Result<()> {
        let body = serde_json::to_string(&Body {
            summary: summary.summary.clone(),
            decisions: summary.decisions.clone(),
            open_questions: summary.open_questions.clone(),
        })?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO summaries (meeting_id, template, cli, generated_at, body)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (meeting_id) DO UPDATE SET
                 template = excluded.template, cli = excluded.cli,
                 generated_at = excluded.generated_at, body = excluded.body",
            params![
                meeting_id,
                summary.template,
                summary.cli,
                summary.generated_at,
                body
            ],
        )?;
        type Kept = (String, String, Option<String>, Option<String>);
        let kept: Vec<Kept> = {
            let mut stmt = tx.prepare(
                "SELECT text, status, jira_key, jira_url FROM action_items WHERE meeting_id = ?1",
            )?;
            let rows = stmt.query_map([meeting_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        tx.execute(
            "DELETE FROM action_items WHERE meeting_id = ?1",
            [meeting_id],
        )?;
        for (i, item) in items.iter().enumerate() {
            let prev = kept.iter().find(|(t, ..)| *t == item.text);
            let status = prev.map_or("open", |(_, s, ..)| s.as_str());
            let (jira_key, jira_url) =
                prev.map_or((None, None), |(_, _, k, u)| (k.as_deref(), u.as_deref()));
            tx.execute(
                "INSERT INTO action_items
                 (meeting_id, position, text, owner_id, owner_name, due, at_ms, status,
                  jira_key, jira_url)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    meeting_id,
                    i as i64,
                    item.text,
                    item.owner_id,
                    item.owner_name,
                    item.due,
                    item.at_ms,
                    status,
                    jira_key,
                    jira_url
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// A meeting's summary, if it has one.
    pub fn summary(&self, meeting_id: &str) -> Result<Option<Summary>> {
        let row: Option<(String, String, i64, String)> = self
            .conn
            .query_row(
                "SELECT template, cli, generated_at, body FROM summaries WHERE meeting_id = ?1",
                [meeting_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((template, cli, generated_at, body)) = row else {
            return Ok(None);
        };
        let body: Body = serde_json::from_str(&body)?;
        Ok(Some(Summary {
            template,
            cli,
            generated_at,
            summary: body.summary,
            decisions: body.decisions,
            open_questions: body.open_questions,
        }))
    }

    /// Action items, newest meeting first: one meeting's, or everyone's.
    pub fn action_items(&self, meeting_id: Option<&str>) -> Result<Vec<ActionItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.id, a.meeting_id, m.title, m.started_at, a.text, a.owner_id,
                    a.owner_name, a.due, a.at_ms, a.status, a.jira_key, a.jira_url
             FROM action_items a JOIN meetings m ON m.id = a.meeting_id
             WHERE ?1 IS NULL OR a.meeting_id = ?1
             ORDER BY m.started_at DESC, a.position",
        )?;
        let rows = stmt.query_map([meeting_id], |r| {
            let status: String = r.get(9)?;
            Ok(ActionItem {
                id: r.get(0)?,
                meeting_id: r.get(1)?,
                meeting_title: r.get(2)?,
                meeting_started_at: r.get(3)?,
                text: r.get(4)?,
                owner_id: r.get(5)?,
                owner_name: r.get(6)?,
                due: r.get(7)?,
                at_ms: r.get(8)?,
                status: ActionStatus::parse(&status),
                jira_key: r.get(10)?,
                jira_url: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Change an action item's status.
    pub fn set_action_status(&self, id: i64, status: ActionStatus) -> Result<()> {
        self.conn.execute(
            "UPDATE action_items SET status = ?2 WHERE id = ?1",
            params![id, status.as_str()],
        )?;
        Ok(())
    }

    /// Record the Jira issue an action item became, and mark it [`ActionStatus::InJira`].
    pub fn set_action_jira(&self, id: i64, key: &str, url: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE action_items SET jira_key = ?2, jira_url = ?3, status = ?4 WHERE id = ?1",
            params![id, key, url, ActionStatus::InJira.as_str()],
        )?;
        Ok(())
    }

    /// Queue a change for the user to approve; returns its id.
    pub fn add_pending(&self, tool: &str, meeting_id: Option<&str>, payload: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO pending_actions (tool, meeting_id, payload, created_at)
             VALUES (?1, ?2, ?3, strftime('%s', 'now'))",
            params![tool, meeting_id, payload],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Changes waiting for approval, oldest first.
    pub fn pending(&self) -> Result<Vec<PendingAction>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, tool, meeting_id, payload FROM pending_actions ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(PendingAction {
                id: r.get(0)?,
                tool: r.get(1)?,
                meeting_id: r.get(2)?,
                payload: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Drop a pending change (approved and applied, or denied).
    pub fn remove_pending(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM pending_actions WHERE id = ?1", [id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MeetingStatus, NewMeeting};

    fn store() -> Store {
        let mut s = Store::open_in_memory().unwrap();
        s.insert_meeting(&NewMeeting {
            id: "m".into(),
            title: "Planning".into(),
            started_at: 10,
            duration_secs: 60,
            source_app: "Teams".into(),
            status: MeetingStatus::Ready,
            attendees: vec![],
        })
        .unwrap();
        s
    }

    fn item(text: &str) -> NewActionItem {
        NewActionItem {
            text: text.into(),
            owner_id: None,
            owner_name: Some("Priya".into()),
            due: None,
            at_ms: 5_000,
        }
    }

    #[test]
    fn summaries_and_action_items_round_trip() {
        let mut s = store();
        assert_eq!(s.summary("m").unwrap(), None);
        let sum = Summary {
            template: "planning".into(),
            cli: "Claude Code".into(),
            generated_at: 99,
            summary: vec![Bullet {
                text: "We met.".into(),
                at_ms: 1_000,
            }],
            decisions: vec![],
            open_questions: vec![],
        };
        s.set_summary("m", &sum, &[item("a"), item("b")]).unwrap();
        assert_eq!(s.summary("m").unwrap(), Some(sum.clone()));
        let items = s.action_items(Some("m")).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].meeting_title, "Planning");
        s.set_action_status(items[1].id, ActionStatus::Done)
            .unwrap();
        // A re-run keeps the status of items it keeps.
        s.set_summary("m", &sum, &[item("b"), item("c")]).unwrap();
        let items = s.action_items(None).unwrap();
        let st: Vec<(&str, ActionStatus)> =
            items.iter().map(|i| (i.text.as_str(), i.status)).collect();
        assert_eq!(st, [("b", ActionStatus::Done), ("c", ActionStatus::Open)]);
    }

    #[test]
    fn jira_links_survive_a_rerun() {
        let mut s = store();
        let sum = Summary::default();
        s.set_summary("m", &sum, &[item("a"), item("b")]).unwrap();
        let items = s.action_items(Some("m")).unwrap();
        assert_eq!(items[0].jira_key, None);
        s.set_action_jira(items[0].id, "OPS-7", "https://x.atlassian.net/browse/OPS-7")
            .unwrap();
        let a = &s.action_items(Some("m")).unwrap()[0];
        assert_eq!(a.status, ActionStatus::InJira);
        assert_eq!(a.jira_key.as_deref(), Some("OPS-7"));
        // Same text keeps the issue; new text starts without one.
        s.set_summary("m", &sum, &[item("a"), item("c")]).unwrap();
        let items = s.action_items(Some("m")).unwrap();
        assert_eq!(items[0].status, ActionStatus::InJira);
        assert_eq!(
            items[0].jira_url.as_deref(),
            Some("https://x.atlassian.net/browse/OPS-7")
        );
        assert_eq!(items[1].jira_key, None);
    }

    #[test]
    fn pending_actions_queue() {
        let s = store();
        let id = s.add_pending("update_summary", Some("m"), "{}").unwrap();
        assert_eq!(s.pending().unwrap()[0].tool, "update_summary");
        s.remove_pending(id).unwrap();
        assert!(s.pending().unwrap().is_empty());
    }
}
