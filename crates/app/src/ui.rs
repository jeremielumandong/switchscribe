//! Small pieces of the prototype's component vocabulary: buttons, chips, avatars, kbd
//! hints, timestamp chips, section headings.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Div, ElementId, FontWeight, Hsla, InteractiveElement as _, ParentElement as _, SharedString,
    Stateful, Styled as _, div, px,
};

use crate::appearance::{rpx, ts};
use crate::format;
use crate::theme::{MONO, Palette};

/// A monospace span.
pub fn mono(text: impl Into<SharedString>, size: gpui_kit::Rems, color: Hsla) -> Div {
    div()
        .font_family(MONO)
        .text_size(size)
        .text_color(color)
        .whitespace_nowrap()
        .child(text.into())
}

/// A keyboard hint (`Ctrl+J`) after a label.
pub fn kbd(keys: &'static str, p: &Palette) -> Div {
    mono(keys, ts::SMALL, p.fg4)
}

/// Button looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Light fill, dark text (Export, Confirm, Send).
    Primary,
    /// Bordered, dark fill (Re-run summary, Send to Jira).
    Secondary,
    /// Bordered, transparent (title bar controls).
    Outline,
}

/// A 28 px button.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    kind: Kind,
    p: &Palette,
) -> Stateful<Div> {
    let hover = p.elev;
    let (bg, fg, bd) = match kind {
        Kind::Primary => (p.fg, p.on_light, p.bd3),
        Kind::Secondary => (p.chip, p.fg_chip, p.bd3),
        Kind::Outline => (gpui_kit::transparent_black(), p.fg_ctl, p.bd2),
    };
    div()
        .id(id)
        .h(rpx(28.))
        .px(rpx(10.))
        .flex()
        .flex_none()
        .items_center()
        .gap(rpx(8.))
        .rounded(px(6.))
        .border_1()
        .border_color(bd)
        .bg(bg)
        .text_color(fg)
        .text_size(ts::BODY)
        .whitespace_nowrap()
        .cursor_pointer()
        .when(kind == Kind::Primary, |d| d.font_weight(FontWeight::MEDIUM))
        .when(kind != Kind::Primary, move |d| {
            d.hover(move |s| s.bg(hover))
        })
        .child(label.into())
}

/// A colored dot.
pub fn dot(size: f32, color: Hsla) -> Div {
    div().flex_none().size(rpx(size)).rounded_full().bg(color)
}

/// An attendee avatar: initials in a ring of the speaker color (sidebar rows), or on a
/// filled disc (`filled`, meeting header).
pub fn avatar(
    initials: impl Into<SharedString>,
    color: Hsla,
    size: f32,
    filled: bool,
    p: &Palette,
) -> Div {
    div()
        .flex_none()
        .size(rpx(size))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(ts::AVATAR)
        .font_weight(FontWeight::BOLD)
        .map(|d| {
            if filled {
                d.bg(color).text_color(p.on_light)
            } else {
                d.bg(p.elev)
                    .border(px(1.5))
                    .border_color(color)
                    .text_color(p.fg_chip)
            }
        })
        .child(initials.into())
}

/// An uppercase section heading (`MEETINGS`, `DECISIONS`).
pub fn section_label(text: &'static str, color: Hsla) -> Div {
    div()
        .text_size(ts::SMALL)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .child(SharedString::from(text.to_uppercase()))
}

/// The teal chip that links a claim to a moment (`▸ 03:56`).
pub fn stamp(id: impl Into<ElementId>, secs: i64, arrow: bool, p: &Palette) -> Stateful<Div> {
    let hover = p.stamp_bd;
    let label = if arrow {
        format!("▸ {}", format::clock(secs))
    } else {
        format::clock(secs)
    };
    div()
        .id(id)
        .flex_none()
        .px(rpx(6.))
        .py(rpx(1.))
        .rounded(px(4.))
        .bg(p.stamp_bg)
        .border_1()
        .border_color(p.stamp_bd)
        .font_family(MONO)
        .text_size(ts::SMALL)
        .text_color(p.stamp)
        .whitespace_nowrap()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(label)
}

/// A rounded chip with a colored dot (speaker filters, owners).
pub fn person_chip(name: impl Into<SharedString>, color: Hsla, p: &Palette) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(rpx(4.))
        .px(rpx(6.))
        .rounded(px(4.))
        .bg(p.chip)
        .border_1()
        .border_color(p.bd2)
        .text_size(ts::SMALL)
        .text_color(p.fg_chip)
        .child(dot(6., color))
        .child(name.into())
}

/// A centered empty state: title, a line of text, and optional actions under it.
pub fn empty_state(title: &'static str, body: impl Into<SharedString>, p: &Palette) -> Div {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(rpx(8.))
        .p(rpx(24.))
        .child(
            div()
                .text_size(ts::TITLE)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.fg)
                .child(title),
        )
        .child(
            div()
                .max_w(rpx(420.))
                .text_center()
                .text_color(p.fg3)
                .text_size(ts::BODY)
                .line_height(rpx(19.))
                .child(body.into()),
        )
}
