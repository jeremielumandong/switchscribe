//! The live meeting view: a header strip (pulsing dot, title, attendees, elapsed time,
//! Pause, Stop), the tracks being captured with their meters on the left (live captions
//! join them in M6), and the private notes editor with bookmarks on the right.

use gpui_kit::component::input::Textarea;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};
use switchscribe_capture::Track;

use crate::appearance::{rpx, ts};
use crate::format;
use crate::recording::Session;
use crate::theme::{MONO, Palette};
use crate::ui::{self, Kind};
use crate::workspace::Workspace;

/// The live view of the running session.
pub fn render(ws: &Workspace, s: &Session, p: &Palette, cx: &mut Context<Workspace>) -> AnyElement {
    let me = ws.people.iter().find(|x| x.is_me);
    let paused = s.paused();
    let header = div()
        .flex_none()
        .flex()
        .items_center()
        .gap(rpx(14.))
        .px(rpx(20.))
        .py(rpx(12.))
        .border_b_1()
        .border_color(p.bd)
        .child(ui::dot(9., p.rec).opacity(s.pulse()))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(ts::LARGE)
                .font_weight(FontWeight::SEMIBOLD)
                .child(s.title.clone()),
        )
        .when_some(me, |d, me| {
            d.child(ui::avatar(me.initials(), p.speaker(me.color), 22., true, p))
        })
        .when(paused, |d| d.child(ui::mono("PAUSED", ts::SMALL, p.fg2)))
        .child(div().flex_1())
        .child(ui::mono(
            format::clock(s.elapsed_secs()),
            ts::LARGE,
            p.rec_fg,
        ))
        .child(
            ui::button(
                "live-pause",
                if paused { "Resume" } else { "Pause" },
                Kind::Outline,
                p,
            )
            .h(rpx(30.))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_pause(cx))),
        )
        .child(
            div()
                .id("live-stop")
                .h(rpx(30.))
                .px(rpx(14.))
                .flex()
                .flex_none()
                .items_center()
                .gap(rpx(8.))
                .rounded(px(6.))
                .bg(p.rec)
                .text_color(gpui_kit::white())
                .text_size(ts::BODY)
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| this.stop_recording(cx)))
                .child(div().size(px(9.)).rounded(px(1.)).bg(gpui_kit::white()))
                .child("Stop"),
        );

    let tracks = div()
        .flex()
        .flex_col()
        .gap(rpx(10.))
        .children(Track::ALL.map(|track| {
            let level = s.monitor.level(track);
            div()
                .flex()
                .items_center()
                .gap(rpx(12.))
                .p(rpx(12.))
                .rounded(px(6.))
                .border_1()
                .border_color(p.bd2)
                .bg(p.surface)
                .child(
                    gpui_kit::svg()
                        .path(match track {
                            Track::Mic => "icons/mic.svg",
                            Track::Remote => "icons/audio-lines.svg",
                        })
                        .size(rpx(14.))
                        .text_color(p.fg2),
                )
                .child(
                    div()
                        .w(rpx(140.))
                        .flex_none()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .child(track.meter_label()),
                        )
                        .child(
                            div()
                                .text_size(ts::SMALL)
                                .text_color(p.fg3)
                                .child(track.file_name()),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .h(rpx(6.))
                        .rounded(px(3.))
                        .bg(p.meter_bg)
                        .overflow_hidden()
                        .child(
                            div()
                                .h_full()
                                .w(gpui_kit::relative(level.clamp(0.0, 1.0)))
                                .bg(p.fg_chip),
                        ),
                )
        }));

    let left = div()
        .id("live-left")
        .flex_grow(2.)
        .flex_basis(px(0.))
        .min_w_0()
        .overflow_y_scroll()
        .px(rpx(20.))
        .py(rpx(16.))
        .flex()
        .flex_col()
        .gap(rpx(14.))
        .border_r_1()
        .border_color(p.bd)
        .child(ui::section_label("Recording", p.fg3))
        .child(tracks)
        .child(ui::section_label("Live captions", p.fg3).mt(rpx(10.)))
        .child(
            div()
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .line_height(rpx(19.))
                .child(
                    "Captions stream here from a small on-device model in M6. The full \
                     transcript is made after the call (M2).",
                ),
        );

    let bookmarks = div()
        .w_full()
        .flex()
        .flex_col()
        .gap(rpx(6.))
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .child(ui::section_label("Bookmarks", p.fg3))
                .child(div().flex_1())
                .child(
                    div()
                        .id("live-bookmark")
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .text_size(ts::BODY)
                        .text_color(p.fg_ctl)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.bookmark(cx)))
                        .child("+ Bookmark")
                        .child(ui::kbd("Ctrl+Shift+B", p)),
                ),
        )
        .when(s.bookmarks.is_empty(), |d| {
            d.child(
                div()
                    .text_size(ts::BODY)
                    .text_color(p.fg4)
                    .child("Mark a moment to come back to it after the call."),
            )
        })
        .children(s.bookmarks.iter().enumerate().map(|(i, ms)| {
            let at = (ms / 1000) as i64;
            div()
                .flex()
                .items_center()
                .gap(rpx(8.))
                .text_size(ts::BODY)
                .text_color(p.fg_chip)
                .child(ui::mono(format::clock(at), ts::SMALL, p.stamp))
                .child(format!("Bookmark {}", i + 1))
        }));

    let right = div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .px(rpx(16.))
                .pt(rpx(12.))
                .pb(rpx(6.))
                .child(ui::section_label("Private notes", p.fg3))
                .child(ui::mono("Markdown", ts::SMALL, p.fg4)),
        )
        .child(
            div()
                .flex_1()
                .min_h_0()
                .mx(rpx(16.))
                .font_family(MONO)
                .text_size(ts::BODY)
                .child(Textarea::new(&s.notes).h_full()),
        )
        .child(
            div()
                .flex_none()
                .px(rpx(16.))
                .pt(rpx(12.))
                .pb(rpx(16.))
                .child(bookmarks),
        );

    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(header)
        .child(div().flex_1().min_h_0().flex().child(left).child(right))
        .into_any_element()
}
