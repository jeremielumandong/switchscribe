//! M5 integrations in the app: the Export menu, Send to Jira, the calendar that names
//! meetings, and Settings → Integrations. Network calls and file writes run on the
//! background executor; tokens live sealed in the store, never in the settings file.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement as _,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use switchscribe_integrations::ExportFormat;
use switchscribe_integrations::calendar::{
    self, CalendarEvent, DeviceCode, GraphAuth, Poll, Tokens,
};
use switchscribe_integrations::export::{MeetingExport, export, file_name};
use switchscribe_integrations::jira::{JiraClient, JiraConfig, NewIssue};
use switchscribe_store::{ActionItem, ActionStatus, Store};

use crate::appearance::{rpx, ts};
use crate::recording::{Banner, NOTES_FILE};
use crate::theme::Palette;
use crate::ui::{self, Kind};
use crate::workspace::{Screen, Workspace};

/// The sealed secret holding the Jira API token.
const JIRA_TOKEN: &str = "jira_token";
/// The sealed secret holding the Microsoft Graph tokens (JSON).
const GRAPH_TOKENS: &str = "graph_tokens";
/// How often the calendar is read again.
const CALENDAR_EVERY: Duration = Duration::from_secs(10 * 60);

/// A neutral strip above the main pane: something finished.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    /// What happened.
    pub text: String,
    /// A file to show in its folder.
    pub reveal: Option<PathBuf>,
    /// A web page to open.
    pub url: Option<(String, String)>,
}

impl Notice {
    /// A notice with no link.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            reveal: None,
            url: None,
        }
    }
}

/// Where Jira stands in Settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JiraState {
    /// Not tried.
    Unknown,
    /// Testing the connection.
    Testing,
    /// Connected as this person.
    Connected(String),
    /// What went wrong.
    Failed(String),
}

/// Integration state and the Settings fields.
pub struct Integrations {
    /// The Export menu is open.
    pub export_open: bool,
    /// The neutral strip.
    pub notice: Option<Notice>,
    jira_site: Entity<InputState>,
    jira_email: Entity<InputState>,
    jira_token: Entity<InputState>,
    jira_project: Entity<InputState>,
    jira_type: Entity<InputState>,
    /// A Jira API token is saved.
    pub jira_token_saved: bool,
    /// The last connection test.
    pub jira: JiraState,
    /// Action items being sent.
    pub sending: HashSet<i64>,
    /// Action items ticked on the Action items screen.
    pub checked: HashSet<i64>,
    graph_client: Entity<InputState>,
    graph_tenant: Entity<InputState>,
    ics_url: Entity<InputState>,
    /// A Microsoft sign-in waiting for the user's code.
    pub device: Option<DeviceCode>,
    /// Signed in to Microsoft (tokens saved).
    pub graph_signed_in: bool,
    /// Why the calendar could not be read.
    pub calendar_error: Option<String>,
    /// Events from yesterday to tomorrow.
    pub events: Vec<CalendarEvent>,
    calendar_polling: bool,
}

impl Integrations {
    /// The fields, filled from settings.
    pub fn new(
        s: &switchscribe_store::IntegrationSettings,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let mut field = |placeholder: &'static str, value: Option<&str>, masked: bool| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value.unwrap_or_default().to_owned())
                    .masked(masked)
            })
        };
        Self {
            export_open: false,
            notice: None,
            jira_site: field(
                "https://example.atlassian.net",
                s.jira_site.as_deref(),
                false,
            ),
            jira_email: field("you@example.com", s.jira_email.as_deref(), false),
            jira_token: field("API token", None, true),
            jira_project: field("Project key, e.g. PLAT", s.jira_project.as_deref(), false),
            jira_type: field("Task", Some(&s.jira_issue_type), false),
            jira_token_saved: false,
            jira: JiraState::Unknown,
            sending: HashSet::new(),
            checked: HashSet::new(),
            graph_client: field(
                "Application (client) ID",
                s.graph_client_id.as_deref(),
                false,
            ),
            graph_tenant: field("organizations", Some(&s.graph_tenant), false),
            ics_url: field(
                "https://outlook.office365.com/…/calendar.ics",
                s.ics_url.as_deref(),
                false,
            ),
            device: None,
            graph_signed_in: false,
            calendar_error: None,
            events: Vec::new(),
            calendar_polling: false,
        }
    }
}

fn value(input: &Entity<InputState>, cx: &Context<Workspace>) -> Option<String> {
    let v = input.read(cx).value().trim().to_owned();
    (!v.is_empty()).then_some(v)
}

fn lock(store: &crate::workspace::SharedStore) -> Result<std::sync::MutexGuard<'_, Store>, String> {
    store
        .lock()
        .map_err(|_| "the index is unavailable".to_owned())
}

