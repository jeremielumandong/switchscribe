//! Summaries and action items: running the coding CLI after a call (and on Re-run
//! summary), the Summary and Action items tabs, the cross-meeting Action items screen,
//! and Settings → Agent.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement as _,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use switchscribe_agent::{Cli, Template};
use switchscribe_store::{ActionItem, ActionStatus, Bullet, Meeting, Summary};

use crate::appearance::{rpx, ts};
use crate::format;
use crate::processing::{Poll, Worker};
use crate::recording::Banner;
use crate::theme::Palette;
use crate::ui::{self, Kind};
use crate::workspace::{Screen, Workspace, screen_title};

/// Where a meeting's summary job is.
#[derive(Clone, Debug, PartialEq)]
pub enum SummaryJob {
    /// Waiting its turn.
    Queued,
    /// The CLI is writing it.
    Running,
    /// It failed; why.
    Failed(String),
}

/// The workspace's summary and action-item state.
#[derive(Default)]
pub struct Agent {
    /// The selected meeting's summary and action items.
    pub meeting: Option<(String, Option<Summary>, Vec<ActionItem>)>,
    /// Every action item, for the Action items screen.
    pub all_items: Vec<ActionItem>,
    /// The CLI found for the chosen kind, or why not; `None` while looking.
    pub cli: Option<Result<PathBuf, String>>,
    /// Show done items on the Action items screen too.
    pub show_done: bool,
    /// Settings → Agent: the template being edited and its text.
    editing: Option<(Template, Entity<TextareaState>)>,
    queue: VecDeque<String>,
    running: Option<(String, Worker<Result<(), String>>)>,
    failed: HashMap<String, String>,
    polling: bool,
}

impl Agent {
    /// A meeting's summary job, if it has one.
    pub fn job(&self, id: &str) -> Option<SummaryJob> {
        if self.running.as_ref().is_some_and(|(r, _)| r == id) {
            return Some(SummaryJob::Running);
        }
        if let Some(e) = self.failed.get(id) {
            return Some(SummaryJob::Failed(e.clone()));
        }
        self.queue
            .iter()
            .any(|q| q == id)
            .then_some(SummaryJob::Queued)
    }
}

