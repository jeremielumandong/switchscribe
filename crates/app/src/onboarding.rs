//! First-run setup (M6): six short steps from the UI spec, shown in place of the main
//! window until finished or skipped. Each step reuses the Settings controls it stands
//! for, so setup and Settings never disagree.
//!
//! 1. Welcome and what stays local.
//! 2. Audio: capture mode, microphone and a live meter test.
//! 3. A transcription model (the one for this machine preselected) and live captions.
//! 4. Your voice: your name, and a headset tip (the voiceprint itself is learned from the
//!    mic track after the first meeting).
//! 5. Calendar and Jira, skippable.
//! 6. The consent notice to paste into meetings.

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Task, Window, div, px,
};
use switchscribe_capture::{Monitor, Recorder, Track};
use switchscribe_pipeline::models;

use crate::appearance::{rpx, ts};
use crate::theme::Palette;
use crate::ui::{self, Kind};
use crate::workspace::Workspace;

/// The steps, as the stepper names them.
const STEPS: [&str; 6] = [
    "Welcome",
    "Audio",
    "Model",
    "Your voice",
    "Calendar & Jira",
    "Consent",
];

/// Folder for the audio test, under the data folder; deleted after the test.
const TEST_DIR: &str = "setup-test";

/// The audio test: both tracks recording into a scratch folder, for the meters.
struct MeterTest {
    recorder: Recorder,
    monitor: Monitor,
    dir: PathBuf,
}

/// Setup in progress.
pub struct Onboarding {
    step: usize,
    name: Entity<InputState>,
    test: Option<MeterTest>,
    test_starting: bool,
    test_error: Option<String>,
    copied: bool,
    _meters: Option<Task<()>>,
}

impl Onboarding {
    /// Setup from step one, with the user's current name filled in.
    pub fn new(my_name: &str, window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Your name")
                .default_value(my_name.to_owned())
        });
        Self {
            step: 0,
            name,
            test: None,
            test_starting: false,
            test_error: None,
            copied: false,
            _meters: None,
        }
    }
}

/// Whether setup should open: never in the demo unless asked with
/// `SWITCHSCRIBE_ONBOARDING=1`, otherwise until it was finished once.
pub fn wanted(demo: bool, onboarded: bool) -> bool {
    match std::env::var("SWITCHSCRIBE_ONBOARDING").ok().as_deref() {
        Some("1") => true,
        Some("0") => false,
        _ => !demo && !onboarded,
    }
}