/// The demo's calendar: the meeting the fake Teams call is.
fn demo_events() -> Vec<CalendarEvent> {
    let now = chrono::Local::now().timestamp();
    let who = |n: &str, e: &str| calendar::Attendee {
        name: n.into(),
        email: Some(e.into()),
    };
    vec![CalendarEvent {
        subject: "Cerberus rollout sync".into(),
        start: now - 120,
        end: now + 1680,
        attendees: vec![
            who("Priya Nair", "priya@example.com"),
            who("Marcus Chen", "marcus@example.com"),
            who("Dana Ortiz", "dana@example.com"),
            who("Lena Park", "lena@example.com"),
        ],
        organizer: Some("Priya Nair".into()),
        online_meeting: true,
    }]
}

impl Workspace {
    // ----- notice -------------------------------------------------------------------

    pub(crate) fn dismiss_notice(&mut self, cx: &mut Context<Self>) {
        self.integ.notice = None;
        cx.notify();
    }

    // ----- export -------------------------------------------------------------------

    pub(crate) fn toggle_export_menu(&mut self, cx: &mut Context<Self>) {
        self.integ.export_open = !self.integ.export_open;
        cx.notify();
    }

    /// Ask where to save, then write the selected meeting in `format`.
    pub(crate) fn export_selected(&mut self, format: ExportFormat, cx: &mut Context<Self>) {
        self.integ.export_open = false;
        let Some(meeting) = self.selected().cloned() else {
            return;
        };
        let name = file_name(&meeting, format);
        let dir = self.export_dir();
        let prompt = cx.prompt_for_new_path(&dir, Some(&name));
        let store = self.store.clone();
        let meeting_dir = self.meeting_dir(&meeting.id);
        cx.spawn(async move |this, cx| {
            let path = match prompt.await {
                Ok(Ok(Some(p))) => p,
                Ok(Ok(None)) => return,
                // No file picker (a Linux desktop without a portal): save to the default
                // folder instead.
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "no file picker; saving to the default folder");
                    dir.join(&name)
                }
                Err(_) => {
                    tracing::warn!("no file picker; saving to the default folder");
                    dir.join(&name)
                }
            };
            let task = cx.background_executor().spawn(async move {
                let notes =
                    std::fs::read_to_string(meeting_dir.join(NOTES_FILE)).unwrap_or_default();
                let bytes = {
                    let s = lock(&store)?;
                    let segments = s.segments(&meeting.id).map_err(|e| e.to_string())?;
                    let people = s.people().map_err(|e| e.to_string())?;
                    let summary = s.summary(&meeting.id).map_err(|e| e.to_string())?;
                    let items = s
                        .action_items(Some(&meeting.id))
                        .map_err(|e| e.to_string())?;
                    export(
                        &MeetingExport {
                            meeting: &meeting,
                            segments: &segments,
                            people: &people,
                            summary: summary.as_ref(),
                            action_items: &items,
                            notes: &notes,
                        },
                        format,
                    )
                    .map_err(|e| e.to_string())?
                };
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
                Ok::<_, String>(path)
            });
            let result = task.await;
            let _ = this.update(cx, |ws, cx| {
                match result {
                    Ok(path) => {
                        ws.integ.notice = Some(Notice {
                            text: format!("Exported {}.", path.display()),
                            reveal: Some(path),
                            url: None,
                        });
                    }
                    Err(e) => ws
                        .banners
                        .push(Banner::plain(format!("Could not export: {e}"))),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Where exports go by default: Downloads, else the data folder's `exports`.
    fn export_dir(&self) -> PathBuf {
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
        match home.map(PathBuf::from).map(|h| h.join("Downloads")) {
            Some(d) if d.is_dir() => d,
            _ => self.paths.data.join("exports"),
        }
    }

    // ----- Jira ---------------------------------------------------------------------

    fn jira_config(&self) -> Option<(String, String, String, String)> {
        let s = &self.settings.integrations;
        Some((
            s.jira_site.clone()?,
            s.jira_email.clone()?,
            s.jira_project.clone()?,
            s.jira_issue_type.clone(),
        ))
    }

    /// Jira is set up enough to send.
    pub(crate) fn jira_ready(&self) -> bool {
        self.jira_config().is_some() && self.integ.jira_token_saved
    }

    /// Read which secrets exist (off the UI thread).
    pub(crate) fn load_integrations(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let s = lock(&store)?;
            let jira = s.secret(JIRA_TOKEN).map_err(|e| e.to_string())?.is_some();
            let graph = s.secret(GRAPH_TOKENS).map_err(|e| e.to_string())?.is_some();
            Ok::<_, String>((jira, graph))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |ws, cx| {
                if let Ok((jira, graph)) = result {
                    ws.integ.jira_token_saved = jira;
                    ws.integ.graph_signed_in = graph;
                }
                ws.refresh_calendar(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Settings → Jira's Save: keep the fields, seal the token, test the connection.
    fn save_jira(&mut self, cx: &mut Context<Self>) {
        let i = &self.integ;
        let s = &mut self.settings.integrations;
        s.jira_site = value(&i.jira_site, cx);
        s.jira_email = value(&i.jira_email, cx);
        s.jira_project = value(&i.jira_project, cx).map(|p| p.to_uppercase());
        s.jira_issue_type = value(&i.jira_type, cx).unwrap_or_else(|| "Task".into());
        let token = value(&i.jira_token, cx);
        self.save_settings(cx);
        let Some((site, email, project, issue_type)) = self.jira_config() else {
            self.integ.jira = JiraState::Failed("Fill in the site, email and project.".into());
            cx.notify();
            return;
        };
        self.integ.jira = JiraState::Testing;
        let store = self.store.clone();
        let demo = self.demo;
        let task = cx.background_executor().spawn(async move {
            let token = match token {
                Some(t) => {
                    if !demo {
                        lock(&store)?
                            .set_secret(JIRA_TOKEN, t.as_bytes())
                            .map_err(|e| e.to_string())?;
                    }
                    t
                }
                None => lock(&store)?
                    .secret(JIRA_TOKEN)
                    .map_err(|e| e.to_string())?
                    .and_then(|b| String::from_utf8(b).ok())
                    .ok_or_else(|| "Paste an API token.".to_owned())?,
            };
            let client = JiraClient::new(JiraConfig {
                site,
                email,
                token,
                project,
                issue_type,
            });
            client.myself().map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |ws, cx| {
                ws.integ.jira = match result {
                    Ok(name) => {
                        ws.integ.jira_token_saved = true;
                        JiraState::Connected(name)
                    }
                    Err(e) => JiraState::Failed(e),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn forget_jira(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        cx.background_executor()
            .spawn(async move {
                if let Ok(s) = lock(&store)
                    && let Err(e) = s.remove_secret(JIRA_TOKEN)
                {
                    tracing::warn!(error = %e, "could not remove the Jira token");
                }
            })
            .detach();
        self.integ.jira_token_saved = false;
        self.integ.jira = JiraState::Unknown;
        self.integ
            .jira_token
            .update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    /// The action items with these ids that can still be sent.
    fn sendable(&self, ids: &[i64]) -> Vec<ActionItem> {
        self.agent
            .all_items
            .iter()
            .filter(|a| ids.contains(&a.id))
            .filter(|a| a.jira_key.is_none() && !self.integ.sending.contains(&a.id))
            .cloned()
            .collect()
    }

    /// Create a Jira issue for each of these action items, linking back to its moment.
    pub(crate) fn send_to_jira(&mut self, ids: Vec<i64>, cx: &mut Context<Self>) {
        let items = self.sendable(&ids);
        if items.is_empty() {
            return;
        }
        let Some((site, email, project, issue_type)) =
            self.jira_config().filter(|_| self.jira_ready())
        else {
            self.banners.push(Banner {
                text: "Connect Jira first: add your site, project and API token.".into(),
                action: Some(("Settings", Screen::Settings)),
            });
            cx.notify();
            return;
        };
        for a in &items {
            self.integ.sending.insert(a.id);
            self.integ.checked.remove(&a.id);
        }
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let token = lock(&store)?
                .secret(JIRA_TOKEN)
                .map_err(|e| e.to_string())?
                .and_then(|b| String::from_utf8(b).ok())
                .ok_or_else(|| "the Jira token is missing".to_owned())?;
            let client = JiraClient::new(JiraConfig {
                site,
                email,
                token,
                project,
                issue_type,
            });
            let mut created = Vec::new();
            let mut failed = None;
            for item in &items {
                let mut issue = NewIssue::from_action(item);
                issue.link = Some(crate::instance::link(&item.meeting_id, item.at_ms / 1000));
                issue.quote = quote(&store, item);
                if let Some(owner) = &issue.owner {
                    issue.assignee = client.find_user(owner).ok().flatten();
                }
                match client.create_issue(&issue) {
                    Ok(c) => {
                        lock(&store)?
                            .set_action_jira(item.id, &c.key, &c.url)
                            .map_err(|e| e.to_string())?;
                        created.push(c);
                    }
                    Err(e) => {
                        failed = Some(e.to_string());
                        break;
                    }
                }
            }
            Ok::<_, String>((created, failed))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |ws, cx| {
                for id in &ids {
                    ws.integ.sending.remove(id);
                }
                match result {
                    Ok((created, failed)) => {
                        if !created.is_empty() {
                            let keys: Vec<&str> = created.iter().map(|c| c.key.as_str()).collect();
                            ws.integ.notice = Some(Notice {
                                text: format!("Created {} in Jira.", keys.join(", ")),
                                reveal: None,
                                url: created
                                    .first()
                                    .filter(|_| created.len() == 1)
                                    .map(|c| ("Open".to_owned(), c.url.clone())),
                            });
                        }
                        if let Some(e) = failed {
                            ws.banners
                                .push(Banner::plain(format!("Jira did not create the issue: {e}")));
                        }
                    }
                    Err(e) => ws
                        .banners
                        .push(Banner::plain(format!("Could not reach Jira: {e}"))),
                }
                ws.load_all_actions(cx);
                ws.load_summary(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// An issue the Ask panel's agent proposed and the user approved.
    pub(crate) fn create_proposed_issue(&mut self, payload: String, cx: &mut Context<Self>) {
        let Some((site, email, project, issue_type)) =
            self.jira_config().filter(|_| self.jira_ready())
        else {
            self.ask_say(
                "Jira is not connected, so nothing was created. Add it in Settings → Integrations."
                    .into(),
                true,
                cx,
            );
            return;
        };
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let v: serde_json::Value = serde_json::from_str(&payload).map_err(|e| e.to_string())?;
            let text = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_owned);
            let meeting_id = text("meeting_id").unwrap_or_default();
            let at_ms = text("at")
                .and_then(|a| switchscribe_agent::summarize::parse_clock(&a))
                .unwrap_or(0);
            let (token, meeting) = {
                let s = lock(&store)?;
                let token = s
                    .secret(JIRA_TOKEN)
                    .map_err(|e| e.to_string())?
                    .and_then(|b| String::from_utf8(b).ok())
                    .ok_or_else(|| "the Jira token is missing".to_owned())?;
                (token, s.meeting(&meeting_id).map_err(|e| e.to_string())?)
            };
            let summary = text("summary").unwrap_or_default();
            let issue = NewIssue {
                summary: summary.clone(),
                description_text: text("description").unwrap_or(summary),
                meeting_title: meeting
                    .as_ref()
                    .map(|m| m.title.clone())
                    .unwrap_or_default(),
                meeting_date: calendar::local_day(meeting.as_ref().map_or(0, |m| m.started_at)),
                at_ms,
                link: meeting
                    .as_ref()
                    .map(|m| crate::instance::link(&m.id, at_ms / 1000)),
                ..Default::default()
            };
            let client = JiraClient::new(JiraConfig {
                site,
                email,
                token,
                project,
                issue_type,
            });
            client.create_issue(&issue).map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |ws, cx| match result {
                Ok(c) => ws.ask_say(format!("Created {} in Jira: {}", c.key, c.url), false, cx),
                Err(e) => ws.ask_say(format!("Jira did not create the issue: {e}"), true, cx),
            });
        })
        .detach();
    }

    pub(crate) fn toggle_checked(&mut self, id: i64, cx: &mut Context<Self>) {
        if !self.integ.checked.remove(&id) {
            self.integ.checked.insert(id);
        }
        cx.notify();
    }

    // ----- calendar -----------------------------------------------------------------

    /// The calendar event happening now, if any.
    pub(crate) fn current_event(&self) -> Option<CalendarEvent> {
        calendar::event_at(&self.integ.events, chrono::Local::now().timestamp()).cloned()
    }

    /// Read the calendar now and every ten minutes.
    pub(crate) fn refresh_calendar(&mut self, cx: &mut Context<Self>) {
        if self.demo {
            self.integ.events = demo_events();
            return;
        }
        let s = &self.settings.integrations;
        let graph = s
            .graph_client_id
            .clone()
            .filter(|_| self.integ.graph_signed_in)
            .map(|id| GraphAuth {
                client_id: id,
                tenant: s.graph_tenant.clone(),
            });
        let ics = s.ics_url.clone();
        if graph.is_none() && ics.is_none() {
            self.integ.events.clear();
            return;
        }
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let now = chrono::Local::now().timestamp();
            let (from, to) = (now - 12 * 3600, now + 36 * 3600);
            if let Some(auth) = graph {
                let saved = lock(&store)?
                    .secret(GRAPH_TOKENS)
                    .map_err(|e| e.to_string())?
                    .and_then(|b| serde_json::from_slice::<Tokens>(&b).ok())
                    .ok_or_else(|| "sign in to Microsoft again".to_owned())?;
                let tokens = if saved.expired(now) {
                    let mut fresh = calendar::refresh(&auth, &saved.refresh_token)
                        .map_err(|e| e.to_string())?;
                    fresh.account = fresh.account.or(saved.account);
                    if let Ok(json) = serde_json::to_vec(&fresh) {
                        lock(&store)?
                            .set_secret(GRAPH_TOKENS, &json)
                            .map_err(|e| e.to_string())?;
                    }
                    fresh
                } else {
                    saved
                };
                return calendar::calendar_view(&tokens.access_token, from, to)
                    .map_err(|e| e.to_string());
            }
            let url = ics.unwrap_or_default();
            let text = calendar::fetch_ics(&url).map_err(|e| e.to_string())?;
            Ok(calendar::parse_ics(&text)
                .into_iter()
                .filter(|e| e.end >= from && e.start <= to)
                .collect())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |ws, cx| {
                match result {
                    Ok(events) => {
                        tracing::info!(events = events.len(), "calendar read");
                        ws.integ.events = events;
                        ws.integ.calendar_error = None;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "could not read the calendar");
                        ws.integ.calendar_error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        if !self.integ.calendar_polling {
            self.integ.calendar_polling = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(CALENDAR_EVERY).await;
                    if this.update(cx, |ws, cx| ws.refresh_calendar(cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
    }

    /// Settings → Microsoft 365's Connect: the device code sign-in.
    fn connect_microsoft(&mut self, cx: &mut Context<Self>) {
        let Some(client_id) = value(&self.integ.graph_client.clone(), cx) else {
            self.integ.calendar_error = Some("Paste your app's client ID first.".into());
            cx.notify();
            return;
        };
        let tenant =
            value(&self.integ.graph_tenant.clone(), cx).unwrap_or_else(|| "organizations".into());
        self.settings.integrations.graph_client_id = Some(client_id.clone());
        self.settings.integrations.graph_tenant = tenant.clone();
        self.save_settings(cx);
        let auth = GraphAuth { client_id, tenant };
        self.integ.calendar_error = None;
        let start = cx.background_executor().spawn({
            let auth = auth.clone();
            async move { calendar::start_device_code(&auth).map_err(|e| e.to_string()) }
        });
        let store = self.store.clone();
        cx.spawn(async move |this, cx| {
            let code = match start.await {
                Ok(c) => c,
                Err(e) => {
                    let _ = this.update(cx, |ws, cx| {
                        ws.integ.calendar_error = Some(e);
                        cx.notify();
                    });
                    return;
                }
            };
            let _ = this.update(cx, |ws, cx| {
                ws.integ.device = Some(code.clone());
                cx.notify();
            });
            let mut interval = code.interval.max(1);
            let deadline = std::time::Instant::now() + Duration::from_secs(code.expires_in);
            let outcome: Result<Tokens, String> = loop {
                cx.background_executor()
                    .timer(Duration::from_secs(interval))
                    .await;
                // Cancelled from Settings.
                let waiting = this
                    .update(cx, |ws, _| ws.integ.device.as_ref() == Some(&code))
                    .unwrap_or(false);
                if !waiting {
                    return;
                }
                if std::time::Instant::now() > deadline {
                    break Err("The sign-in code expired. Connect again.".into());
                }
                let poll = cx.background_executor().spawn({
                    let (auth, code) = (auth.clone(), code.clone());
                    async move { calendar::poll_device_code(&auth, &code) }
                });
                match poll.await {
                    Ok(Poll::Pending) => {}
                    Ok(Poll::SlowDown) => interval += 5,
                    Ok(Poll::Token(t)) => break Ok(t),
                    Ok(Poll::Declined) => break Err("The sign-in was declined.".into()),
                    Ok(Poll::Expired) => {
                        break Err("The sign-in code expired. Connect again.".into());
                    }
                    Err(e) => break Err(e.to_string()),
                }
            };
            let saved = match outcome {
                Ok(tokens) => {
                    let account = tokens.account.clone();
                    let task = cx.background_executor().spawn({
                        let store = store.clone();
                        async move {
                            let json = serde_json::to_vec(&tokens).map_err(|e| e.to_string())?;
                            lock(&store)?
                                .set_secret(GRAPH_TOKENS, &json)
                                .map_err(|e| e.to_string())
                        }
                    });
                    task.await.map(|()| account)
                }
                Err(e) => Err(e),
            };
            let _ = this.update(cx, |ws, cx| {
                ws.integ.device = None;
                match saved {
                    Ok(account) => {
                        ws.integ.graph_signed_in = true;
                        ws.settings.integrations.graph_account = account;
                        ws.save_settings(cx);
                        ws.refresh_calendar(cx);
                    }
                    Err(e) => ws.integ.calendar_error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn disconnect_microsoft(&mut self, cx: &mut Context<Self>) {
        self.integ.device = None;
        self.integ.graph_signed_in = false;
        self.settings.integrations.graph_account = None;
        self.save_settings(cx);
        let store = self.store.clone();
        cx.background_executor()
            .spawn(async move {
                if let Ok(s) = lock(&store)
                    && let Err(e) = s.remove_secret(GRAPH_TOKENS)
                {
                    tracing::warn!(error = %e, "could not remove the Microsoft tokens");
                }
            })
            .detach();
        self.refresh_calendar(cx);
        cx.notify();
    }

    fn save_ics(&mut self, cx: &mut Context<Self>) {
        self.settings.integrations.ics_url = value(&self.integ.ics_url.clone(), cx);
        self.save_settings(cx);
        self.refresh_calendar(cx);
        cx.notify();
    }
}

/// The transcript line an action item came from, as "Name: words".
fn quote(store: &crate::workspace::SharedStore, item: &ActionItem) -> Option<String> {
    let s = store.lock().ok()?;
    let segments = s.segments(&item.meeting_id).ok()?;
    let people = s.people().ok()?;
    let seg = segments
        .iter()
        .find(|g| g.start_ms <= item.at_ms && item.at_ms < g.end_ms.max(g.start_ms + 1))
        .or_else(|| {
            segments
                .iter()
                .min_by_key(|g| (g.start_ms - item.at_ms).abs())
        })?;
    Some(format!(
        "{}: {}",
        switchscribe_agent::summarize::speaker_name(seg, &people),
        seg.text.trim()
    ))
}

/// The notice strip.
pub fn notice_strip(
    ws: &Workspace,
    p: &Palette,
    cx: &mut Context<Workspace>,
) -> Option<AnyElement> {
    let n = ws.integ.notice.clone()?;
    let link = |id: &'static str, label: String| {
        div()
            .id(id)
            .text_size(ts::BODY)
            .underline()
            .cursor_pointer()
            .child(label)
    };
    Some(
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(10.))
            .px(rpx(16.))
            .py(rpx(8.))
            .bg(p.elev)
            .border_b_1()
            .border_color(p.bd)
            .text_color(p.fg)
            .child(ui::dot(7., p.ok))
            .child(div().flex_1().min_w_0().truncate().child(n.text))
            .when_some(n.reveal, |d, path| {
                d.child(
                    link("notice-reveal", "Show in folder".into())
                        .on_click(cx.listener(move |_, _, _, cx| cx.reveal_path(&path))),
                )
            })
            .when_some(n.url, |d, (label, url)| {
                d.child(
                    link("notice-url", label)
                        .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url))),
                )
            })
            .child(
                div()
                    .id("notice-x")
                    .text_color(p.fg3)
                    .cursor_pointer()
                    .hover(|s| s.text_color(p.fg))
                    .on_click(cx.listener(|ws, _, _, cx| ws.dismiss_notice(cx)))
                    .child("✕"),
            )
            .into_any_element(),
    )
}

/// The Export button and its menu.
pub fn export_button(ws: &Workspace, p: &Palette, cx: &mut Context<Workspace>) -> AnyElement {
    let open = ws.integ.export_open;
    div()
        .relative()
        .child(
            ui::button("export", "Export ▾", Kind::Primary, p)
                .on_click(cx.listener(|ws, _, _, cx| ws.toggle_export_menu(cx))),
        )
        .when(open, |d| {
            d.child(
                gpui_kit::deferred(
                    gpui_kit::anchored()
                        .snap_to_window_with_margin(px(8.))
                        .child(
                            div()
                                .id("export-menu")
                                .occlude()
                                .mt(rpx(32.))
                                .w(rpx(180.))
                                .py(rpx(4.))
                                .rounded(px(6.))
                                .border_1()
                                .border_color(p.bd2)
                                .bg(p.elev)
                                .shadow_lg()
                                .on_mouse_down_out(cx.listener(|ws, _, _, cx| {
                                    ws.integ.export_open = false;
                                    cx.notify();
                                }))
                                .children(ExportFormat::ALL.iter().enumerate().map(|(i, f)| {
                                    let f = *f;
                                    div()
                                        .id(("export-format", i))
                                        .flex()
                                        .justify_between()
                                        .px(rpx(10.))
                                        .py(rpx(5.))
                                        .text_size(ts::BODY)
                                        .cursor_pointer()
                                        .hover(|s| s.bg(p.chip))
                                        .on_click(cx.listener(move |ws, _, _, cx| {
                                            ws.export_selected(f, cx)
                                        }))
                                        .child(f.label())
                                        .child(ui::mono(
                                            format!(".{}", f.extension()),
                                            ts::SMALL,
                                            p.fg3,
                                        ))
                                })),
                        ),
                )
                .with_priority(1),
            )
        })
        .into_any_element()
}

/// The meeting header's Send to Jira: every open item of the meeting not yet sent.
pub fn meeting_jira_button(
    ws: &Workspace,
    meeting_id: &str,
    p: &Palette,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let ids: Vec<i64> = ws
        .agent
        .all_items
        .iter()
        .filter(|a| {
            a.meeting_id == meeting_id && a.status == ActionStatus::Open && a.jira_key.is_none()
        })
        .map(|a| a.id)
        .collect();
    let sending = ws
        .agent
        .all_items
        .iter()
        .any(|a| a.meeting_id == meeting_id && ws.integ.sending.contains(&a.id));
    let label = match (sending, ids.len()) {
        (true, _) => "Sending…".to_owned(),
        (_, 0) => "Send to Jira".to_owned(),
        (_, n) => format!("Send {n} to Jira"),
    };
    ui::button("jira", label, Kind::Secondary, p)
        .when(ids.is_empty() || sending, |b| b.opacity(0.5))
        .on_click(cx.listener(move |ws, _, _, cx| ws.send_to_jira(ids.clone(), cx)))
        .into_any_element()
}

/// One action item's Jira button: Send, Sending…, or View KEY.
pub fn item_jira_button(
    ws: &Workspace,
    item: &ActionItem,
    p: &Palette,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let id = item.id;
    let base = div()
        .id(("jira-item", id as usize))
        .w(rpx(96.))
        .flex_none()
        .px(rpx(8.))
        .py(rpx(3.))
        .rounded(px(5.))
        .border_1()
        .border_color(p.bd2)
        .bg(p.surface)
        .text_size(ts::BODY)
        .text_color(p.fg_chip)
        .truncate()
        .cursor_pointer()
        .hover(|s| s.bg(p.chip));
    if let (Some(key), Some(url)) = (&item.jira_key, &item.jira_url) {
        let url = url.clone();
        return base
            .text_color(p.stamp)
            .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)))
            .child(format!("View {key}"))
            .into_any_element();
    }
    if ws.integ.sending.contains(&id) {
        return base.opacity(0.6).child("Sending…").into_any_element();
    }
    base.on_click(cx.listener(move |ws, _, _, cx| ws.send_to_jira(vec![id], cx)))
        .child("Send to Jira")
        .into_any_element()
}

/// Settings → Integrations.
pub fn settings_integrations(
    ws: &Workspace,
    p: &Palette,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let s = &ws.settings.integrations;
    let i = &ws.integ;
    let row = |label: &'static str, value: AnyElement| {
        div()
            .flex()
            .items_start()
            .gap(rpx(12.))
            .py(rpx(8.))
            .border_b_1()
            .border_color(p.bd)
            .child(
                div()
                    .w(rpx(200.))
                    .flex_none()
                    .pt(rpx(4.))
                    .text_color(p.fg2)
                    .child(label),
            )
            .child(div().flex_1().min_w_0().text_color(p.text).child(value))
    };
    let note = |t: String| div().text_size(ts::BODY).text_color(p.fg3).child(t);
    let warn = |t: String| div().text_size(ts::BODY).text_color(p.warn_fg).child(t);
    let field = |e: &Entity<InputState>| Input::new(e).text_size(ts::BODY);

    // Microsoft 365 calendar.
    let microsoft = div().flex().flex_col().gap(rpx(6.));
    let microsoft = if i.graph_signed_in {
        microsoft
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .child(ui::dot(7., p.ok))
                    .child(format!(
                        "Connected{}",
                        s.graph_account
                            .as_deref()
                            .map(|a| format!(" as {a}"))
                            .unwrap_or_default()
                    ))
                    .child(
                        ui::button("ms-disconnect", "Disconnect", Kind::Outline, p)
                            .on_click(cx.listener(|ws, _, _, cx| ws.disconnect_microsoft(cx))),
                    ),
            )
            .child(note(format!(
                "{} events from yesterday to tomorrow. Names meetings, lists invitees and picks the template.",
                i.events.len()
            )))
    } else if let Some(code) = &i.device {
        let (uri, user_code) = (code.verification_uri.clone(), code.user_code.clone());
        microsoft
            .child(note("Open the page, sign in, and enter this code:".into()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .child(ui::mono(code.user_code.clone(), ts::BASE, p.fg))
                    .child(
                        ui::button("ms-copy", "Copy code", Kind::Outline, p).on_click(cx.listener(
                            move |_, _, _, cx| {
                                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                    user_code.clone(),
                                ))
                            },
                        )),
                    )
                    .child(
                        ui::button("ms-open", "Open page", Kind::Primary, p)
                            .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&uri))),
                    )
                    .child(
                        ui::button("ms-cancel", "Cancel", Kind::Outline, p).on_click(cx.listener(
                            |ws, _, _, cx| {
                                ws.integ.device = None;
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(note(format!("Waiting for {}…", code.verification_uri)))
    } else {
        microsoft
            .child(field(&i.graph_client))
            .child(
                div()
                    .flex()
                    .gap(rpx(8.))
                    .child(div().flex_1().child(field(&i.graph_tenant)))
                    .child(
                        ui::button("ms-connect", "Connect", Kind::Primary, p)
                            .on_click(cx.listener(|ws, _, _, cx| ws.connect_microsoft(cx))),
                    ),
            )
            .child(note(
                "Register an app in Microsoft Entra ID (public client flows on, delegated Calendars.Read) and paste its client ID. Tenant: your tenant ID, or \"organizations\"."
                    .into(),
            ))
    };
    let ics = div()
        .flex()
        .flex_col()
        .gap(rpx(6.))
        .child(
            div()
                .flex()
                .gap(rpx(8.))
                .child(div().flex_1().child(field(&i.ics_url)))
                .child(
                    ui::button("ics-save", "Save", Kind::Outline, p)
                        .on_click(cx.listener(|ws, _, _, cx| ws.save_ics(cx))),
                ),
        )
        .child(note(
            "If your tenant blocks the app: Outlook → Settings → Calendar → Shared calendars → Publish, then paste the ICS link."
                .into(),
        ));
    let calendar_error = i.calendar_error.clone();

    // Jira.
    let jira_status = match &i.jira {
        JiraState::Unknown if i.jira_token_saved => note("API token saved.".into()),
        JiraState::Unknown => note(
            "Create an API token at id.atlassian.com → Security → API tokens. It is stored encrypted on this machine."
                .into(),
        ),
        JiraState::Testing => note("Checking…".into()),
        JiraState::Connected(name) => div()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .text_size(ts::BODY)
            .child(ui::dot(7., p.ok))
            .child(format!("Connected as {name}")),
        JiraState::Failed(e) => warn(e.clone()),
    };
    let jira = div()
        .flex()
        .flex_col()
        .gap(rpx(6.))
        .child(field(&i.jira_site))
        .child(
            div()
                .flex()
                .gap(rpx(8.))
                .child(div().flex_1().child(field(&i.jira_email)))
                .child(div().flex_1().child(field(&i.jira_token))),
        )
        .child(
            div()
                .flex()
                .gap(rpx(8.))
                .child(div().flex_1().child(field(&i.jira_project)))
                .child(div().flex_1().child(field(&i.jira_type)))
                .child(
                    ui::button("jira-save", "Save and test", Kind::Primary, p)
                        .on_click(cx.listener(|ws, _, _, cx| ws.save_jira(cx))),
                )
                .when(i.jira_token_saved, |d| {
                    d.child(
                        ui::button("jira-forget", "Forget token", Kind::Outline, p)
                            .on_click(cx.listener(|ws, _, w, cx| ws.forget_jira(w, cx))),
                    )
                }),
        )
        .child(jira_status)
        .child(note(
            "Each issue links back to its moment (switchscribe://) and quotes the line it came from."
                .into(),
        ));
    div()
        .child(
            ui::section_label("Integrations", p.fg3)
                .mt(rpx(16.))
                .mb(rpx(4.)),
        )
        .child(row("Microsoft 365 calendar", microsoft.into_any_element()))
        .child(row("Published calendar", ics.into_any_element()))
        .when_some(calendar_error, |d, e| {
            d.child(row("", warn(format!("Calendar: {e}")).into_any_element()))
        })
        .child(row("Jira", jira.into_any_element()))
        .into_any_element()
}

/// Keep a calendar event with a new meeting and add the invitees already in the library
/// as attendees (called with the store locked, off the UI thread).
pub fn attach_event(store: &Store, meeting_id: &str, event: &CalendarEvent) {
    match serde_json::to_string(event) {
        Ok(json) => {
            if let Err(e) = store.set_meeting_calendar(meeting_id, Some(&json)) {
                tracing::warn!(error = %e, "could not keep the calendar event");
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not write the calendar event"),
    }
    let Ok(people) = store.people() else {
        return;
    };
    for a in &event.attendees {
        if let Some(id) = switchscribe_agent::summarize::match_person(&a.name, &people)
            && let Err(e) = store.add_attendee(meeting_id, id)
        {
            tracing::warn!(error = %e, "could not add an attendee");
        }
    }
}

/// Invitees from a meeting's calendar event who are not in the library yet.
pub fn new_invitees(
    calendar_json: Option<&str>,
    people: &[switchscribe_store::Person],
) -> Vec<String> {
    let Some(event) = calendar_json.and_then(|j| serde_json::from_str::<CalendarEvent>(j).ok())
    else {
        return Vec::new();
    };
    event
        .attendees
        .into_iter()
        .map(|a| a.name)
        .filter(|n| !n.trim().is_empty() && !n.contains('@'))
        .filter(|n| switchscribe_agent::summarize::match_person(n, people).is_none())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invitees_not_in_the_library_are_suggested() {
        let json = serde_json::to_string(&demo_events()[0]).unwrap();
        let people = [switchscribe_store::Person {
            id: 1,
            name: "Priya Nair".into(),
            color: None,
            is_me: false,
        }];
        assert_eq!(
            new_invitees(Some(&json), &people),
            ["Marcus Chen", "Dana Ortiz", "Lena Park"]
        );
        assert!(new_invitees(None, &people).is_empty());
    }

    #[test]
    fn a_new_meeting_keeps_its_event_and_known_attendees() {
        let mut s = Store::open_in_memory().unwrap();
        let priya = s.add_person("Priya Nair", None, false).unwrap();
        s.insert_meeting(&switchscribe_store::NewMeeting {
            id: "m".into(),
            title: "Cerberus rollout sync".into(),
            started_at: 0,
            duration_secs: 0,
            source_app: "Teams".into(),
            status: switchscribe_store::MeetingStatus::Recording,
            attendees: vec![],
        })
        .unwrap();
        attach_event(&s, "m", &demo_events()[0]);
        let m = s.meeting("m").unwrap().unwrap();
        assert_eq!(
            m.attendees.iter().map(|p| p.id).collect::<Vec<_>>(),
            [priya]
        );
        assert!(s.meeting_calendar("m").unwrap().is_some());
    }
}