impl Workspace {
    /// Read the selected meeting's summary and action items on a background thread.
    pub(crate) fn load_summary(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected().map(|m| m.id.clone()) else {
            return;
        };
        let store = self.store.clone();
        let task = cx.background_executor().spawn({
            let id = id.clone();
            async move {
                let s = store
                    .lock()
                    .map_err(|_| "the index is unavailable".to_owned())?;
                let summary = s.summary(&id).map_err(|e| e.to_string())?;
                let items = s.action_items(Some(&id)).map_err(|e| e.to_string())?;
                Ok::<_, String>((summary, items))
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |ws, cx| {
                match result {
                    Ok((summary, items)) => ws.agent.meeting = Some((id, summary, items)),
                    Err(e) => tracing::warn!(error = %e, "could not read the summary"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Read every action item on a background thread.
    pub(crate) fn load_all_actions(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            store
                .lock()
                .map_err(|_| "the index is unavailable".to_owned())?
                .action_items(None)
                .map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |ws, cx| {
                match result {
                    Ok(items) => ws.agent.all_items = items,
                    Err(e) => tracing::warn!(error = %e, "could not read action items"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Look for the chosen CLI on a background thread.
    pub(crate) fn detect_cli(&mut self, cx: &mut Context<Self>) {
        let kind = self.settings.agent.cli;
        let path = self.settings.agent.cli_path.clone();
        self.agent.cli = None;
        let task = cx.background_executor().spawn(async move {
            Cli::find(kind, path.as_deref())
                .map(|c| c.program)
                .map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let found = task.await;
            let _ = this.update(cx, |ws, cx| {
                // The selector may have moved on meanwhile.
                if ws.settings.agent.cli == kind {
                    ws.agent.cli = Some(found);
                    ws.pump_summaries(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Queue a meeting for a summary (after transcription, or Re-run summary).
    pub(crate) fn queue_summary(&mut self, id: String, cx: &mut Context<Self>) {
        if self.demo {
            self.banners.push(Banner::plain(
                "The demo's summaries are prewritten; record a meeting to summarize it.",
            ));
            cx.notify();
            return;
        }
        self.agent.failed.remove(&id);
        let running = self.agent.running.as_ref().is_some_and(|(r, _)| *r == id);
        if !running && !self.agent.queue.contains(&id) {
            self.agent.queue.push_back(id);
        }
        self.pump_summaries(cx);
        cx.notify();
    }

    fn pump_summaries(&mut self, cx: &mut Context<Self>) {
        if self.agent.running.is_some() || self.agent.queue.is_empty() {
            return;
        }
        let program = match &self.agent.cli {
            None => return,
            Some(Err(e)) => {
                // Everything queued fails the same way; the Summary tab says why.
                for id in std::mem::take(&mut self.agent.queue) {
                    self.agent.failed.insert(id, e.clone());
                }
                return;
            }
            Some(Ok(p)) => p.clone(),
        };
        let Some(id) = self.agent.queue.pop_front() else {
            return;
        };
        let cli = Cli {
            kind: self.settings.agent.cli,
            program,
        };
        let title = self
            .meetings
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.title.clone())
            .unwrap_or_default();
        let guidance = self
            .settings
            .agent
            .templates
            .get(Template::for_title(&title).key())
            .cloned();
        let store = self.store.clone();
        let workdir = self.paths.data.join("agent");
        let meeting_id = id.clone();
        tracing::info!(meeting = %id, cli = cli.kind.label(), "summarizing");
        let worker = Worker::spawn("summarize", move |_, cancel| {
            switchscribe_agent::summarize_meeting(
                &store,
                &meeting_id,
                &cli,
                guidance.as_deref(),
                &workdir,
                cancel,
            )
            .map_err(|e| e.to_string())
        });
        self.agent.running = Some((id, worker));
        self.poll_summaries(cx);
    }

    fn poll_summaries(&mut self, cx: &mut Context<Self>) {
        if self.agent.polling {
            return;
        }
        self.agent.polling = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let busy = this.update(cx, |ws, cx| {
                    ws.collect_summary(cx);
                    let busy = ws.agent.running.is_some();
                    if !busy {
                        ws.agent.polling = false;
                    }
                    busy
                });
                if !busy.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
    }

    fn collect_summary(&mut self, cx: &mut Context<Self>) {
        let Some((id, w)) = &self.agent.running else {
            return;
        };
        let result = match w.poll() {
            Poll::Pending => return,
            Poll::Done(r) => r,
            Poll::Lost => Err("the summary stopped unexpectedly".into()),
        };
        let id = id.clone();
        self.agent.running = None;
        match result {
            Ok(()) => {
                tracing::info!(meeting = %id, "summarized");
                if self.selected().is_some_and(|m| m.id == id) {
                    self.load_summary(cx);
                }
                self.load_all_actions(cx);
            }
            Err(e) if e == "cancelled" => {}
            Err(e) => {
                tracing::warn!(meeting = %id, error = %e, "summary failed");
                self.agent.failed.insert(id, e);
            }
        }
        self.pump_summaries(cx);
        cx.notify();
    }

    /// Mark an action item open or done.
    pub(crate) fn set_action_status(
        &mut self,
        id: i64,
        status: ActionStatus,
        cx: &mut Context<Self>,
    ) {
        for item in self
            .agent
            .all_items
            .iter_mut()
            .chain(self.agent.meeting.iter_mut().flat_map(|m| m.2.iter_mut()))
        {
            if item.id == id {
                item.status = status;
            }
        }
        let store = self.store.clone();
        cx.background_executor()
            .spawn(async move {
                if let Ok(s) = store.lock()
                    && let Err(e) = s.set_action_status(id, status)
                {
                    tracing::warn!(error = %e, "could not save the status");
                }
            })
            .detach();
        cx.notify();
    }

    /// Open a meeting at a moment (a citation, or a cross-meeting action item).
    pub(crate) fn open_moment(&mut self, meeting_id: &str, secs: i64, cx: &mut Context<Self>) {
        if !self.meetings.iter().any(|m| m.id == meeting_id) {
            return;
        }
        if self.selected().is_none_or(|m| m.id != meeting_id) {
            self.select(meeting_id.to_owned(), cx);
        }
        self.screen = Screen::Meeting(meeting_id.to_owned());
        self.cue(secs, cx);
    }

    fn toggle_auto_summarize(&mut self, cx: &mut Context<Self>) {
        self.settings.agent.auto_summarize = !self.settings.agent.auto_summarize;
        self.save_settings(cx);
        cx.notify();
    }

    fn edit_template(&mut self, t: Template, window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .settings
            .agent
            .templates
            .get(t.key())
            .cloned()
            .unwrap_or_else(|| t.guidance().to_owned());
        let state = cx.new(|cx| {
            let mut s = TextareaState::new(window, cx);
            s.set_value(text, window, cx);
            s
        });
        self.agent.editing = Some((t, state));
        cx.notify();
    }

    fn save_template(&mut self, cx: &mut Context<Self>) {
        let Some((t, state)) = &self.agent.editing else {
            return;
        };
        let text = state.read(cx).value().trim().to_owned();
        if text.is_empty() || text == t.guidance() {
            self.settings.agent.templates.remove(t.key());
        } else {
            self.settings
                .agent
                .templates
                .insert(t.key().to_owned(), text);
        }
        self.agent.editing = None;
        self.save_settings(cx);
        cx.notify();
    }

    fn reset_template(&mut self, cx: &mut Context<Self>) {
        if let Some((t, _)) = self.agent.editing.take() {
            self.settings.agent.templates.remove(t.key());
            self.save_settings(cx);
        }
        cx.notify();
    }
}

/// The owner of an action item: their name and speaker color.
fn owner(ws: &Workspace, item: &ActionItem, p: &Palette) -> Option<(String, gpui_kit::Hsla)> {
    match item.owner_id.and_then(|id| ws.person(id)) {
        Some(person) => Some((person.name.clone(), p.speaker(person.color))),
        None => item.owner_name.clone().map(|n| (n, p.unknown)),
    }
}

/// The Summary tab.
pub fn summary_tab(
    ws: &Workspace,
    m: &Meeting,
    p: &Palette,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let loaded = ws.agent.meeting.as_ref().filter(|(id, _, _)| *id == m.id);
    let job = ws.agent.job(&m.id);
    let Some((_, Some(summary), items)) = loaded else {
        let body = match job {
            Some(SummaryJob::Running) => format!(
                "{} is writing the summary from the transcript.",
                ws.settings.agent.cli.label()
            ),
            Some(SummaryJob::Queued) => "The summary is queued.".to_owned(),
            Some(SummaryJob::Failed(e)) => format!("The summary failed: {e}"),
            None => format!(
                "After each call {} writes the summary, decisions, action items and open questions, each linked to its moment. Re-run summary writes one now.",
                ws.settings.agent.cli.label()
            ),
        };
        return ui::empty_state("No summary yet", body, p).into_any_element();
    };
    let plain =
        |b: &[Bullet]| -> Vec<Line> { b.iter().map(|b| (b.text.clone(), b.at_ms, None)).collect() };
    let actions = items
        .iter()
        .map(|i| (i.text.clone(), i.at_ms, owner(ws, i, p)))
        .collect();
    let byline = match job {
        Some(SummaryJob::Running) => format!("{} is rewriting it…", ws.settings.agent.cli.label()),
        Some(SummaryJob::Failed(e)) => format!("The last re-run failed: {e}"),
        _ => format!("generated by {} via MCP", summary.cli),
    };
    div()
        .max_w(rpx(760.))
        .p(rpx(20.))
        .flex()
        .flex_col()
        .gap(rpx(22.))
        .child(
            div()
                .flex()
                .gap(rpx(8.))
                .text_size(ts::SMALL)
                .text_color(p.fg3)
                .child(ui::mono(
                    format!("template: {}", summary.template),
                    ts::SMALL,
                    p.fg3,
                ))
                .child("·")
                .child(byline),
        )
        .child(section(0, "Summary", plain(&summary.summary), p, cx))
        .child(section(1, "Decisions", plain(&summary.decisions), p, cx))
        .child(section(2, "Action items", actions, p, cx))
        .child(section(
            3,
            "Open questions",
            plain(&summary.open_questions),
            p,
            cx,
        ))
        .into_any_element()
}

/// A line of the summary: its text, moment (milliseconds) and owner.
type Line = (String, i64, Option<(String, gpui_kit::Hsla)>);

/// A summary section: a label and its lines, each with a timestamp chip.
fn section(
    si: usize,
    title: &'static str,
    lines: Vec<Line>,
    p: &Palette,
    cx: &mut Context<Workspace>,
) -> gpui_kit::Div {
    let empty = lines.is_empty();
    let rows: Vec<gpui_kit::Div> = lines
        .into_iter()
        .enumerate()
        .map(|(bi, (text, at_ms, who))| {
            let at = at_ms / 1000;
            div()
                .flex()
                .items_start()
                .gap(rpx(10.))
                .line_height(rpx(20.))
                .child(
                    div()
                        .mt(rpx(8.))
                        .size(rpx(4.))
                        .flex_none()
                        .rounded_full()
                        .bg(p.fg4),
                )
                .child(div().flex_1().min_w_0().text_color(p.text).child(text))
                .when_some(who, |d, (name, color)| {
                    d.child(ui::person_chip(name, color, p).mt(rpx(1.)))
                })
                .child(
                    ui::stamp(("sum", si * 1000 + bi), at, true, p)
                        .mt(rpx(1.))
                        .on_click(cx.listener(move |this, _, _, cx| this.cue(at, cx))),
                )
        })
        .collect();
    div()
        .flex()
        .flex_col()
        .gap(rpx(8.))
        .child(ui::section_label(title, p.fg2))
        .when(empty, |d| {
            d.child(div().text_size(ts::BODY).text_color(p.fg3).child("None."))
        })
        .children(rows)
}

/// A status chip that flips an item between open and done.
fn status_chip(
    id: i64,
    status: ActionStatus,
    p: &Palette,
    cx: &mut Context<Workspace>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let next = match status {
        ActionStatus::Done => ActionStatus::Open,
        _ => ActionStatus::Done,
    };
    div()
        .id(("status", id as usize))
        .w(rpx(84.))
        .flex_none()
        .px(rpx(8.))
        .py(rpx(3.))
        .rounded(px(5.))
        .border_1()
        .border_color(p.bd2)
        .bg(p.surface)
        .text_size(ts::BODY)
        .text_color(match status {
            ActionStatus::Done => p.ok,
            ActionStatus::InJira => p.stamp,
            ActionStatus::Open => p.fg_chip,
        })
        .cursor_pointer()
        .hover(|s| s.bg(p.chip))
        .on_click(cx.listener(move |ws, _, _, cx| ws.set_action_status(id, next, cx)))
        .child(status.label())
}

/// The Action items tab: this meeting's items.
pub fn actions_tab(
    ws: &Workspace,
    m: &Meeting,
    p: &Palette,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let items = match ws.agent.meeting.as_ref().filter(|(id, _, _)| *id == m.id) {
        Some((_, _, items)) if !items.is_empty() => items,
        _ => {
            return ui::empty_state(
                "No action items",
                "Action items come out of the summary, with owners, due dates and the moment they came up.",
                p,
            )
            .into_any_element();
        }
    };
    div()
        .max_w(rpx(860.))
        .px(rpx(20.))
        .py(rpx(16.))
        .flex()
        .flex_col()
        .gap(rpx(6.))
        .children(items.iter().enumerate().map(|(i, a)| {
            let at = a.at_ms / 1000;
            let done = a.status == ActionStatus::Done;
            let who = owner(ws, a, p);
            div()
                .flex()
                .items_center()
                .gap(rpx(12.))
                .p(rpx(10.))
                .border_1()
                .border_color(p.bd)
                .rounded(px(6.))
                .bg(p.panel)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(if done { p.fg3 } else { p.text })
                        .when(done, |d| d.line_through())
                        .child(div().flex_1().min_w_0().child(a.text.clone())),
                )
                .child(
                    div()
                        .w(rpx(130.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .text_size(ts::BODY)
                        .text_color(p.fg_chip)
                        .when_some(who, |d, (name, color)| {
                            d.child(ui::dot(7., color))
                                .child(div().min_w_0().truncate().child(name))
                        }),
                )
                .child(
                    div()
                        .w(rpx(70.))
                        .flex_none()
                        .text_size(ts::BODY)
                        .text_color(p.fg2)
                        .child(a.due.clone().unwrap_or_else(|| "—".into())),
                )
                .child(
                    div().w(rpx(70.)).flex_none().child(
                        ui::stamp(("act", i), at, true, p)
                            .on_click(cx.listener(move |this, _, _, cx| this.cue(at, cx))),
                    ),
                )
                .child(status_chip(a.id, a.status, p, cx))
                .child(crate::integrations_view::item_jira_button(ws, a, p, cx))
        }))
        .into_any_element()
}

/// The Action items screen: every meeting's items.
pub fn action_items_screen(ws: &Workspace, p: &Palette, cx: &mut Context<Workspace>) -> AnyElement {
    let open = ws
        .agent
        .all_items
        .iter()
        .filter(|i| i.status != ActionStatus::Done)
        .count();
    let show_done = ws.agent.show_done;
    let items: Vec<&ActionItem> = ws
        .agent
        .all_items
        .iter()
        .filter(|i| show_done || i.status != ActionStatus::Done)
        .collect();
    const OWNER: f32 = 120.;
    const DUE: f32 = 56.;
    const SOURCE: f32 = 182.;
    const STATUS: f32 = 92.;
    const CHECK: f32 = 28.;
    const JIRA: f32 = 104.;
    let cell = |w: Option<f32>| {
        let d = div().px(rpx(4.)).flex().items_center();
        match w {
            Some(w) => d.w(rpx(w)).flex_none(),
            None => d.flex_1().min_w_0(),
        }
    };
    let header = div()
        .w_full()
        .flex()
        .py(rpx(8.))
        .border_b_1()
        .border_color(p.bd)
        .text_size(ts::SMALL)
        .text_color(p.fg3)
        .child(cell(Some(CHECK)))
        .child(cell(None).child("Item"))
        .child(cell(Some(OWNER)).child("Owner"))
        .child(cell(Some(DUE)).child("Due"))
        .child(cell(Some(SOURCE)).child("Source"))
        .child(cell(Some(STATUS)).child("Status"))
        .child(cell(Some(JIRA)));
    let rows: Vec<gpui_kit::Div> = items
        .into_iter()
        .enumerate()
        .map(|(i, a)| {
            let at = a.at_ms / 1000;
            let meeting_id = a.meeting_id.clone();
            let who = owner(ws, a, p);
            let done = a.status == ActionStatus::Done;
            let (id, sendable) = (a.id, a.jira_key.is_none() && !done);
            let checked = ws.integ.checked.contains(&a.id);
            div()
                .w_full()
                .flex()
                .py(rpx(6.))
                .min_h(rpx(38.))
                .child(
                    cell(Some(CHECK)).child(
                        div()
                            .id(("check", id as usize))
                            .when(sendable, |d| {
                                d.cursor_pointer().on_click(
                                    cx.listener(move |ws, _, _, cx| ws.toggle_checked(id, cx)),
                                )
                            })
                            .when(!sendable, |d| d.opacity(0.3))
                            .child(ui::checkbox(checked && sendable, p)),
                    ),
                )
                .border_b_1()
                .border_color(p.elev)
                .child(
                    // A block, not a flex cell, so long items wrap.
                    div()
                        .flex_1()
                        .min_w_0()
                        .px(rpx(4.))
                        .py(rpx(4.))
                        .text_color(if done { p.fg3 } else { p.text })
                        .when(done, |d| d.line_through())
                        .child(a.text.clone()),
                )
                .child(
                    cell(Some(OWNER))
                        .gap(rpx(6.))
                        .text_color(p.fg_chip)
                        .when_some(who, |d, (name, color)| {
                            d.child(ui::dot(7., color))
                                .child(div().min_w_0().truncate().child(name))
                        }),
                )
                .child(
                    cell(Some(DUE))
                        .text_color(p.fg2)
                        .child(a.due.clone().unwrap_or_else(|| "—".into())),
                )
                .child(
                    cell(Some(SOURCE))
                        .gap(rpx(6.))
                        .child(div().min_w_0().truncate().text_color(p.fg2).child(format!(
                            "{} · {}",
                            a.meeting_title,
                            format::day(a.meeting_started_at, ws.today)
                        )))
                        .child(ui::stamp(("all-act", i), at, false, p).on_click(
                            cx.listener(move |ws, _, _, cx| ws.open_moment(&meeting_id, at, cx)),
                        )),
                )
                .child(cell(Some(STATUS)).child(status_chip(a.id, a.status, p, cx)))
                .child(
                    cell(Some(JIRA))
                        .child(crate::integrations_view::item_jira_button(ws, a, p, cx)),
                )
        })
        .collect();
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .items_center()
                .pr(rpx(20.))
                .child(div().flex_1().child(screen_title("Action items", None, p)))
                .child(ui::mono(format!("{open} open"), ts::BODY, p.fg3))
                .child(div().w(rpx(12.)))
                .child({
                    let ids: Vec<i64> = ws.integ.checked.iter().copied().collect();
                    let label = match ids.len() {
                        0 => "Send to Jira".to_owned(),
                        n => format!("Send {n} to Jira"),
                    };
                    ui::button("bulk-jira", label, Kind::Primary, p)
                        .when(ids.is_empty(), |b| b.opacity(0.5))
                        .on_click(cx.listener(move |ws, _, _, cx| ws.send_to_jira(ids.clone(), cx)))
                })
                .child(div().w(rpx(6.)))
                .child(
                    ui::button(
                        "show-done",
                        if show_done { "Hide done" } else { "Show done" },
                        Kind::Outline,
                        p,
                    )
                    .on_click(cx.listener(|ws, _, _, cx| {
                        ws.agent.show_done = !ws.agent.show_done;
                        cx.notify();
                    })),
                ),
        )
        .child(if ws.agent.all_items.is_empty() {
            ui::empty_state(
                "No action items yet",
                "Every meeting's summary adds its action items here, with owners, due dates and the moment they came up.",
                p,
            )
            .into_any_element()
        } else {
            div()
                .id("all-actions")
                .flex_1()
                .overflow_y_scroll()
                .px(rpx(20.))
                .pb(rpx(20.))
                .text_size(ts::UI)
                .child(header)
                .children(rows)
                .into_any_element()
        })
        .into_any_element()
}

/// Settings → Agent.
pub fn settings_agent(ws: &Workspace, p: &Palette, cx: &mut Context<Workspace>) -> AnyElement {
    let a = &ws.settings.agent;
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
                    .text_color(p.fg2)
                    .child(label),
            )
            .child(div().flex_1().min_w_0().text_color(p.text).child(value))
    };
    let note = |t: String| div().text_size(ts::BODY).text_color(p.fg3).child(t);
    let found = match &ws.agent.cli {
        None => note("Looking…".into()).into_any_element(),
        Some(Ok(path)) => ui::mono(path.display().to_string(), ts::SMALL, p.fg2).into_any_element(),
        Some(Err(e)) => div()
            .text_size(ts::BODY)
            .text_color(p.warn_fg)
            .child(e.clone())
            .into_any_element(),
    };
    let cli = div()
        .flex()
        .flex_col()
        .gap(rpx(6.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(rpx(8.))
                .child(div().child(a.cli.label()))
                .child(
                    ui::button("cli-next", "Change", Kind::Outline, p)
                        .on_click(cx.listener(|ws, _, _, cx| ws.cycle_cli(cx))),
                ),
        )
        .child(found)
        .child(note(
            "Summaries and answers send transcript text to this CLI's model provider; a CLI on a local model keeps everything on this machine."
                .into(),
        ));
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "switchscribe".into());
    let own_cli = div()
        .flex()
        .flex_col()
        .gap(rpx(6.))
        .child(note(
            "Add switchscribe to your own CLI sessions to ask about meetings from your terminal:"
                .into(),
        ))
        .child({
            let command = format!("claude mcp add switchscribe -- \"{exe}\" mcp");
            div()
                .flex()
                .items_center()
                .gap(rpx(8.))
                .child(div().flex_1().min_w_0().truncate().child(ui::mono(
                    command.clone(),
                    ts::SMALL,
                    p.fg_chip,
                )))
                .child(
                    ui::button("copy-mcp", "Copy", Kind::Outline, p).on_click(cx.listener(
                        move |_, _, _, cx| {
                            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                command.clone(),
                            ));
                        },
                    )),
                )
        });
    let auto = a.auto_summarize;
    let templates = div()
        .flex()
        .flex_col()
        .gap(rpx(8.))
        .child(div().flex().flex_wrap().gap(rpx(6.)).children(
            Template::ALL.iter().enumerate().map(|(i, t)| {
                let t = *t;
                let custom = a.templates.contains_key(t.key());
                let on = ws.agent.editing.as_ref().is_some_and(|(e, _)| *e == t);
                div()
                    .id(("template", i))
                    .px(rpx(8.))
                    .py(rpx(3.))
                    .rounded(px(5.))
                    .border_1()
                    .border_color(if on { p.fg2 } else { p.bd2 })
                    .text_size(ts::BODY)
                    .text_color(if on { p.fg } else { p.fg_ctl })
                    .cursor_pointer()
                    .hover(|s| s.bg(p.chip))
                    .on_click(cx.listener(move |ws, _, window, cx| ws.edit_template(t, window, cx)))
                    .child(if custom {
                        format!("{} · edited", t.key())
                    } else {
                        t.key().to_owned()
                    })
            }),
        ))
        .map(|d| match &ws.agent.editing {
            Some((t, state)) => d
                .child(
                    div()
                        .h(rpx(110.))
                        .child(Textarea::new(state).h_full()),
                )
                .child(
                    div()
                        .flex()
                        .gap(rpx(6.))
                        .child(
                            ui::button("template-save", "Save", Kind::Primary, p)
                                .on_click(cx.listener(|ws, _, _, cx| ws.save_template(cx))),
                        )
                        .child(
                            ui::button("template-reset", "Use the default", Kind::Outline, p)
                                .on_click(cx.listener(|ws, _, _, cx| ws.reset_template(cx))),
                        )
                        .child(note(format!(
                            "Picked for meetings whose title suggests a {}.",
                            t.key()
                        ))),
                ),
            None => d.child(note(
                "Each template tells the CLI what to focus on; the one used is picked from the meeting title."
                    .into(),
            )),
        });
    div()
        .flex()
        .flex_col()
        .child(row("Coding CLI", cli.into_any_element()))
        .child(row(
            "Summarize after each call",
            div()
                .flex()
                .child(
                    ui::button(
                        "auto-summary",
                        if auto { "On" } else { "Off" },
                        Kind::Outline,
                        p,
                    )
                    .on_click(cx.listener(|ws, _, _, cx| ws.toggle_auto_summarize(cx))),
                )
                .into_any_element(),
        ))
        .child(row("Summary templates", templates.into_any_element()))
        .child(row("Your own CLI", own_cli.into_any_element()))
        .into_any_element()
}
