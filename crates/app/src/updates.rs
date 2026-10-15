//! Updates (M6): a check against GitHub Releases shortly after start and every few hours,
//! a title-bar button when a newer version is out, and Settings → Updates. Installing
//! downloads the signed installer (its SHA-256 checked), runs it silently through
//! `process::command` and quits; the installer starts the new version. On macOS and Linux
//! the button opens the release page instead.

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};
use switchscribe_integrations::update::{self, INSTALLER_ARGS, Release};

use crate::appearance::{rpx, ts};
use crate::recording::Banner;
use crate::theme::Palette;
use crate::ui::{self, Kind};
use crate::workspace::Workspace;

/// The first check waits for startup to settle.
const FIRST_CHECK: Duration = Duration::from_secs(60);
/// Then every six hours (GitHub allows 60 anonymous API calls an hour).
const EVERY: Duration = Duration::from_secs(6 * 3600);

/// The running version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where updates stand.
#[derive(Default)]
pub struct Updates {
    /// A newer release, once found.
    pub available: Option<Release>,
    /// A check is running.
    pub checking: bool,
    /// The installer is downloading.
    pub downloading: bool,
    /// The last check found nothing newer.
    pub up_to_date: bool,
    /// The last check that the user asked for failed; why.
    pub error: Option<String>,
    polling: bool,
}

