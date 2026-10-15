//! switchscribe's MCP server (`switchscribe mcp`): JSON-RPC 2.0 over stdio, one message
//! per line, as the Model Context Protocol's stdio transport specifies. It reads the
//! store and, in `--ask` mode, turns writes into proposals the user approves in the app.

use std::io::{BufRead, Write};

use serde_json::{Value, json};
use switchscribe_store::{Bullet, Meeting, NewActionItem, SearchFilter, Store, Summary};

use crate::summarize::{clock, match_person, parse_clock};
use crate::{Error, MCP_SERVER_NAME, Result};

/// The protocol version answered when the client asks for one we do not know.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// How writes are handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Writes apply (the user's own CLI session, which asks before each tool call).
    Direct,
    /// Writes wait for approval in the app (the Ask panel's headless runs).
    Ask,
}

/// Serve requests from `input` until it closes.
pub fn serve(
    store: &mut Store,
    mode: Mode,
    input: impl BufRead,
    mut output: impl Write,
) -> Result<()> {
    for line in input.lines() {
        let line = line.map_err(|e| Error::Io("stdin".into(), e))?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(store, mode, &msg),
            Err(e) => Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(reply) = reply {
            writeln!(output, "{reply}").map_err(|e| Error::Io("stdout".into(), e))?;
            output.flush().map_err(|e| Error::Io("stdout".into(), e))?;
        }
    }
    Ok(())
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Answer one message; notifications get no answer.
pub fn handle(store: &mut Store, mode: Mode, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    Some(match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION);
            ok(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": MCP_SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "Meetings recorded and transcribed by switchscribe. Cite \
                        moments as meeting_id and mm:ss."
                }),
            )
        }
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": tools() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let (text, is_error) = match call(store, mode, name, &args) {
                Ok(v) => (
                    match v {
                        Value::String(s) => s,
                        v => serde_json::to_string_pretty(&v).unwrap_or_default(),
                    },
                    false,
                ),
                Err(e) => (e.to_string(), true),
            };
            ok(
                id,
                json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }),
            )
        }
        _ => error(id, -32601, &format!("unknown method {method}")),
    })
}

fn tools() -> Value {
    let bullets = json!({
        "type": "array",
        "items": { "type": "object", "properties": {
            "text": { "type": "string" }, "at": { "type": "string", "description": "mm:ss" }
        }, "required": ["text", "at"] }
    });
    json!([
        {
            "name": "list_meetings",
            "description": "Recorded meetings, newest first: id, title, date, length, attendees.",
            "inputSchema": { "type": "object", "properties": {
                "query": { "type": "string", "description": "Words in the title" },
                "since_days": { "type": "integer", "description": "Only the last N days" },
                "limit": { "type": "integer", "default": 20 }
            } }
        },
        {
            "name": "get_transcript",
            "description": "A meeting's transcript, one line per stretch of speech: [mm:ss] Speaker: words.",
            "inputSchema": { "type": "object", "properties": {
                "meeting_id": { "type": "string" }
            }, "required": ["meeting_id"] }
        },
        {
            "name": "get_summary",
            "description": "A meeting's summary, decisions and open questions, each with its moment.",
            "inputSchema": { "type": "object", "properties": {
                "meeting_id": { "type": "string" }
            }, "required": ["meeting_id"] }
        },
        {
            "name": "search_meetings",
            "description": "Full-text search over every transcript; returns matching lines with their meeting and moment.",
            "inputSchema": { "type": "object", "properties": {
                "query": { "type": "string" },
                "person": { "type": "string", "description": "Only lines this person said" },
                "since_days": { "type": "integer" },
                "meeting_id": { "type": "string" },
                "limit": { "type": "integer", "default": 30 }
            }, "required": ["query"] }
        },
        {
            "name": "get_action_items",
            "description": "Action items with owner, due date, status and the moment they came up.",
            "inputSchema": { "type": "object", "properties": {
                "meeting_id": { "type": "string" },
                "status": { "type": "string", "enum": ["open", "in-jira", "done", "all"], "default": "open" }
            } }
        },
        {
            "name": "update_summary",
            "description": "Replace parts of a meeting's summary. Sections left out stay as they are.",
            "inputSchema": { "type": "object", "properties": {
                "meeting_id": { "type": "string" },
                "summary": bullets,
                "decisions": bullets,
                "open_questions": bullets,
                "action_items": { "type": "array", "items": { "type": "object", "properties": {
                    "text": { "type": "string" }, "owner": { "type": "string" },
                    "due": { "type": "string" }, "at": { "type": "string" }
                }, "required": ["text", "at"] } }
            }, "required": ["meeting_id"] }
        },
        {
            "name": "create_jira_issue",
            "description": "Propose a Jira issue from a meeting; the user approves it in switchscribe.",
            "inputSchema": { "type": "object", "properties": {
                "meeting_id": { "type": "string" },
                "summary": { "type": "string" },
                "description": { "type": "string" },
                "at": { "type": "string", "description": "mm:ss" }
            }, "required": ["meeting_id", "summary"] }
        }
    ])
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    str_arg(args, key).ok_or_else(|| Error::Tool(format!("{key} is required")))
}

