//! Meeting detection in the app: the watcher's events, the "Teams meeting detected"
//! prompt (a small always-on-top window in the bottom-right corner), recording without
//! asking for chosen titles, and auto-stop when the call ends or Teams goes quiet.

use std::time::{Duration, Instant};

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Bounds, Context, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Render, StatefulInteractiveElement as _,
    Styled as _, WeakEntity, Window, WindowBackgroundAppearance, WindowBounds, WindowKind,
    WindowOptions, div, point, px, size,
};
use switchscribe_capture::RecorderState;
use switchscribe_capture::Track;
use switchscribe_capture::detect::{Detection, TeamsCall, Watcher, system_probe};

use crate::appearance::{rpx, ts};
use crate::recording::Banner;
use crate::theme::{Palette, SANS, palette};
use crate::ui::{self, Kind};
use crate::workspace::Workspace;

/// The prompt's size, logical pixels.
const WIDTH: f32 = 340.;
const HEIGHT: f32 = 140.;
/// How long after the call ends a recording keeps going (someone may rejoin).
pub const AFTER_CALL: Duration = Duration::from_secs(30);
/// A Teams track level below this is silence.
const QUIET: f32 = 0.02;
/// The name when neither Teams nor the calendar gives one.
pub const UNTITLED: &str = "Teams meeting";

/// Detection state.
#[derive(Default)]
pub struct Detect {
    watcher: Option<Watcher>,
    /// The call in progress.
    pub call: Option<TeamsCall>,
    /// The offer on screen: the title and the "always record" box.
    pub offer: Option<Offer>,
    prompt: Option<AnyWindowHandle>,
    /// When the call ended while recording.
    pub call_ended: Option<Instant>,
    polling: bool,
}

/// The "Record this meeting?" prompt's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    /// The meeting's name.
    pub title: String,
    /// "Always record meetings with this title".
    pub always: bool,
}

/// Whether a meeting with this title should be recorded without asking.
pub fn always_records(list: &[String], title: &str) -> bool {
    title != UNTITLED
        && list
            .iter()
            .any(|t| t.trim().eq_ignore_ascii_case(title.trim()))
}

/// Why a recording should stop on its own, if it should.
pub fn auto_stop(
    call_ended: Option<Instant>,
    quiet_since: Instant,
    auto_stop_minutes: u32,
    now: Instant,
) -> Option<String> {
    if call_ended.is_some_and(|t| now.duration_since(t) >= AFTER_CALL) {
        return Some("Recording stopped: the Teams call ended.".into());
    }
    let limit = Duration::from_secs(u64::from(auto_stop_minutes) * 60);
    (auto_stop_minutes > 0 && now.duration_since(quiet_since) >= limit)
        .then(|| format!("Recording stopped: Teams was silent for {auto_stop_minutes} min."))
}

