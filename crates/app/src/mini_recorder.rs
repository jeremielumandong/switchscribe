//! The mini recorder: a borderless, always-on-top pill (340 × 56) shown while recording,
//! with the pulsing dot, elapsed time, "You" / "Teams" level meters, and Bookmark, Quick
//! note, Pause and Stop. Drag it anywhere. Hovering grows it to show the meeting's title
//! and the latest live caption under the controls (below rather than above, as the
//! prototype has it, so the buttons stay put under the pointer).

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    WeakEntity, Window, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div,
    point, px, size,
};
use switchscribe_capture::Track;

use crate::appearance::{rpx, ts};
use crate::format;
use crate::theme::{MONO, Palette, SANS, palette};
use crate::workspace::Workspace;

/// The pill's size, logical pixels.
const WIDTH: f32 = 340.;
const HEIGHT: f32 = 56.;
/// Its height while hovered.
const EXPANDED: f32 = 100.;

/// The mini recorder's view. It reads everything from the workspace's session.
pub struct MiniRecorder {
    ws: WeakEntity<Workspace>,
    hovered: bool,
}

/// Open the mini recorder near the bottom center of the primary display.
pub fn open(ws: WeakEntity<Workspace>, cx: &mut App) -> Option<AnyWindowHandle> {
    let display = cx.primary_display().map(|d| d.bounds());
    let origin = display.map_or(point(px(100.), px(100.)), |b| {
        point(
            b.origin.x + (b.size.width - px(WIDTH)) / 2.,
            b.origin.y + b.size.height - px(HEIGHT + 96.),
        )
    });
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin,
            size: size(px(WIDTH), px(HEIGHT)),
        })),
        titlebar: None,
        kind: WindowKind::PopUp,
        focus: false,
        show: true,
        is_movable: true,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        app_id: Some("switchscribe".into()),
        ..Default::default()
    };
    match gpui_kit::open_window(options, cx, |_, cx| {
        cx.new(|_| MiniRecorder { ws, hovered: false })
    }) {
        Ok((handle, _)) => Some(handle),
        Err(e) => {
            tracing::warn!(error = %e, "could not open the mini recorder");
            None
        }
    }
}

/// Close it.
pub fn close(handle: AnyWindowHandle, cx: &mut App) {
    let _ = handle.update(cx, |_, window, _| window.remove_window());
}

fn round_button(id: &'static str, p: &Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    let hover = p.bd2;
    div()
        .id(id)
        .size(rpx(30.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(3.))
        .rounded_full()
        .cursor_pointer()
        .text_color(p.fg_chip)
        .hover(move |s| s.bg(hover))
        // Keep clicks on the buttons from starting a window drag.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

impl Render for MiniRecorder {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let state = self.ws.upgrade().and_then(|ws| {
            let ws = ws.read(cx);
            ws.recording.as_ref().map(|s| {
                let caption = s.captions.as_ref().and_then(|c| c.last()).map(|b| {
                    let who = match b.track {
                        Track::Mic => ws
                            .people
                            .iter()
                            .find(|x| x.is_me)
                            .map_or_else(|| "You".to_owned(), |m| m.name.clone()),
                        Track::Remote => "Teams".to_owned(),
                    };
                    format!("{who}: {}", b.text)
                });
                (
                    s.elapsed_secs(),
                    s.pulse(),
                    s.paused(),
                    [s.monitor.level(Track::Mic), s.monitor.level(Track::Remote)],
                    s.title.clone(),
                    caption,
                )
            })
        });
        let (elapsed, pulse, paused, levels, title, caption) =
            state.unwrap_or((0, 0.5, false, [0.0; 2], String::new(), None));
        let expanded = self.hovered;
        let ws = self.ws.clone();
        let call = move |f: fn(&mut Workspace, &mut Context<Workspace>)| {
            let ws = ws.clone();
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                let _ = ws.update(cx, f);
            }
        };
        let meters = div()
            .w(rpx(78.))
            .flex_none()
            .flex()
            .flex_col()
            .gap(rpx(4.))
            .children(Track::ALL.iter().zip(levels).map(|(track, level)| {
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(5.))
                    .child(
                        div()
                            .w(rpx(32.))
                            .text_size(ts::TINY)
                            .text_color(p.fg2)
                            .child(track.meter_label()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h(rpx(4.))
                            .rounded(px(2.))
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
        let pause_bar = || div().w(px(3.)).h(px(11.)).rounded(px(1.)).bg(p.fg_chip);
        let row = div()
            .h(px(HEIGHT - 2.))
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(12.))
            .child(
                div()
                    .size(rpx(9.))
                    .flex_none()
                    .rounded_full()
                    .bg(p.rec)
                    .opacity(pulse),
            )
            .child(
                div()
                    .w(rpx(56.))
                    .flex_none()
                    .font_family(MONO)
                    .text_size(ts::BASE)
                    .text_color(p.rec_fg)
                    .child(format::clock(elapsed)),
            )
            .child(meters)
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .child(
                        round_button("mini-bookmark", &p)
                            .text_size(ts::SMALL)
                            .on_click(call(Workspace::bookmark))
                            .child("◆"),
                    )
                    .child(
                        round_button("mini-note", &p)
                            .text_size(ts::BODY)
                            .on_click(call(Workspace::quick_note))
                            .child("✎"),
                    )
                    .child(
                        round_button("mini-pause", &p)
                            .on_click(call(Workspace::toggle_pause))
                            .map(|d| {
                                if paused {
                                    // Resume: a play triangle.
                                    d.child(
                                        gpui_kit::svg()
                                            .path("icons/play.svg")
                                            .size(rpx(12.))
                                            .text_color(p.fg_chip),
                                    )
                                } else {
                                    d.child(pause_bar()).child(pause_bar())
                                }
                            }),
                    )
                    .child(
                        div()
                            .id("mini-stop")
                            .size(rpx(30.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(p.rec)
                            .cursor_pointer()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(call(Workspace::stop_recording))
                            .child(div().size(px(10.)).rounded(px(1.5)).bg(gpui_kit::white())),
                    ),
            );
        div()
            .id("mini")
            .size_full()
            .flex()
            .flex_col()
            .pl(rpx(18.))
            .pr(rpx(8.))
            .rounded(px(if expanded { 20. } else { 28. }))
            .border_1()
            .border_color(p.pill_bd)
            .bg(p.pill)
            .font_family(SANS)
            .text_color(p.fg)
            .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
            .on_hover(cx.listener(|this, hovered: &bool, window, cx| {
                if this.hovered != *hovered {
                    this.hovered = *hovered;
                    let h = if *hovered { EXPANDED } else { HEIGHT };
                    window.resize(size(px(WIDTH), px(h)));
                    cx.notify();
                }
            }))
            .child(row)
            .when(expanded, |d| {
                d.child(
                    div()
                        .pr(rpx(10.))
                        .pb(rpx(10.))
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .child(
                            div()
                                .truncate()
                                .text_size(ts::BODY)
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .child(title),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(ts::BODY)
                                .text_color(p.fg2)
                                .child(caption.unwrap_or_else(|| "No live captions".into())),
                        ),
                )
            })
            .when(self.ws.upgrade().is_none(), |d| d.opacity(0.5))
    }
}
