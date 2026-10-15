//! A meeting after the call: header, the processing stepper, the Summary, Transcript,
//! Action items and Notes tabs, and the audio player. The transcript is real (from the
//! store); the summary and action items are demo content until M4 produces them.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
};
use switchscribe_pipeline::Step;
use switchscribe_store::{Meeting, MeetingStatus, Segment};

use crate::agent_view::{self, SummaryJob};
use crate::appearance::{rpx, ts};
use crate::demo;
use crate::format;
use crate::player_bar;
use crate::processing::Transcription;
use crate::recording::{self, NotesDoc};
use crate::speakers;
use crate::theme::{MONO, Palette};
use crate::ui::{self, Kind};
use crate::workspace::{Tab, Workspace};

/// The selected meeting's pane.
pub fn render(ws: &Workspace, m: &Meeting, p: &Palette, cx: &mut Context<Workspace>) -> AnyElement {
    let featured = ws.demo && m.id == demo::FEATURED;
    let transcript: &[Segment] = match &ws.transcript {
        Some((id, segs)) if *id == m.id => segs,
        _ => &[],
    };
    let transcribing = ws.jobs.transcription(&m.id);
    // The transcript's lines sit directly in the scroll container, so playback can
    // scroll to one.
    let rows: Vec<AnyElement> = if ws.tab == Tab::Transcript && !transcript.is_empty() {
        // The Name-a-speaker card comes first (see `scroll_to_playhead`).
        ws.naming_cluster()
            .map(|c| speakers::name_card(ws, c, transcript, p, cx))
            .into_iter()
            .chain(transcript_rows(ws, transcript, p, cx))
            .collect()
    } else {
        Vec::new()
    };
    let body: AnyElement = match ws.tab {
        Tab::Summary => agent_view::summary_tab(ws, m, p, cx),
        Tab::Transcript if transcript.is_empty() => {
            let body = match &transcribing {
                Some(Transcription::NeedsModel) => {
                    "Download a transcription model in Settings and this meeting is transcribed on this machine.".to_owned()
                }
                Some(Transcription::Failed(e)) => format!("Transcription failed: {e}"),
                Some(_) => "Transcribing on this machine. The transcript appears here when it is done.".to_owned(),
                None => "Each track is transcribed on this machine after the call.".to_owned(),
            };
            ui::empty_state("No transcript yet", body, p).into_any_element()
        }
        Tab::Transcript => div().into_any_element(),
        Tab::Actions => agent_view::actions_tab(ws, m, p, cx),
        Tab::Notes if featured => notes(p, cx),
        Tab::Notes => match &ws.notes_doc {
            Some((id, doc)) if *id == m.id && !doc.is_empty() => recorded_notes(doc, p, cx),
            _ => pending(
                "No notes",
                "Notes and bookmarks you take in the live view during the call appear here, anchored to their moments.",
                p,
            ),
        },
    };
    let action_count = ws
        .agent
        .meeting
        .as_ref()
        .filter(|(id, _, _)| *id == m.id)
        .map_or(0, |(_, _, items)| items.len());
    let player = ws
        .playback
        .as_ref()
        .filter(|pb| pb.meeting_id == m.id)
        .map(|pb| player_bar::render(ws, pb, p, cx));
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(header(
            ws,
            m,
            transcript.len(),
            action_count,
            transcribing,
            p,
            cx,
        ))
        .child(
            div()
                .id("meeting-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&ws.body_scroll)
                .flex()
                .flex_col()
                .when(rows.is_empty(), |d| d.child(body))
                .when(!rows.is_empty(), |d| {
                    d.px(rpx(12.)).pt(rpx(8.)).pb(rpx(20.)).children(rows)
                }),
        )
        .children(player)
        .into_any_element()
}

fn header(
    ws: &Workspace,
    m: &Meeting,
    segments: usize,
    action_count: usize,
    transcribing: Option<Transcription>,
    p: &Palette,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let attendees = div()
        .flex()
        .flex_wrap()
        .gap(rpx(6.))
        .children(m.attendees.iter().map(|a| {
            div()
                .flex()
                .items_center()
                .gap(rpx(5.))
                .pl(rpx(2.))
                .pr(rpx(7.))
                .py(px(1.))
                .rounded(px(10.))
                .bg(p.chip)
                .border_1()
                .border_color(p.bd2)
                .text_color(p.fg_chip)
                .child(ui::avatar(a.initials(), p.speaker(a.color), 16., true, p))
                .child(a.name.clone())
        }));
    let buttons = div()
        .flex()
        .flex_none()
        .gap(rpx(6.))
        .child(
            ui::button("rerun", "Re-run summary", Kind::Secondary, p).on_click({
                let id = m.id.clone();
                cx.listener(move |ws, _, _, cx| ws.queue_summary(id.clone(), cx))
            }),
        )
        .child(crate::integrations_view::meeting_jira_button(
            ws, &m.id, p, cx,
        ))
        .child(crate::integrations_view::export_button(ws, p, cx));
    let tabs = div().flex().gap(px(2.)).mt(rpx(14.)).children(
        [
            (Tab::Summary, "Summary", String::new()),
            (Tab::Transcript, "Transcript", count(segments)),
            (Tab::Actions, "Action items", count(action_count)),
            (Tab::Notes, "Notes", String::new()),
        ]
        .map(|(tab, label, n)| {
            let on = ws.tab == tab;
            div()
                .id(label)
                .px(rpx(12.))
                .py(rpx(8.))
                .flex()
                .items_center()
                .gap(rpx(6.))
                .cursor_pointer()
                .font_weight(FontWeight::MEDIUM)
                .text_color(if on { p.fg } else { p.fg2 })
                .border_b_2()
                .border_color(if on {
                    p.fg
                } else {
                    gpui_kit::transparent_black()
                })
                .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
                .child(label)
                .child(ui::mono(n, ts::SMALL, p.fg4))
        }),
    );
    div()
        .flex_none()
        .px(rpx(20.))
        .pt(rpx(16.))
        .border_b_1()
        .border_color(p.bd)
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_start()
                .gap(rpx(12.))
                .child(
                    div()
                        .flex_1()
                        .min_w(rpx(280.))
                        .child(
                            div()
                                .text_size(ts::DISPLAY)
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(m.title.clone()),
                        )
                        .child(
                            div()
                                .mt(rpx(6.))
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap(rpx(12.))
                                .text_size(ts::BODY)
                                .text_color(p.fg2)
                                .child(format::date_long(m))
                                .child(ui::mono(
                                    format::clock(i64::from(m.duration_secs)),
                                    ts::BODY,
                                    p.fg2,
                                ))
                                .child(m.source_app.clone())
                                .child(attendees),
                        ),
                )
                .child(buttons),
        )
        .map(|d| {
            let summarizing = ws
                .agent
                .job(&m.id)
                .filter(|j| !matches!(j, SummaryJob::Failed(_)));
            if m.status == MeetingStatus::Processing || summarizing.is_some() {
                d.child(stepper(
                    ws.demo && m.id == demo::FEATURED,
                    m.status == MeetingStatus::Processing,
                    transcribing,
                    summarizing,
                    p,
                ))
            } else {
                d
            }
        })
        .child(tabs)
        .into_any_element()
}