impl Workspace {
    /// Start or stop watching for Teams calls to match Settings.
    pub(crate) fn sync_detection(&mut self, cx: &mut Context<Self>) {
        let want = self.settings.detection.auto_detect;
        if want && self.detect.watcher.is_none() {
            self.detect.watcher = system_probe().map(Watcher::start);
        } else if !want {
            self.detect.watcher = None;
            self.detect.call = None;
            self.close_offer(cx);
        }
        if self.detect.watcher.is_some() && !self.detect.polling {
            self.detect.polling = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let alive = this.update(cx, |ws, cx| {
                        ws.poll_detection(cx);
                        ws.detect.watcher.is_some()
                    });
                    if !alive.unwrap_or(false) {
                        let _ = this.update(cx, |ws, _| ws.detect.polling = false);
                        break;
                    }
                }
            })
            .detach();
        }
    }

    /// The calendar's name for a meeting happening now.
    pub(crate) fn calendar_title(&self) -> Option<String> {
        None
    }

    fn poll_detection(&mut self, cx: &mut Context<Self>) {
        let Some(watcher) = &self.detect.watcher else {
            return;
        };
        for event in watcher.take() {
            match event {
                Detection::Started(call) => self.call_started(call, cx),
                Detection::Ended => self.call_ended(cx),
            }
        }
    }

    fn call_started(&mut self, call: TeamsCall, cx: &mut Context<Self>) {
        tracing::info!(title = ?call.title, "Teams call detected");
        let title = call
            .title
            .clone()
            .or_else(|| self.calendar_title())
            .unwrap_or_else(|| UNTITLED.to_owned());
        self.detect.call = Some(call);
        self.detect.call_ended = None;
        if self.recording.is_some() || self.starting {
            return;
        }
        let d = &self.settings.detection;
        if d.auto_record || always_records(&d.always_record, &title) {
            self.record_detected(title, cx);
            return;
        }
        self.detect.offer = Some(Offer {
            title,
            always: false,
        });
        self.set_tray(RecorderState::Armed);
        if self.detect.prompt.is_none() {
            let ws = cx.weak_entity();
            cx.defer(move |cx| {
                let Some(handle) = open(ws.clone(), cx) else {
                    return;
                };
                let kept = ws.update(cx, |ws, _| {
                    let wanted = ws.detect.offer.is_some();
                    if wanted {
                        ws.detect.prompt = Some(handle);
                    }
                    wanted
                });
                if !kept.unwrap_or(false) {
                    close(handle, cx);
                }
            });
        }
        cx.notify();
    }

    fn call_ended(&mut self, cx: &mut Context<Self>) {
        tracing::info!("Teams call ended");
        self.detect.call = None;
        if self.recording.as_ref().is_some_and(|s| !s.test) {
            self.detect.call_ended = Some(Instant::now());
        }
        self.close_offer(cx);
    }

    /// Record the detected meeting under `title`.
    fn record_detected(&mut self, title: String, cx: &mut Context<Self>) {
        let main = self.window;
        let ws = cx.weak_entity();
        // Starting needs the main window; this may run from the prompt's click.
        cx.defer(move |cx| {
            let _ = main.update(cx, |_, window, cx| {
                let _ = ws.update(cx, |ws, cx| ws.start_titled_recording(title, window, cx));
            });
        });
    }

    /// The prompt's Record.
    pub(crate) fn accept_offer(&mut self, cx: &mut Context<Self>) {
        let Some(offer) = self.detect.offer.clone() else {
            return;
        };
        if offer.always && !always_records(&self.settings.detection.always_record, &offer.title) {
            self.settings
                .detection
                .always_record
                .push(offer.title.clone());
            self.save_settings(cx);
        }
        self.close_offer(cx);
        self.record_detected(offer.title, cx);
    }

    /// The prompt's Ignore (and ✕).
    pub(crate) fn ignore_offer(&mut self, cx: &mut Context<Self>) {
        self.close_offer(cx);
    }

    pub(crate) fn toggle_offer_always(&mut self, cx: &mut Context<Self>) {
        if let Some(o) = &mut self.detect.offer {
            o.always = !o.always;
        }
        if let Some(h) = self.detect.prompt {
            let _ = h.update(cx, |_, window, _| window.refresh());
        }
        cx.notify();
    }

    fn close_offer(&mut self, cx: &mut Context<Self>) {
        let had = self.detect.offer.take().is_some();
        if let Some(h) = self.detect.prompt.take() {
            cx.defer(move |cx| close(h, cx));
        }
        if had && self.recording.is_none() {
            self.set_tray(RecorderState::Idle);
        }
        cx.notify();
    }

    /// Called every recording tick: track silence and stop when it is time.
    pub(crate) fn check_auto_stop(&mut self, cx: &mut Context<Self>) {
        let Some(s) = &mut self.recording else {
            return;
        };
        if s.test || s.paused() {
            s.quiet_since = Instant::now();
            return;
        }
        let now = Instant::now();
        if s.monitor.level(Track::Remote) >= QUIET {
            s.quiet_since = now;
        }
        let why = auto_stop(
            self.detect.call_ended,
            s.quiet_since,
            self.settings.detection.auto_stop_minutes,
            now,
        );
        if let Some(why) = why {
            tracing::info!(%why, "auto-stop");
            self.detect.call_ended = None;
            self.stop_recording(cx);
            self.banners.push(Banner::plain(why));
            cx.notify();
        }
    }

    pub(crate) fn toggle_auto_detect(&mut self, cx: &mut Context<Self>) {
        self.settings.detection.auto_detect = !self.settings.detection.auto_detect;
        self.save_settings(cx);
        self.sync_detection(cx);
        cx.notify();
    }

    pub(crate) fn toggle_auto_record(&mut self, cx: &mut Context<Self>) {
        self.settings.detection.auto_record = !self.settings.detection.auto_record;
        self.save_settings(cx);
        cx.notify();
    }

    pub(crate) fn cycle_auto_stop(&mut self, cx: &mut Context<Self>) {
        const STEPS: [u32; 5] = [1, 3, 5, 10, 0];
        let now = self.settings.detection.auto_stop_minutes;
        let i = STEPS.iter().position(|m| *m == now).map_or(1, |i| i + 1);
        self.settings.detection.auto_stop_minutes = STEPS[i % STEPS.len()];
        self.save_settings(cx);
        cx.notify();
    }

    pub(crate) fn forget_always(&mut self, i: usize, cx: &mut Context<Self>) {
        if i < self.settings.detection.always_record.len() {
            self.settings.detection.always_record.remove(i);
            self.save_settings(cx);
            cx.notify();
        }
    }
}