impl Workspace {
    /// Open setup again (Settings → Run setup again).
    pub(crate) fn restart_onboarding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let me = self
            .people
            .iter()
            .find(|p| p.is_me)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        self.onboarding = Some(Onboarding::new(&me, window, cx));
        cx.notify();
    }

    fn onboarding_go(&mut self, step: usize, cx: &mut Context<Self>) {
        let Some(o) = self.onboarding.as_mut() else {
            return;
        };
        if o.step == 3 && step != 3 {
            self.save_my_name(cx);
        }
        if step != 1 {
            self.stop_meter_test(cx);
        }
        if let Some(o) = self.onboarding.as_mut() {
            o.step = step.min(STEPS.len() - 1);
        }
        cx.notify();
    }

    /// Finish (or skip) setup: it does not open again.
    fn finish_onboarding(&mut self, cx: &mut Context<Self>) {
        if self.onboarding.as_ref().is_some_and(|o| o.step == 3) {
            self.save_my_name(cx);
        }
        self.stop_meter_test(cx);
        self.onboarding = None;
        if !self.settings.onboarded {
            self.settings.onboarded = true;
            self.save_settings(cx);
        }
        cx.notify();
    }

    fn save_my_name(&mut self, cx: &mut Context<Self>) {
        let Some(o) = &self.onboarding else {
            return;
        };
        let name = o.name.read(cx).value().trim().to_owned();
        let Some(me) = self.people.iter_mut().find(|p| p.is_me) else {
            return;
        };
        if name.is_empty() || name == me.name {
            return;
        }
        me.name = name.clone();
        let id = me.id;
        let store = self.store.clone();
        cx.background_executor()
            .spawn(async move {
                let renamed = store
                    .lock()
                    .map_err(|_| "the index is unavailable".to_owned())
                    .and_then(|s| s.rename_person(id, &name).map_err(|e| e.to_string()));
                if let Err(e) = renamed {
                    tracing::warn!(error = %e, "could not save your name");
                }
            })
            .detach();
    }

    fn start_meter_test(&mut self, cx: &mut Context<Self>) {
        let backend = self.backend();
        let Some(o) = self.onboarding.as_mut() else {
            return;
        };
        if o.test.is_some() || o.test_starting {
            return;
        }
        let backend = match backend {
            Ok(b) => b,
            Err(e) => {
                o.test_error = Some(e);
                cx.notify();
                return;
            }
        };
        o.test_starting = true;
        o.test_error = None;
        let dir = self.paths.data.join(TEST_DIR);
        let task = cx.background_executor().spawn({
            let dir = dir.clone();
            async move { Recorder::start(&dir, backend) }
        });
        cx.spawn(async move |this, cx| {
            let started = task.await;
            let _ = this.update(cx, |ws, cx| {
                let Some(o) = ws.onboarding.as_mut() else {
                    // Setup closed while the test started: stop it again.
                    if let Ok(r) = started {
                        stop_test(r, dir, cx);
                    }
                    return;
                };
                o.test_starting = false;
                match started {
                    Ok(recorder) => {
                        let monitor = recorder.monitor();
                        o.test = Some(MeterTest {
                            recorder,
                            monitor,
                            dir,
                        });
                        // Redraw the meters ten times a second while the test runs.
                        o._meters = Some(cx.spawn(async move |this, cx| {
                            loop {
                                cx.background_executor()
                                    .timer(Duration::from_millis(100))
                                    .await;
                                if this.update(cx, |_, cx| cx.notify()).is_err() {
                                    break;
                                }
                            }
                        }));
                    }
                    Err(e) => o.test_error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn stop_meter_test(&mut self, cx: &mut Context<Self>) {
        let Some(o) = self.onboarding.as_mut() else {
            return;
        };
        o._meters = None;
        if let Some(t) = o.test.take() {
            stop_test(t.recorder, t.dir, cx);
        }
    }

    fn copy_consent(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
            self.settings.storage.consent_notice.clone(),
        ));
        if let Some(o) = self.onboarding.as_mut() {
            o.copied = true;
        }
        cx.notify();
    }

    /// Setup in place of the window's content.
    pub(crate) fn render_onboarding(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(o) = &self.onboarding else {
            return div().into_any_element();
        };
        let step = o.step;
        let stepper =
            div()
                .flex()
                .items_center()
                .gap(rpx(6.))
                .children(STEPS.iter().enumerate().map(|(i, label)| {
                    let (fg, dot) = if i == step {
                        (p.fg, p.fg)
                    } else if i < step {
                        (p.fg2, p.ok)
                    } else {
                        (p.fg4, p.fg4)
                    };
                    div()
                        .id(("setup-step", i))
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .px(rpx(6.))
                        .py(rpx(3.))
                        .rounded(px(4.))
                        .cursor_pointer()
                        .text_size(ts::SMALL)
                        .text_color(fg)
                        .on_click(cx.listener(move |ws, _, _, cx| ws.onboarding_go(i, cx)))
                        .child(ui::dot(6., dot))
                        .child(*label)
                }));
        let body = match step {
            0 => self.setup_welcome(p),
            1 => self.setup_audio(o, p, cx),
            2 => self.setup_model(p, cx),
            3 => self.setup_voice(o, p),
            4 => crate::integrations_view::settings_integrations(self, p, cx),
            _ => self.setup_consent(o, p, cx),
        };
        let last = step + 1 == STEPS.len();
        let footer = div()
            .flex()
            .items_center()
            .gap(rpx(10.))
            .pt(rpx(14.))
            .border_t_1()
            .border_color(p.bd)
            .child(
                div()
                    .id("setup-skip")
                    .text_size(ts::BODY)
                    .text_color(p.fg3)
                    .cursor_pointer()
                    .on_click(cx.listener(|ws, _, _, cx| ws.finish_onboarding(cx)))
                    .child("Skip setup"),
            )
            .child(div().flex_1())
            .when(step > 0, |d| {
                d.child(
                    ui::button("setup-back", "Back", Kind::Outline, p)
                        .on_click(cx.listener(move |ws, _, _, cx| ws.onboarding_go(step - 1, cx))),
                )
            })
            .child(
                ui::button(
                    "setup-next",
                    if last {
                        "Finish"
                    } else if step == 4 {
                        "Next (or later)"
                    } else {
                        "Next"
                    },
                    Kind::Primary,
                    p,
                )
                .px(rpx(14.))
                .on_click(cx.listener(move |ws, _, _, cx| {
                    if last {
                        ws.finish_onboarding(cx);
                    } else {
                        ws.onboarding_go(step + 1, cx);
                    }
                })),
            );
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .w(rpx(720.))
                    .max_w_full()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .px(rpx(24.))
                    .py(rpx(20.))
                    .gap(rpx(16.))
                    .child(stepper)
                    .child(
                        div()
                            .text_size(ts::TITLE)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(STEPS[step]),
                    )
                    .child(
                        div()
                            .id("setup-body")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .text_size(ts::UI)
                            .child(body),
                    )
                    .child(footer),
            )
            .into_any_element()
    }

    fn setup_welcome(&self, p: &Palette) -> AnyElement {
        let point = |title: &'static str, body: &'static str| {
            div()
                .flex()
                .gap(rpx(12.))
                .py(rpx(8.))
                .child(div().pt(rpx(6.)).child(ui::dot(6., p.fg3)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().font_weight(FontWeight::MEDIUM).child(title))
                        .child(div().text_color(p.fg2).line_height(rpx(20.)).child(body)),
                )
        };
        div()
            .flex()
            .flex_col()
            .gap(rpx(4.))
            .child(div().text_color(p.fg2).pb(rpx(8.)).child(
                "switchscribe records your Teams calls without a bot, transcribes them on this \
                 machine and turns them into notes you can search and ask about.",
            ))
            .child(point(
                "Stays here",
                "Audio, transcripts and voiceprints never leave this computer.",
            ))
            .child(point(
                "Leaves only when you ask",
                "Summaries and answers send transcript text to the coding CLI you pick (Claude \
                 Code, Codex or Gemini CLI) and its model provider; a CLI running a local model \
                 keeps even that here. Action items you send to Jira go to Jira.",
            ))
            .child(point(
                "About two minutes",
                "Check your audio, pick a speech model, and optionally connect your calendar \
                 and Jira. Everything here is also in Settings.",
            ))
            .into_any_element()
    }

    fn setup_audio(&self, o: &Onboarding, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let (modes, mode_note, mics) = self.audio_controls(p, cx);
        let test = o.test.as_ref();
        let meters = div()
            .flex()
            .flex_col()
            .gap(rpx(8.))
            .children(Track::ALL.map(|track| {
                let level = test.map_or(0.0, |t| t.monitor.level(track));
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(12.))
                    .child(
                        div()
                            .w(rpx(60.))
                            .text_color(p.fg2)
                            .child(track.meter_label()),
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
        let action = if test.is_some() {
            ui::button("setup-test", "Stop test", Kind::Outline, p)
                .on_click(cx.listener(|ws, _, _, cx| {
                    ws.stop_meter_test(cx);
                    cx.notify();
                }))
                .into_any_element()
        } else {
            ui::button(
                "setup-test",
                if o.test_starting {
                    "Starting…"
                } else {
                    "Test audio"
                },
                Kind::Outline,
                p,
            )
            .on_click(cx.listener(|ws, _, _, cx| ws.start_meter_test(cx)))
            .into_any_element()
        };
        let label = |t: &'static str| ui::section_label(t, p.fg3).mt(rpx(14.)).mb(rpx(6.));
        div()
            .flex()
            .flex_col()
            .child(label("Teams audio").mt(rpx(0.)))
            .child(div().flex().child(modes))
            .child(
                div()
                    .pt(rpx(6.))
                    .text_size(ts::BODY)
                    .text_color(p.fg3)
                    .child(mode_note),
            )
            .child(label("Microphone"))
            .child(mics)
            .child(label("Test"))
            .child(div().text_size(ts::BODY).text_color(p.fg3).pb(rpx(8.)).child(
                "Play something in a Teams call (or the Teams test call) and say a few words: both \
                 meters should move. Nothing from the test is kept.",
            ))
            .child(meters)
            .child(div().pt(rpx(10.)).flex().child(action))
            .when_some(o.test_error.clone(), |d, e| {
                d.child(
                    div()
                        .pt(rpx(8.))
                        .text_size(ts::BODY)
                        .text_color(p.warn_fg)
                        .child(format!("The test could not start: {e}")),
                )
            })
            .into_any_element()
    }

    fn setup_model(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let gpu = self.settings.models.use_gpu && switchscribe_pipeline::GPU_BUILD;
        let pick = models::recommended(gpu);
        let note = if gpu {
            format!(
                "{} is marked recommended for this machine (a GPU build). Download it below; it \
                 runs after each call, so a meeting's transcript is ready a few minutes later.",
                pick.label
            )
        } else {
            format!(
                "{} is marked recommended: it runs on the CPU at about real time. Base is faster \
                 and also runs live captions. Downloads continue if you move on.",
                pick.label
            )
        };
        div()
            .flex()
            .flex_col()
            .gap(rpx(8.))
            .child(div().text_color(p.fg2).child(note))
            .child(self.render_models(p, cx))
            .into_any_element()
    }

    fn setup_voice(&self, o: &Onboarding, p: &Palette) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(rpx(10.))
            .child(div().text_color(p.fg2).child(
                "Everything on your microphone is you, so transcripts name you without any \
                 training. After your first meeting switchscribe also keeps a voiceprint of you, \
                 so your voice is recognized when it comes back through Teams.",
            ))
            .child(ui::section_label("What should meetings call you?", p.fg3).mt(rpx(6.)))
            .child(
                div()
                    .w(rpx(320.))
                    .child(Input::new(&o.name).text_size(ts::BODY)),
            )
            .child(ui::section_label("Use a headset", p.fg3).mt(rpx(10.)))
            .child(div().text_color(p.fg2).child(
                "On laptop speakers your microphone also hears the other people, and their words \
                 can end up under your name. A headset keeps the two tracks apart.",
            ))
            .into_any_element()
    }

    fn setup_consent(&self, o: &Onboarding, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(rpx(10.))
            .child(div().text_color(p.fg2).child(
                "Some places require everyone's consent before a call is recorded, and your \
                 company may have its own policy. Paste this into the meeting chat when you \
                 start recording (you can change the text in Settings):",
            ))
            .child(
                div()
                    .p(rpx(12.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(p.bd2)
                    .bg(p.surface)
                    .line_height(rpx(20.))
                    .child(self.settings.storage.consent_notice.clone()),
            )
            .child(
                div().flex().child(
                    ui::button(
                        "setup-copy",
                        if o.copied { "Copied" } else { "Copy notice" },
                        Kind::Outline,
                        p,
                    )
                    .on_click(cx.listener(|ws, _, _, cx| ws.copy_consent(cx))),
                ),
            )
            .child(div().text_color(p.fg3).text_size(ts::BODY).child(
                "The red dot in the title bar and the mini recorder show whenever switchscribe is \
                 recording.",
            ))
            .into_any_element()
    }
}

/// Stop the audio test and delete its folder, off the UI thread.
fn stop_test(recorder: Recorder, dir: PathBuf, cx: &mut Context<Workspace>) {
    cx.background_executor()
        .spawn(async move {
            if let Err(e) = recorder.stop() {
                tracing::warn!(error = %e, "the audio test did not stop cleanly");
            }
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                tracing::warn!(error = %e, "could not remove the audio test folder");
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_opens_once_and_never_in_the_demo() {
        if std::env::var("SWITCHSCRIBE_ONBOARDING").is_ok() {
            return;
        }
        assert!(wanted(false, false));
        assert!(!wanted(false, true));
        assert!(!wanted(true, false));
    }
}