fn since(args: &Value) -> Option<i64> {
    args.get("since_days")
        .and_then(Value::as_i64)
        .map(|d| chrono::Utc::now().timestamp() - d.max(0) * 86_400)
}

fn local_time(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn meeting(store: &Store, id: &str) -> Result<Meeting> {
    store
        .meeting(id)?
        .ok_or_else(|| Error::Tool(format!("no meeting {id}")))
}

fn call(store: &mut Store, mode: Mode, name: &str, args: &Value) -> Result<Value> {
    match name {
        "list_meetings" => {
            let query = str_arg(args, "query").map(str::to_lowercase);
            let since = since(args);
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
            let list: Vec<Value> = store
                .meetings()?
                .into_iter()
                .filter(|m| {
                    query
                        .as_ref()
                        .is_none_or(|q| m.title.to_lowercase().contains(q))
                })
                .filter(|m| since.is_none_or(|s| m.started_at >= s))
                .take(limit)
                .map(|m| {
                    json!({
                        "meeting_id": m.id,
                        "title": m.title,
                        "started": local_time(m.started_at),
                        "minutes": m.duration_secs.div_ceil(60),
                        "attendees": m.attendees.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
                        "status": m.status.as_str(),
                    })
                })
                .collect();
            Ok(Value::Array(list))
        }
        "get_transcript" => {
            let id = required(args, "meeting_id")?;
            let m = meeting(store, id)?;
            let people = store.people()?;
            let segments = store.segments(id)?;
            let names: Vec<&str> = m.attendees.iter().map(|a| a.name.as_str()).collect();
            Ok(Value::String(format!(
                "meeting_id: {}\ntitle: {}\nstarted: {}\nattendees: {}\n\n{}",
                m.id,
                m.title,
                local_time(m.started_at),
                names.join(", "),
                crate::summarize::transcript_text(&segments, &people)
            )))
        }
        "get_summary" => {
            let id = required(args, "meeting_id")?;
            let s = store
                .summary(id)?
                .ok_or_else(|| Error::Tool(format!("{id} has no summary yet")))?;
            let list = |b: &[Bullet]| {
                b.iter()
                    .map(|b| json!({ "text": b.text, "at": clock(b.at_ms) }))
                    .collect::<Vec<_>>()
            };
            Ok(json!({
                "meeting_id": id,
                "summary": list(&s.summary),
                "decisions": list(&s.decisions),
                "open_questions": list(&s.open_questions),
            }))
        }
        "search_meetings" => {
            let query = required(args, "query")?;
            let people = store.people()?;
            let person = match str_arg(args, "person") {
                Some(name) => Some(
                    match_person(name, &people)
                        .ok_or_else(|| Error::Tool(format!("nobody called {name}")))?,
                ),
                None => None,
            };
            let filter = SearchFilter {
                person,
                since: since(args),
                meeting: str_arg(args, "meeting_id").map(str::to_owned),
            };
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(30) as usize;
            let hits: Vec<Value> = store
                .search_with(query, &filter, limit)?
                .into_iter()
                .map(|h| {
                    let speaker = h
                        .person_id
                        .and_then(|id| people.iter().find(|p| p.id == id))
                        .map_or(h.speaker_label.clone(), |p| p.name.clone());
                    json!({
                        "meeting_id": h.meeting_id,
                        "title": h.meeting_title,
                        "at": clock(h.start_ms),
                        "speaker": speaker,
                        "line": h.snippet.replace([switchscribe_store::db::MATCH_START, switchscribe_store::db::MATCH_END], ""),
                    })
                })
                .collect();
            Ok(Value::Array(hits))
        }
        "get_action_items" => {
            let status = str_arg(args, "status").unwrap_or("open");
            let people = store.people()?;
            let items: Vec<Value> = store
                .action_items(str_arg(args, "meeting_id"))?
                .into_iter()
                .filter(|i| status == "all" || i.status.as_str() == status)
                .map(|i| {
                    let owner = i
                        .owner_id
                        .and_then(|id| people.iter().find(|p| p.id == id))
                        .map(|p| p.name.clone())
                        .or(i.owner_name);
                    json!({
                        "meeting_id": i.meeting_id,
                        "meeting": i.meeting_title,
                        "text": i.text,
                        "owner": owner,
                        "due": i.due,
                        "status": i.status.as_str(),
                        "at": clock(i.at_ms),
                    })
                })
                .collect();
            Ok(Value::Array(items))
        }
        "update_summary" => {
            let id = required(args, "meeting_id")?;
            meeting(store, id)?;
            match mode {
                Mode::Ask => {
                    store.add_pending(name, Some(id), &args.to_string())?;
                    Ok(Value::String(
                        "Proposed. The user approves or denies it in switchscribe.".into(),
                    ))
                }
                Mode::Direct => {
                    apply_update_summary(store, args)?;
                    Ok(Value::String("Updated.".into()))
                }
            }
        }
        "create_jira_issue" => {
            let id = required(args, "meeting_id")?;
            required(args, "summary")?;
            meeting(store, id)?;
            store.add_pending(name, Some(id), &args.to_string())?;
            Ok(Value::String(
                "Proposed. The user approves it in switchscribe before anything reaches Jira."
                    .into(),
            ))
        }
        _ => Err(Error::Tool(format!("unknown tool {name}"))),
    }
}

/// Apply an `update_summary` call: replace the sections it gives, keep the rest.
pub fn apply_update_summary(store: &mut Store, args: &Value) -> Result<()> {
    let id = required(args, "meeting_id")?;
    let m = meeting(store, id)?;
    let duration = i64::from(m.duration_secs) * 1000;
    let people = store.people()?;
    let mut summary = store.summary(id)?.unwrap_or_else(|| Summary {
        template: "edited".into(),
        cli: "agent".into(),
        ..Summary::default()
    });
    summary.generated_at = chrono::Utc::now().timestamp();
    let at = |v: &Value| {
        v.get("at")
            .and_then(Value::as_str)
            .and_then(parse_clock)
            .unwrap_or(0)
            .clamp(0, duration.max(0))
    };
    let bullets = |key: &str| -> Option<Vec<Bullet>> {
        args.get(key).and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(|v| {
                    let text = v.get("text").and_then(Value::as_str)?.trim();
                    (!text.is_empty()).then(|| Bullet {
                        text: text.to_owned(),
                        at_ms: at(v),
                    })
                })
                .collect()
        })
    };
    if let Some(b) = bullets("summary") {
        summary.summary = b;
    }
    if let Some(b) = bullets("decisions") {
        summary.decisions = b;
    }
    if let Some(b) = bullets("open_questions") {
        summary.open_questions = b;
    }
    let items: Vec<NewActionItem> = match args.get("action_items").and_then(Value::as_array) {
        Some(a) => a
            .iter()
            .filter_map(|v| {
                let text = v.get("text").and_then(Value::as_str)?.trim().to_owned();
                let owner_name = str_arg(v, "owner").map(str::to_owned);
                Some(NewActionItem {
                    owner_id: owner_name.as_deref().and_then(|n| match_person(n, &people)),
                    owner_name,
                    due: str_arg(v, "due").map(str::to_owned),
                    at_ms: at(v),
                    text,
                })
            })
            .filter(|i| !i.text.is_empty())
            .collect(),
        None => store
            .action_items(Some(id))?
            .into_iter()
            .map(|i| NewActionItem {
                text: i.text,
                owner_id: i.owner_id,
                owner_name: i.owner_name,
                due: i.due,
                at_ms: i.at_ms,
            })
            .collect(),
    };
    store.set_summary(id, &summary, &items)?;
    Ok(())
}