/// Settings → Detection.
pub fn settings_detection(ws: &Workspace, p: &Palette, cx: &mut Context<Workspace>) -> AnyElement {
    let d = &ws.settings.detection;
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
    let note = |t: &'static str| div().text_size(ts::BODY).text_color(p.fg3).child(t);
    let watching = system_probe().is_some();
    let detect = div()
        .flex()
        .flex_col()
        .gap(rpx(6.))
        .child(
            div().flex().child(
                ui::button(
                    "auto-detect",
                    if d.auto_detect { "On" } else { "Off" },
                    Kind::Outline,
                    p,
                )
                .on_click(cx.listener(|ws, _, _, cx| ws.toggle_auto_detect(cx))),
            ),
        )
        .child(note(if watching {
            "Watches for Teams holding the microphone and offers to record."
        } else {
            "Detection needs Windows; record with the Record button."
        }));
    let when = div()
        .flex()
        .gap(rpx(6.))
        .child(
            ui::button("when-ask", "Ask first", Kind::Outline, p)
                .when(!d.auto_record, |b| b.bg(p.sel))
                .on_click(cx.listener(|ws, _, _, cx| {
                    if ws.settings.detection.auto_record {
                        ws.toggle_auto_record(cx);
                    }
                })),
        )
        .child(
            ui::button("when-record", "Record", Kind::Outline, p)
                .when(d.auto_record, |b| b.bg(p.sel))
                .on_click(cx.listener(|ws, _, _, cx| {
                    if !ws.settings.detection.auto_record {
                        ws.toggle_auto_record(cx);
                    }
                })),
        );
    let stop_label = match d.auto_stop_minutes {
        0 => "Never".to_owned(),
        m => format!("{m} min"),
    };
    let stop = div()
        .flex()
        .flex_col()
        .gap(rpx(6.))
        .child(
            div().flex().child(
                ui::button("auto-stop", stop_label, Kind::Outline, p)
                    .on_click(cx.listener(|ws, _, _, cx| ws.cycle_auto_stop(cx))),
            ),
        )
        .child(note(
            "Also stops 30 s after the Teams call ends. Test recordings never stop on their own.",
        ));
    let always = if d.always_record.is_empty() {
        note("None yet; tick the box on the prompt to add one.").into_any_element()
    } else {
        div()
            .flex()
            .flex_wrap()
            .gap(rpx(6.))
            .children(d.always_record.iter().enumerate().map(|(i, t)| {
                div()
                    .id(("always", i))
                    .flex()
                    .items_center()
                    .gap(rpx(6.))
                    .px(rpx(8.))
                    .py(rpx(3.))
                    .rounded(px(5.))
                    .border_1()
                    .border_color(p.bd2)
                    .text_size(ts::BODY)
                    .child(t.clone())
                    .child(
                        div()
                            .id(("always-x", i))
                            .text_color(p.fg3)
                            .cursor_pointer()
                            .hover(|s| s.text_color(p.fg))
                            .on_click(cx.listener(move |ws, _, _, cx| ws.forget_always(i, cx)))
                            .child("✕"),
                    )
            }))
            .into_any_element()
    };
    div()
        .child(
            ui::section_label("Detection", p.fg3)
                .mt(rpx(16.))
                .mb(rpx(4.)),
        )
        .child(row("Detect meetings", detect.into_any_element()))
        .child(row("When detected", when.into_any_element()))
        .child(row("Auto-stop after silence", stop.into_any_element()))
        .child(row("Always record", always))
        .into_any_element()
}

