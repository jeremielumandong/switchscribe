//! switchscribe: a local-first meeting recorder and transcriber.

// Release builds on Windows are GUI-subsystem binaries: no console window behind the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

extern crate gpui_kit as gpui;

mod actions;
mod appearance;
mod ask_panel;
mod assets;
mod demo;
mod format;
mod live_view;
mod meeting_view;
mod mini_recorder;
mod player_bar;
mod processing;
mod recording;
mod theme;
#[cfg(windows)]
mod tray;
mod ui;
mod workspace;

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use gpui_kit::component::TitleBar;
use gpui_kit::{App, AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};
use switchscribe_store::{MeetingStatus, Paths, Settings, Store};
use tracing_subscriber::EnvFilter;

use crate::workspace::{Startup, Workspace};

const FONTS: &[&[u8]] = &[
    include_bytes!("../assets/fonts/Geist-Regular.ttf"),
    include_bytes!("../assets/fonts/Geist-Medium.ttf"),
    include_bytes!("../assets/fonts/Geist-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Geist-Italic.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Regular.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Medium.ttf"),
    include_bytes!("../assets/fonts/GeistMono-SemiBold.ttf"),
    include_bytes!("../assets/fonts/GeistMono-Italic.ttf"),
];

fn load_fonts(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> = FONTS.iter().map(|f| Cow::Borrowed(*f)).collect();
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        tracing::warn!(error = %e, "could not load bundled fonts");
    }
}

/// Read everything the first frame needs: settings, and the meetings in the index (or the
/// demo data). A database that will not open leaves the app running on an empty
/// in-memory index, with a warning in the window.
fn startup(demo: bool) -> Result<Startup> {
    let paths = Paths::resolve().context("could not determine a home directory")?;
    let settings = Settings::load(&paths.settings_file());
    let today = chrono::Local::now().date_naive();
    let (store, store_error) = if demo {
        (demo::store(today)?, None)
    } else {
        match paths.ensure().and_then(|()| Store::open(&paths.db_file())) {
            Ok(s) => (s, None),
            Err(e) => {
                tracing::error!(error = %e, "could not open the meetings database");
                (Store::open_in_memory()?, Some(e.to_string()))
            }
        }
    };
    // A recording cut off by a crash or power loss keeps what reached the disk; queue it
    // for processing like any other.
    for m in store.meetings()? {
        if m.status == MeetingStatus::Recording {
            tracing::warn!(id = %m.id, "recording was interrupted");
            store.set_status(&m.id, MeetingStatus::Processing)?;
        }
    }
    let meetings = store.meetings()?;
    let people = store.people()?;
    let devices = switchscribe_capture::input_devices();
    let installed_models = processing::scan_models(&paths.models_dir());
    tracing::info!(
        data = %paths.data.display(),
        meetings = meetings.len(),
        demo,
        "store opened"
    );
    Ok(Startup {
        paths,
        settings,
        store: Arc::new(Mutex::new(store)),
        meetings,
        people,
        demo,
        store_error,
        devices,
        installed_models,
    })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--version") {
        println!("switchscribe {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SWITCHSCRIBE_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let started = std::time::Instant::now();
    let state = startup(demo::requested(&args))?;
    let zoom = appearance::zoom();

    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            load_fonts(cx);
            actions::init(cx);
            theme::apply(
                theme::Palette::dark(),
                appearance::ui_font_size(zoom),
                None,
                cx,
            );
            let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(960.), px(600.))),
                app_id: Some("switchscribe".into()),
                ..TitleBar::window_options()
            };
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workspace::new(state, window, cx))
            });
            if let Err(e) = opened {
                tracing::error!(error = %e, "failed to open the main window");
                cx.quit();
                return;
            }
            tracing::debug!(ms = started.elapsed().as_millis() as u64, "window opened");
            cx.activate(true);
        });
    Ok(())
}