/// Apply a change the user approved. Returns what happened, in a sentence.
pub fn apply_pending(
    store: &mut Store,
    action: &switchscribe_store::PendingAction,
) -> Result<String> {
    match action.tool.as_str() {
        "update_summary" => {
            let args: Value = serde_json::from_str(&action.payload)?;
            apply_update_summary(store, &args)?;
            Ok("Done: the summary is updated.".into())
        }
        "create_jira_issue" => Ok(
            "Jira is not connected yet (it arrives with integrations), so nothing was created."
                .into(),
        ),
        t => Err(Error::Tool(format!("switchscribe cannot apply {t}"))),
    }
}

/// The lines a pending change would write, for the approve/deny card to show.
pub fn pending_details(tool: &str, payload: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let mut out = Vec::new();
    if tool == "create_jira_issue" {
        if let Some(d) = str_arg(&v, "description") {
            out.push(d.to_owned());
        }
        return out;
    }
    for key in ["summary", "decisions", "open_questions", "action_items"] {
        for item in v.get(key).and_then(Value::as_array).into_iter().flatten() {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                let at = item.get("at").and_then(Value::as_str).unwrap_or("");
                out.push(if at.is_empty() {
                    text.to_owned()
                } else {
                    format!("{at} · {text}")
                });
            }
        }
    }
    out
}