/// The prompt's view; it reads the offer from the workspace.
pub struct Prompt {
    ws: WeakEntity<Workspace>,
}

/// Open the prompt in the bottom-right corner of the primary display.
pub fn open(ws: WeakEntity<Workspace>, cx: &mut App) -> Option<AnyWindowHandle> {
    let display = cx.primary_display().map(|d| d.bounds());
    let origin = display.map_or(point(px(100.), px(100.)), |b| {
        point(
            b.origin.x + b.size.width - px(WIDTH + 16.),
            b.origin.y + b.size.height - px(HEIGHT + 64.),
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
    match gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| Prompt { ws })) {
        Ok((handle, _)) => Some(handle),
        Err(e) => {
            tracing::warn!(error = %e, "could not open the meeting prompt");
            None
        }
    }
}

/// Close it.
pub fn close(handle: AnyWindowHandle, cx: &mut App) {
    let _ = handle.update(cx, |_, window, _| window.remove_window());
}

impl Render for Prompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let offer = self
            .ws
            .upgrade()
            .and_then(|ws| ws.read(cx).detect.offer.clone());
        let Offer { title, always } = offer.unwrap_or(Offer {
            title: UNTITLED.into(),
            always: false,
        });
        let ws = self.ws.clone();
        let call = move |f: fn(&mut Workspace, &mut Context<Workspace>)| {
            let ws = ws.clone();
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                let _ = ws.update(cx, f);
            }
        };
        let stop = |d: gpui_kit::Stateful<gpui_kit::Div>| {
            d.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        };
        div()
            .id("prompt")
            .size_full()
            .flex()
            .flex_col()
            .gap(rpx(10.))
            .p(rpx(14.))
            .rounded(px(8.))
            .border_1()
            .border_color(p.pill_bd)
            .bg(p.pill)
            .font_family(SANS)
            .text_size(ts::UI)
            .text_color(p.fg)
            .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .text_size(ts::SMALL)
                    .text_color(p.fg2)
                    .child(div().size(rpx(12.)).rounded(px(3.)).bg(p.fg))
                    .child("switchscribe")
                    .child(div().flex_1())
                    .child(
                        stop(div().id("prompt-x"))
                            .cursor_pointer()
                            .hover(|s| s.text_color(p.fg))
                            .on_click(call(Workspace::ignore_offer))
                            .child("✕"),
                    ),
            )
            .child(
                div()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .line_height(gpui_kit::relative(1.4))
                    .child(format!("Teams meeting detected: {title}")),
            )
            .child(
                stop(div().id("prompt-always"))
                    .flex()
                    .items_center()
                    .gap(rpx(6.))
                    .text_size(ts::BODY)
                    .text_color(p.fg_ctl)
                    .cursor_pointer()
                    .on_click(call(Workspace::toggle_offer_always))
                    .child(ui::checkbox(always, &p))
                    .child("Always record meetings with this title"),
            )
            .child(
                div()
                    .flex()
                    .gap(rpx(6.))
                    .child(
                        stop(ui::button("prompt-record", "Record", Kind::Primary, &p))
                            .flex_1()
                            .justify_center()
                            .on_click(call(Workspace::accept_offer)),
                    )
                    .child(
                        stop(ui::button("prompt-ignore", "Ignore", Kind::Outline, &p))
                            .flex_1()
                            .justify_center()
                            .on_click(call(Workspace::ignore_offer)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_record_matches_titles_loosely() {
        let list = vec!["Platform standup".to_owned()];
        assert!(always_records(&list, "platform standup "));
        assert!(!always_records(&list, "Platform sync"));
        // The placeholder name never matches.
        assert!(!always_records(&[UNTITLED.to_owned()], UNTITLED));
    }

    #[test]
    fn recordings_stop_after_the_call_or_silence() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert_eq!(auto_stop(None, t0, 3, at(179)), None);
        assert!(auto_stop(None, t0, 3, at(180)).is_some_and(|w| w.contains("silent for 3 min")));
        // 0 means never for silence.
        assert_eq!(auto_stop(None, t0, 0, at(10_000)), None);
        // The call ended: 30 s of grace.
        assert_eq!(auto_stop(Some(at(100)), at(100), 3, at(129)), None);
        assert!(auto_stop(Some(at(100)), at(100), 3, at(130)).is_some_and(|w| w.contains("ended")));
    }
}
