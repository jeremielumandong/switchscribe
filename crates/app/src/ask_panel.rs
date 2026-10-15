//! The Ask panel (right, 360 px): chat with the coding CLI, scoped to this meeting or all
//! meetings. M0 lays it out and keeps the conversation locally; the CLI behind it, its MCP
//! tools and the approve/deny cards arrive in M4.

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, px,
};
use switchscribe_store::CliKind;

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
    /// Cited moments, seconds.
    cites: Vec<i64>,
}

/// A request from the panel to the workspace.
pub enum AskEvent {
    /// A citation chip was clicked: jump to this moment of the selected meeting.
    Cite(i64),
}

impl gpui_kit::EventEmitter<AskEvent> for AskPanel {}

/// The panel's state.
pub struct AskPanel {
    input: Entity<InputState>,
    scope: Scope,
    cli: CliKind,
    messages: Vec<Message>,
    _sub: Subscription,
}

const PROMPTS: [&str; 3] = [
    "Summarize decisions",
    "What did I commit to this week?",
    "Draft Jira issues",
];

impl AskPanel {
    /// A panel with an empty conversation (or the demo's, when `demo` is set).
    pub fn new(cli: CliKind, demo: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
                    cites,
                })
                .collect()
        } else {
            Vec::new()
        };
        Self {
            input,
            scope: Scope::Meeting,
            cli,
            messages,
            _sub: sub,
        }
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
        if question.is_empty() {
            return;
        }
        self.messages.push(Message {
            from_user: true,
            text: question.into(),
            cites: Vec::new(),
        });
        self.messages.push(Message {
            from_user: false,
            text: format!(
                "Answers come from {} through switchscribe's MCP server, which arrives in M4.",
                self.cli.label()
            )
            .into(),
            cites: Vec::new(),
        });
        cx.notify();
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

        let body = if self.messages.is_empty() {
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
                                .text_color(p.text)
                                .when(m.from_user, |d| d.bg(p.raised))
                                .child(m.text.clone()),
                        )
                        .when(!m.cites.is_empty(), |d| {
                            d.child(div().flex().flex_wrap().gap(rpx(4.)).children(
                                m.cites.iter().enumerate().map(|(j, at)| {
                                    let at = *at;
                                    ui::stamp(("cite", i * 100 + j), at, true, &p).on_click(
                                        cx.listener(move |_, _, _, cx| cx.emit(AskEvent::Cite(at))),
                                    )
                                }),
                            ))
                        })
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