impl Workspace {
    /// Start the periodic check (not in the demo, not when turned off).
    pub(crate) fn schedule_update_checks(&mut self, cx: &mut Context<Self>) {
        if self.demo || self.updates.polling {
            return;
        }
        self.updates.polling = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FIRST_CHECK).await;
            loop {
                let go = this.update(cx, |ws, cx| {
                    if !ws.settings.skip_update_check {
                        ws.check_for_update(false, cx);
                    }
                });
                if go.is_err() {
                    break;
                }
                cx.background_executor().timer(EVERY).await;
            }
        })
        .detach();
    }

    /// Ask GitHub for a newer release. `asked`: the user clicked Check now, so failures
    /// show.
    pub(crate) fn check_for_update(&mut self, asked: bool, cx: &mut Context<Self>) {
        if self.updates.checking {
            return;
        }
        self.updates.checking = true;
        self.updates.error = None;
        let task = cx
            .background_executor()
            .spawn(async move { update::latest(VERSION) });
        cx.spawn(async move |this, cx| {
            let found = task.await;
            let _ = this.update(cx, |ws, cx| {
                ws.updates.checking = false;
                match found {
                    Ok(Some(r)) => {
                        tracing::info!(version = %r.version, "an update is available");
                        ws.updates.up_to_date = false;
                        ws.updates.available = Some(r);
                    }
                    Ok(None) => ws.updates.up_to_date = true,
                    Err(e) => {
                        tracing::warn!(error = %e, "update check failed");
                        if asked {
                            ws.updates.error = Some(e.to_string());
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Download the new installer, run it and quit.
    pub(crate) fn install_update(&mut self, cx: &mut Context<Self>) {
        let Some(release) = self.updates.available.clone() else {
            return;
        };
        if self.recording.is_some() || self.starting {
            self.banners.push(Banner::plain(
                "Stop the recording first; the update restarts switchscribe.",
            ));
            cx.notify();
            return;
        }
        // Send them to the release page instead when the installer would not do: it is
        // Windows-only (macOS and Linux get the DMG or AppImage there), and a download
        // without a checksum is refused.
        if !cfg!(windows) || release.sha256.is_none() {
            cx.open_url(&release.page_url);
            return;
        }
        if self.updates.downloading {
            return;
        }
        self.updates.downloading = true;
        let dir: PathBuf = self.paths.data.join("updates");
        let task = cx.background_executor().spawn(async move {
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            update::download(&release, &dir, |_, _| {}).map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let downloaded = task.await;
            let _ = this.update(cx, |ws, cx| {
                ws.updates.downloading = false;
                let path = match downloaded {
                    Ok(p) => p,
                    Err(e) => {
                        ws.banners
                            .push(Banner::plain(format!("The update did not download: {e}")));
                        cx.notify();
                        return;
                    }
                };
                let mut cmd = switchscribe_agent::process::command(&path);
                cmd.args(INSTALLER_ARGS);
                match cmd.spawn() {
                    // The installer closes this process if it is still running, installs
                    // and starts the new version.
                    Ok(_) => cx.quit(),
                    Err(e) => {
                        ws.banners
                            .push(Banner::plain(format!("The installer did not start: {e}")));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn toggle_update_checks(&mut self, cx: &mut Context<Self>) {
        self.settings.skip_update_check = !self.settings.skip_update_check;
        self.save_settings(cx);
        cx.notify();
    }
}

/// The title bar's update button, when there is one to install.
pub fn title_button(
    ws: &Workspace,
    p: &Palette,
    cx: &mut Context<Workspace>,
) -> Option<AnyElement> {
    let r = ws.updates.available.as_ref()?;
    let label = if ws.updates.downloading {
        "Downloading update…".to_owned()
    } else {
        format!("Update to {}", r.version)
    };
    Some(
        ui::button("update", label, Kind::Outline, p)
            .occlude()
            .gap(rpx(6.))
            .child(ui::dot(6., p.ok))
            .on_click(cx.listener(|ws, _, _, cx| ws.install_update(cx)))
            .into_any_element(),
    )
}

/// Settings → Updates.
pub fn settings_updates(ws: &Workspace, p: &Palette, cx: &mut Context<Workspace>) -> AnyElement {
    let u = &ws.updates;
    let status = if ws.demo {
        "Not checked in the demo.".to_owned()
    } else if u.checking {
        "Checking…".to_owned()
    } else if let Some(r) = &u.available {
        format!("{} is available.", r.version)
    } else if u.up_to_date {
        "Up to date.".to_owned()
    } else {
        String::new()
    };
    let auto = !ws.settings.skip_update_check;
    div()
        .flex()
        .flex_col()
        .gap(rpx(6.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(rpx(10.))
                .child(format!("switchscribe {VERSION}"))
                .child(div().text_color(p.fg3).child(status)),
        )
        .when_some(u.available.as_ref(), |d, r| {
            let notes = r.notes.lines().take(6).collect::<Vec<_>>().join("\n");
            let page = r.page_url.clone();
            d.child(div().text_size(ts::BODY).text_color(p.fg2).child(notes))
                .child(
                    div()
                        .id("update-notes")
                        .text_size(ts::BODY)
                        .text_color(p.stamp)
                        .cursor_pointer()
                        .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&page)))
                        .child("Release notes"),
                )
        })
        .when_some(u.error.clone(), |d, e| {
            d.child(
                div()
                    .text_size(ts::BODY)
                    .text_color(p.warn_fg)
                    .child(format!("The check failed: {e}")),
            )
        })
        .child(
            div()
                .flex()
                .gap(rpx(8.))
                .pt(px(2.))
                .when(!ws.demo, |d| {
                    d.child(
                        ui::button("update-check", "Check now", Kind::Outline, p)
                            .on_click(cx.listener(|ws, _, _, cx| ws.check_for_update(true, cx))),
                    )
                })
                .when(u.available.is_some() && !ws.demo, |d| {
                    d.child(
                        ui::button("update-install", "Install and restart", Kind::Primary, p)
                            .on_click(cx.listener(|ws, _, _, cx| ws.install_update(cx))),
                    )
                })
                .child(
                    ui::button(
                        "update-auto",
                        if auto {
                            "Check automatically: On"
                        } else {
                            "Check automatically: Off"
                        },
                        Kind::Outline,
                        p,
                    )
                    .on_click(cx.listener(|ws, _, _, cx| ws.toggle_update_checks(cx))),
                ),
        )
        .into_any_element()
}