fn count(n: usize) -> String {
    if n == 0 { String::new() } else { n.to_string() }
}

/// Capture → Transcribe → Identify speakers → Summarize. The demo shows the prototype's
/// progress; a real recording shows its transcription, speaker identification and
/// summary.
fn stepper(
    featured: bool,
    processing: bool,
    transcribing: Option<Transcription>,
    summarizing: Option<SummaryJob>,
    p: &Palette,
) -> AnyElement {
    let progress: [(f32, String); 4] = if featured {
        [
            (1.0, "done".into()),
            (0.64, "~1m 40s".into()),
            (0.0, "~30s".into()),
            (0.0, "~45s".into()),
        ]
    } else {
        let pct = |f: f32| (f.max(0.01), format!("{:.0}%", f * 100.0));
        let done = || (1.0, "done".to_owned());
        let (transcribe, identify) = match transcribing {
            _ if !processing => (done(), done()),
            Some(Transcription::Running(f)) => (pct(f), (0.0, String::new())),
            Some(Transcription::Identifying(f)) => ((1.0, "done".into()), pct(f)),
            Some(Transcription::Queued) | None => ((0.0, "queued".into()), (0.0, String::new())),
            Some(Transcription::NeedsModel) => {
                ((0.0, "needs a model".into()), (0.0, String::new()))
            }
            Some(Transcription::Failed(_)) => ((0.0, "failed".into()), (0.0, String::new())),
        };
        let summarize = match summarizing {
            // How long the CLI takes is unknown; show it as under way.
            Some(SummaryJob::Running) => (0.5, "writing".to_owned()),
            Some(SummaryJob::Queued) => (0.0, "queued".to_owned()),
            _ => (0.0, String::new()),
        };
        [(1.0, "done".into()), transcribe, identify, summarize]
    };
    div()
        .mt(rpx(14.))
        .flex()
        .gap(rpx(8.))
        .children(
            Step::ALL
                .iter()
                .zip(progress)
                .enumerate()
                .map(|(i, (step, (pct, eta)))| {
                    let pending = pct == 0.0;
                    div()
                        .flex_1()
                        .min_w_0()
                        .p(rpx(10.))
                        .flex()
                        .flex_col()
                        .gap(rpx(7.))
                        .border_1()
                        .border_color(p.bd2)
                        .rounded(px(6.))
                        .bg(p.surface)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(rpx(6.))
                                .text_size(ts::BODY)
                                .text_color(if pending { p.fg3 } else { p.fg })
                                .child(ui::mono((i + 1).to_string(), ts::SMALL, p.fg4))
                                .child(
                                    div()
                                        .flex_1()
                                        .truncate()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(step.label()),
                                )
                                .child(ui::mono(eta, ts::SMALL, p.fg3)),
                        )
                        .child(
                            div()
                                .h(rpx(3.))
                                .rounded(px(2.))
                                .bg(p.bd)
                                .overflow_hidden()
                                .child(
                                    div().h_full().w(gpui_kit::relative(pct)).bg(if pct >= 1.0 {
                                        p.fg2
                                    } else {
                                        p.fg
                                    }),
                                ),
                        )
                }),
        )
        .into_any_element()
}