/// A pending change in a sentence, for the approve/deny card.
pub fn describe_pending(tool: &str, payload: &str) -> String {
    let v: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    match tool {
        "create_jira_issue" => format!(
            "Create a Jira issue: {}",
            str_arg(&v, "summary").unwrap_or("(untitled)")
        ),
        "update_summary" => {
            let parts: Vec<&str> = ["summary", "decisions", "open_questions", "action_items"]
                .into_iter()
                .filter(|k| v.get(*k).is_some())
                .map(|k| match k {
                    "open_questions" => "open questions",
                    "action_items" => "action items",
                    k => k,
                })
                .collect();
            format!(
                "Update the {}",
                if parts.is_empty() {
                    "summary".to_owned()
                } else {
                    parts.join(", ")
                }
            )
        }
        t => format!("Run {t}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchscribe_store::{MeetingStatus, NewMeeting, NewSegment};

    #[test]
    fn pending_changes_read_as_lines() {
        let payload = r#"{"meeting_id": "m", "open_questions": [
            {"text": "Who approves?", "at": "05:02"}, {"text": "No time"}]}"#;
        assert_eq!(
            describe_pending("update_summary", payload),
            "Update the open questions"
        );
        assert_eq!(
            pending_details("update_summary", payload),
            ["05:02 · Who approves?", "No time"]
        );
        let jira = r#"{"summary": "Runbook", "description": "Write it."}"#;
        assert_eq!(pending_details("create_jira_issue", jira), ["Write it."]);
    }

    fn store() -> Store {
        let mut s = Store::open_in_memory().unwrap();
        let priya = s.add_person("Priya Nair", Some(1), false).unwrap();
        s.insert_meeting(&NewMeeting {
            id: "aks".into(),
            title: "AKS planning".into(),
            started_at: chrono::Utc::now().timestamp() - 3600,
            duration_secs: 600,
            source_app: "Teams".into(),
            status: MeetingStatus::Ready,
            attendees: vec![priya],
        })
        .unwrap();
        s.add_segments(
            "aks",
            &[NewSegment {
                start_ms: 31_000,
                end_ms: 35_000,
                person_id: Some(priya),
                speaker_label: String::new(),
                text: "Two of three pools are on the new cluster.".into(),
                low_confidence: false,
            }],
        )
        .unwrap();
        s
    }

    fn rpc(store: &mut Store, mode: Mode, method: &str, params: Value) -> Value {
        handle(
            store,
            mode,
            &json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }),
        )
        .unwrap()
    }

    fn tool(store: &mut Store, mode: Mode, name: &str, args: Value) -> (String, bool) {
        let r = rpc(
            store,
            mode,
            "tools/call",
            json!({ "name": name, "arguments": args }),
        );
        let result = &r["result"];
        (
            result["content"][0]["text"].as_str().unwrap().to_owned(),
            result["isError"].as_bool().unwrap(),
        )
    }

    #[test]
    fn speaks_the_protocol() {
        let mut s = store();
        let init = rpc(
            &mut s,
            Mode::Direct,
            "initialize",
            json!({ "protocolVersion": "2025-03-26" }),
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "switchscribe");
        let list = rpc(&mut s, Mode::Direct, "tools/list", json!({}));
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, crate::MCP_TOOLS);
        // Notifications get no answer; unknown methods get an error.
        assert!(
            handle(
                &mut s,
                Mode::Direct,
                &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
            )
            .is_none()
        );
        assert_eq!(
            rpc(&mut s, Mode::Direct, "nope", json!({}))["error"]["code"],
            -32601
        );
    }

    #[test]
    fn serves_over_lines() {
        let mut s = store();
        let input = "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n\nnot json\n";
        let mut out = Vec::new();
        serve(&mut s, Mode::Direct, input.as_bytes(), &mut out).unwrap();
        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["id"], 7);
        assert_eq!(lines[1]["error"]["code"], -32700);
    }

    #[test]
    fn reads_meetings() {
        let mut s = store();
        let (list, _) = tool(
            &mut s,
            Mode::Direct,
            "list_meetings",
            json!({ "since_days": 1 }),
        );
        assert!(list.contains("\"meeting_id\": \"aks\""));
        let (t, _) = tool(
            &mut s,
            Mode::Direct,
            "get_transcript",
            json!({ "meeting_id": "aks" }),
        );
        assert!(t.contains("[00:31] Priya Nair: Two of three pools"));
        let (hits, _) = tool(
            &mut s,
            Mode::Direct,
            "search_meetings",
            json!({ "query": "pools", "person": "Priya" }),
        );
        assert!(hits.contains("\"at\": \"00:31\""));
        let (err, is_error) = tool(&mut s, Mode::Direct, "get_transcript", json!({}));
        assert!(is_error && err.contains("meeting_id"));
    }

    #[test]
    fn writes_apply_directly_or_wait_for_approval() {
        let mut s = store();
        let args = json!({
            "meeting_id": "aks",
            "decisions": [{ "text": "Cut over Oct 22.", "at": "03:56" }],
            "action_items": [{ "text": "Write the runbook", "owner": "Priya", "at": "06:06" }]
        });
        let (_, is_error) = tool(&mut s, Mode::Ask, "update_summary", args.clone());
        assert!(!is_error);
        assert!(s.summary("aks").unwrap().is_none());
        let pending = s.pending().unwrap();
        assert_eq!(
            describe_pending(&pending[0].tool, &pending[0].payload),
            "Update the decisions, action items"
        );

        tool(&mut s, Mode::Direct, "update_summary", args);
        let sum = s.summary("aks").unwrap().unwrap();
        assert_eq!(sum.decisions[0].at_ms, 236_000);
        let items = s.action_items(Some("aks")).unwrap();
        assert_eq!(items[0].owner_name.as_deref(), Some("Priya"));
        assert!(items[0].owner_id.is_some());
        let (open, _) = tool(&mut s, Mode::Direct, "get_action_items", json!({}));
        assert!(open.contains("Priya Nair"));
    }
}
