//! The Ask panel (right, 360 px): chat with the coding CLI, scoped to this meeting or all
//! meetings. Each question runs the CLI headless with switchscribe's MCP server in
//! `--ask` mode; changes it proposes come back as approve/deny cards.

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, px,
};
use std::path::PathBuf;
use std::time::Duration;

use switchscribe_agent::ask::{self, AskScope, Cite};
use switchscribe_agent::cli::ASK_TIMEOUT;
use switchscribe_agent::{Cli, McpServer};
use switchscribe_store::{CliKind, PendingAction};

use crate::processing::{Poll, Worker};
use crate::workspace::SharedStore;

use crate::appearance::{rpx, ts};
use crate::theme::{MONO, palette};
use crate::ui::{self, Kind};

/// What questions cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The selected meeting.
    Meeting,
    /// Every meeting.
    All,
}

struct Message {
    from_user: bool,
    text: SharedString,
    /// Cited moments.
    cites: Vec<Cite>,
    /// An error rather than an answer (shown in amber).
    failed: bool,
}

/// A request from the panel to the workspace.
pub enum AskEvent {
    /// A citation chip was clicked: open this meeting at this moment (seconds).
    Cite(String, i64),
    /// The user asked something; the workspace answers with [`AskPanel::run`].
    Ask(String),
    /// An approved change was applied; summaries should be read again.
    Changed,
}

/// What a question needs to run.
pub struct AskRequest {
    /// The question.
    pub question: String,
    /// The meeting it is about, or all of them.
    pub scope: AskScope,
    /// The CLI, or why there is none.
    pub cli: Result<Cli, String>,
    /// switchscribe's MCP server, as the CLI should start it.
    pub mcp: McpServer,
    /// Where the CLI runs.
    pub workdir: PathBuf,
    /// The user's name, as transcripts show it.
    pub me: Option<String>,
}

impl gpui_kit::EventEmitter<AskEvent> for AskPanel {}

/// The panel's state.
pub struct AskPanel {
    input: Entity<InputState>,
    scope: Scope,
    cli: CliKind,
    messages: Vec<Message>,
    /// Questions and answers so far, for follow-ups.
    history: Vec<(String, String)>,
    running: Option<(String, Worker<Result<String, String>>)>,
    /// Changes the agent proposed, waiting for approve or deny.
    pending: Vec<(PendingAction, String, Vec<String>)>,
    store: SharedStore,
    /// The conversation's scroll position, kept at the newest message.
    scroll: ScrollHandle,
    _sub: Subscription,
}

const PROMPTS: [&str; 3] = [
    "Summarize decisions",
    "What did I commit to this week?",
    "Draft Jira issues",
];