fn pending(title: &'static str, body: &'static str, p: &Palette) -> AnyElement {
    ui::empty_state(title, body, p).into_any_element()
}

fn transcript_rows(
    ws: &Workspace,
    segments: &[Segment],
    p: &Palette,
    cx: &mut Context<Workspace>,
) -> Vec<AnyElement> {
    let active = player_bar::active_segment(segments, ws.playhead).unwrap_or(0);
    segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let person = s.person_id.and_then(|id| ws.person(id));
            let (name, color, named): (SharedString, _, bool) = match person {
                Some(person) => (person.name.clone().into(), p.speaker(person.color), true),
                None => (s.speaker_label.clone().into(), p.unknown, false),
            };
            let on = i == active;
            let relabel = on && ws.can_relabel(&s.speaker_label);
            let at = s.start_ms / 1000;
            div()
                .id(("seg", i))
                .flex()
                .items_start()
                .gap(rpx(12.))
                .p(rpx(8.))
                .rounded(px(6.))
                .border_l_2()
                .border_color(if on {
                    color
                } else {
                    gpui_kit::transparent_black()
                })
                .cursor_pointer()
                .when(on, |d| d.bg(p.surface))
                .when(!on, |d| d.hover(|s| s.bg(p.panel)))
                .on_click(cx.listener(move |this, _, _, cx| this.cue(at, cx)))
                .child(
                    div()
                        .w(rpx(150.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .child(
                            div()
                                .px(rpx(7.))
                                .py(px(1.))
                                .rounded(px(4.))
                                .border_1()
                                .when(!named, |d| d.border_dashed())
                                .border_color(if named { color } else { p.fg4 })
                                .text_size(ts::BODY)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(if named { color } else { p.fg_ctl })
                                .truncate()
                                .child(name),
                        )
                        .when(s.low_confidence, |d| {
                            d.child(
                                div()
                                    .size(rpx(14.))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .border_1()
                                    .border_color(c4a(p))
                                    .font_family(MONO)
                                    .text_size(ts::TINY)
                                    .text_color(p.fg2)
                                    .child("?"),
                            )
                        }),
                )
                .child(div().w(rpx(64.)).flex_none().pt(px(2.)).child(ui::mono(
                    format::clock(at),
                    ts::SMALL,
                    p.fg3,
                )))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .line_height(rpx(20.))
                        .text_color(p.text)
                        .child(s.text.clone()),
                )
                .when(on, |d| {
                    d.child(
                        div()
                            .flex()
                            .flex_none()
                            .gap(px(2.))
                            .child(seg_tool("seg-play", "Play", p).cursor_pointer().on_click(
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.play_from(at, cx)
                                }),
                            ))
                            .children(
                                ["Copy", "Edit"]
                                    .map(|label| seg_tool(label, label, p).opacity(0.6)),
                            )
                            .when(relabel, |d| {
                                let label = s.speaker_label.clone();
                                d.child(
                                    seg_tool("seg-relabel", "Relabel", p)
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.relabel(label.clone(), cx)
                                        })),
                                )
                            }),
                    )
                })
                .into_any_element()
        })
        .collect()
}

/// A small tool chip on the active transcript line.
fn seg_tool(
    id: &'static str,
    label: &'static str,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .px(rpx(6.))
        .py(px(2.))
        .rounded(px(4.))
        .bg(p.raised)
        .text_size(ts::SMALL)
        .text_color(p.fg_ctl)
        .child(label)
}

/// The low-confidence badge's ring (#4a4d55).
fn c4a(p: &Palette) -> gpui_kit::Hsla {
    p.fg4.opacity(0.8)
}

fn notes(p: &Palette, cx: &mut Context<Workspace>) -> AnyElement {
    div()
        .max_w(rpx(760.))
        .p(rpx(20.))
        .flex()
        .flex_col()
        .gap(rpx(10.))
        .font_family(MONO)
        .text_size(ts::UI)
        .line_height(rpx(21.))
        .text_color(p.fg_chip)
        .child(
            div()
                .text_color(p.fg)
                .font_weight(FontWeight::MEDIUM)
                .child("## My notes"),
        )
        .children(
            demo::notes()
                .into_iter()
                .enumerate()
                .map(|(i, (at, text))| {
                    div()
                        .flex()
                        .gap(rpx(10.))
                        .child(
                            div()
                                .id(("note", i))
                                .flex_none()
                                .text_color(p.stamp)
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| this.cue(at, cx)))
                                .child(format!("[{}]", format::clock(at))),
                        )
                        .child(text)
                }),
        )
        .into_any_element()
}

/// A recorded meeting's `notes.md`, with `[mm:ss]` anchors as links to their moments, and
/// its bookmarks.
fn recorded_notes(doc: &NotesDoc, p: &Palette, cx: &mut Context<Workspace>) -> AnyElement {
    div()
        .max_w(rpx(760.))
        .p(rpx(20.))
        .flex()
        .flex_col()
        .gap(rpx(4.))
        .font_family(MONO)
        .text_size(ts::UI)
        .line_height(rpx(21.))
        .text_color(p.fg_chip)
        .children(doc.text.lines().enumerate().map(|(i, line)| {
            match recording::parse_anchor(line) {
                Some((at, rest)) => div()
                    .flex()
                    .gap(rpx(10.))
                    .child(
                        div()
                            .id(("note", i))
                            .flex_none()
                            .text_color(p.stamp)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| this.cue(at, cx)))
                            .child(format!("[{}]", format::clock(at))),
                    )
                    .child(rest.to_owned()),
                // Keep blank lines as spacing.
                None => div().min_h(rpx(21.)).child(line.to_owned()),
            }
        }))
        .when(!doc.bookmarks.is_empty(), |d| {
            d.child(
                ui::section_label("Bookmarks", p.fg3)
                    .mt(rpx(14.))
                    .font_family(crate::theme::SANS),
            )
            .child(div().flex().flex_wrap().gap(rpx(6.)).children(
                doc.bookmarks.iter().enumerate().map(|(i, &at)| {
                    ui::stamp(("bookmark", i), at, true, p)
                        .on_click(cx.listener(move |this, _, _, cx| this.cue(at, cx)))
                }),
            ))
        })
        .into_any_element()
}