impl AskPanel {
    /// A panel with an empty conversation (or the demo's, when `demo` is set).
    pub fn new(
        cli: CliKind,
        demo: bool,
        store: SharedStore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Ask about this meeting"));
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.send(window, cx);
            }
        });
        let messages = if demo {
            crate::demo::conversation()
                .into_iter()
                .map(|(from_user, text, cites)| Message {
                    from_user,
                    text: text.into(),
                    cites: cites
                        .into_iter()
                        .map(|secs| Cite {
                            meeting_id: crate::demo::FEATURED.into(),
                            at_ms: secs * 1000,
                        })
                        .collect(),
                    failed: false,
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut this = Self {
            input,
            scope: Scope::Meeting,
            cli,
            messages,
            history: Vec::new(),
            running: None,
            pending: Vec::new(),
            store,
            scroll: ScrollHandle::new(),
            _sub: sub,
        };
        // Proposals left from an earlier run still wait for an answer.
        this.load_pending(cx);
        this
    }

    /// What questions cover now.
    pub fn scope(&self) -> Scope {
        self.scope
    }

    /// Answer a question: run the CLI with the MCP server, then show the answer and any
    /// changes it proposed.
    pub fn run(&mut self, req: AskRequest, cx: &mut Context<Self>) {
        if self.running.is_some() {
            return;
        }
        self.messages.push(Message {
            from_user: true,
            text: req.question.clone().into(),
            cites: Vec::new(),
            failed: false,
        });
        let cli = match req.cli {
            Ok(cli) => cli,
            Err(e) => {
                self.fail(e);
                cx.notify();
                return;
            }
        };
        let prompt = ask::ask_prompt(&req.question, &req.scope, &self.history, req.me.as_deref());
        let worker = Worker::spawn("ask", move |_, cancel| {
            cli.run(&prompt, &req.workdir, Some(&req.mcp), ASK_TIMEOUT, cancel)
                .map_err(|e| e.to_string())
        });
        self.running = Some((req.question, worker));
        self.scroll.scroll_to_bottom();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let busy = this.update(cx, |panel, cx| panel.collect(cx));
                if !busy.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// Stop the question being answered.
    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some((_, w)) = &self.running {
            w.cancel();
        }
        cx.notify();
    }

    fn fail(&mut self, e: String) {
        self.messages.push(Message {
            from_user: false,
            text: e.into(),
            cites: Vec::new(),
            failed: true,
        });
    }

    /// Check on the running question; `true` while it still runs.
    fn collect(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((question, w)) = &self.running else {
            return false;
        };
        let result = match w.poll() {
            Poll::Pending => return true,
            Poll::Done(r) => r,
            Poll::Lost => Err("the CLI stopped unexpectedly".into()),
        };
        let question = question.clone();
        self.running = None;
        match result {
            Ok(answer) => {
                let (text, cites) = ask::parse_answer(&answer);
                self.history.push((question, text.clone()));
                self.messages.push(Message {
                    from_user: false,
                    text: text.into(),
                    cites,
                    failed: false,
                });
            }
            Err(e) if e == "cancelled" => self.fail("Stopped.".into()),
            Err(e) => self.fail(e),
        }
        self.load_pending(cx);
        self.scroll.scroll_to_bottom();
        cx.notify();
        false
    }

    fn load_pending(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            match store.lock() {
                Ok(s) => s.pending().unwrap_or_default(),
                Err(_) => Vec::new(),
            }
        });
        cx.spawn(async move |this, cx| {
            let pending = task.await;
            let _ = this.update(cx, |panel, cx| {
                panel.pending = pending
                    .into_iter()
                    .map(|p| {
                        let what = switchscribe_agent::mcp::describe_pending(&p.tool, &p.payload);
                        let details = switchscribe_agent::mcp::pending_details(&p.tool, &p.payload);
                        (p, what, details)
                    })
                    .collect();
                panel.scroll.scroll_to_bottom();
                cx.notify();
            });
        })
        .detach();
    }

    /// Approve or deny a proposed change.
    fn decide(&mut self, id: i64, approve: bool, cx: &mut Context<Self>) {
        let Some(i) = self.pending.iter().position(|(p, _, _)| p.id == id) else {
            return;
        };
        let (action, _, _) = self.pending.remove(i);
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let mut s = store
                .lock()
                .map_err(|_| "the index is unavailable".to_owned())?;
            let note = if approve {
                Some(
                    switchscribe_agent::mcp::apply_pending(&mut s, &action)
                        .map_err(|e| e.to_string())?,
                )
            } else {
                None
            };
            s.remove_pending(action.id).map_err(|e| e.to_string())?;
            Ok::<_, String>(note)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |panel, cx| {
                match result {
                    Ok(Some(note)) => {
                        panel.messages.push(Message {
                            from_user: false,
                            text: note.into(),
                            cites: Vec::new(),
                            failed: false,
                        });
                        cx.emit(AskEvent::Changed);
                    }
                    Ok(None) => {}
                    Err(e) => panel.fail(format!("Could not apply the change: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Follow the CLI selector.
    pub fn set_cli(&mut self, cli: CliKind, cx: &mut Context<Self>) {
        self.cli = cli;
        cx.notify();
    }

    /// Put the cursor in the question field.
    pub fn focus_input(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |s, cx| s.focus(window, cx));
    }

    fn set_scope(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        self.scope = scope;
        let hint = match scope {
            Scope::Meeting => "Ask about this meeting",
            Scope::All => "Ask across all meetings",
        };
        self.input
            .update(cx, |s, cx| s.set_placeholder(hint, window, cx));
        cx.notify();
    }

    fn ask(&mut self, question: String, cx: &mut Context<Self>) {
        let question = question.trim().to_owned();
        if question.is_empty() || self.running.is_some() {
            return;
        }
        cx.emit(AskEvent::Ask(question));
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let q = self.input.read(cx).value().to_string();
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        self.ask(q, cx);
    }
}

impl Render for AskPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let scopes = div()
            .flex()
            .flex_none()
            .border_1()
            .border_color(p.bd2)
            .rounded(px(6.))
            .overflow_hidden()
            .text_size(ts::LABEL)
            .children(
                [
                    (Scope::Meeting, "This meeting"),
                    (Scope::All, "All meetings"),
                ]
                .map(|(scope, label)| {
                    let on = self.scope == scope;
                    div()
                        .id(label)
                        .px(rpx(8.))
                        .py(rpx(3.))
                        .cursor_pointer()
                        .when(on, |d| d.bg(p.bd2))
                        .text_color(if on { p.fg } else { p.fg2 })
                        .on_click(cx.listener(move |this, _, w, cx| this.set_scope(scope, w, cx)))
                        .child(label)
                }),
            );
        let header = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .px(rpx(14.))
            .py(rpx(12.))
            .border_b_1()
            .border_color(p.bd)
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Ask"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(5.))
                    .px(rpx(7.))
                    .py(rpx(1.))
                    .rounded(px(4.))
                    .bg(p.chip)
                    .border_1()
                    .border_color(p.bd2)
                    .font_family(MONO)
                    .text_size(ts::SMALL)
                    .text_color(p.fg_ctl)
                    .child(ui::dot(6., p.ok))
                    .child(cli_key(self.cli)),
            )
            .child(div().flex_1())
            .child(scopes);

        let thinking = self.running.is_some();
        let body = if self.messages.is_empty() && self.pending.is_empty() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .justify_center()
                .gap(rpx(6.))
                .p(rpx(14.))
                .text_size(ts::BODY)
                .child(
                    div()
                        .text_color(p.fg_ctl)
                        .font_weight(FontWeight::MEDIUM)
                        .child("Ask about your meetings"),
                )
                .child(div().text_color(p.fg3).line_height(rpx(19.)).child(
                    "Answers cite the meeting and moment they come from; a click plays the audio.",
                ))
                .into_any_element()
        } else {
            div()
                .id("ask-messages")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .p(rpx(14.))
                .flex()
                .flex_col()
                .gap(rpx(14.))
                .children(self.messages.iter().enumerate().map(|(i, m)| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(rpx(6.))
                        .map(|d| {
                            if m.from_user {
                                d.items_end()
                            } else {
                                d.items_start()
                            }
                        })
                        .child(
                            div()
                                .max_w(gpui_kit::relative(0.92))
                                .px(rpx(11.))
                                .py(rpx(9.))
                                .rounded(px(8.))
                                .line_height(rpx(20.))
                                .text_color(if m.failed { p.warn_fg } else { p.text })
                                .when(m.from_user, |d| d.bg(p.raised))
                                .child(m.text.clone()),
                        )
                        .when(!m.cites.is_empty(), |d| {
                            d.child(div().flex().flex_wrap().gap(rpx(4.)).children(
                                m.cites.iter().enumerate().map(|(j, c)| {
                                    let at = c.at_ms / 1000;
                                    let meeting = c.meeting_id.clone();
                                    ui::stamp(("cite", i * 100 + j), at, true, &p).on_click(
                                        cx.listener(move |_, _, _, cx| {
                                            cx.emit(AskEvent::Cite(meeting.clone(), at))
                                        }),
                                    )
                                }),
                            ))
                        })
                }))
                .when(thinking, |d| {
                    d.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(rpx(8.))
                            .text_size(ts::BODY)
                            .text_color(p.fg3)
                            .child(format!("{} is reading your meetings…", self.cli.label()))
                            .child(
                                div()
                                    .id("ask-stop")
                                    .text_color(p.stamp)
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                                    .child("Stop"),
                            ),
                    )
                })
                .children(self.pending.iter().map(|(action, what, details)| {
                    let id = action.id;
                    div()
                        .flex()
                        .flex_col()
                        .gap(rpx(8.))
                        .p(rpx(10.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(p.bd3)
                        .bg(p.surface)
                        .text_size(ts::BODY)
                        .child(
                            div()
                                .text_color(p.fg3)
                                .text_size(ts::SMALL)
                                .child("The agent wants to"),
                        )
                        .child(div().text_color(p.fg).child(what.clone()))
                        .children(details.iter().take(6).map(|line| {
                            div()
                                .pl(rpx(8.))
                                .border_l_2()
                                .border_color(p.bd3)
                                .text_color(p.fg2)
                                .child(line.clone())
                        }))
                        .child(
                            div()
                                .flex()
                                .gap(rpx(6.))
                                .child(
                                    ui::button(
                                        ("approve", id as usize),
                                        "Approve",
                                        Kind::Primary,
                                        &p,
                                    )
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.decide(id, true, cx)
                                        }),
                                    ),
                                )
                                .child(
                                    ui::button(("deny", id as usize), "Deny", Kind::Outline, &p)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.decide(id, false, cx)
                                        })),
                                ),
                        )
                }))
                .into_any_element()
        };

        let prompts = div()
            .flex()
            .flex_wrap()
            .gap(rpx(6.))
            .children(PROMPTS.map(|label| {
                let hover = p.elev;
                div()
                    .id(label)
                    .px(rpx(8.))
                    .py(rpx(3.))
                    .rounded(px(12.))
                    .border_1()
                    .border_color(p.bd2)
                    .text_size(ts::LABEL)
                    .text_color(p.fg_ctl)
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.ask(label.to_owned(), cx)))
                    .child(label)
            }));
        let composer = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(rpx(8.))
            .px(rpx(14.))
            .pt(rpx(10.))
            .pb(rpx(14.))
            .border_t_1()
            .border_color(p.bd)
            .child(prompts)
            .child(
                div()
                    .flex()
                    .gap(rpx(6.))
                    .child(div().flex_1().child(Input::new(&self.input)))
                    .child(
                        ui::button("ask-send", "Send", Kind::Primary, &p)
                            .h(rpx(32.))
                            .on_click(cx.listener(|this, _, w, cx| this.send(w, cx))),
                    ),
            );

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.panel)
            .child(header)
            .child(body)
            .child(composer)
    }
}

/// The CLI's short name in the panel header (`claude-code`).
fn cli_key(cli: CliKind) -> &'static str {
    match cli {
        CliKind::ClaudeCode => "claude-code",
        CliKind::Codex => "codex",
        CliKind::GeminiCli => "gemini-cli",
    }
}
