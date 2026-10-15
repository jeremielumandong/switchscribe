//! The notification-area icon (Windows): mirrors the recorder (idle, armed, recording,
//! paused, processing) and offers Record / Stop, Show and Quit.

use switchscribe_capture::RecorderState;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// Icon edge, pixels.
const SIZE: u32 = 32;

/// Something the user picked from the tray.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    /// Start or stop recording.
    ToggleRecording,
    /// Bring the main window forward.
    Show,
    /// Quit the app.
    Quit,
}

/// The tray icon and its menu.
pub struct Tray {
    icon: TrayIcon,
    record: MenuItem,
    show: MenuId,
    quit: MenuId,
    state: RecorderState,
}

impl Tray {
    /// Add the icon. Returns `None` (logged) if the shell refuses it.
    pub fn new() -> Option<Self> {
        let record = MenuItem::new("Start recording", true, None);
        let show = MenuItem::new("Show switchscribe", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        let built = menu
            .append(&record)
            .and_then(|()| menu.append(&show))
            .and_then(|()| menu.append(&PredefinedMenuItem::separator()))
            .and_then(|()| menu.append(&quit));
        if let Err(e) = built {
            tracing::warn!(error = %e, "could not build the tray menu");
            return None;
        }
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(tooltip(RecorderState::Idle))
            .with_icon(icon(RecorderState::Idle)?)
            .build();
        match icon {
            Ok(icon) => Some(Self {
                icon,
                show: show.id().clone(),
                quit: quit.id().clone(),
                record,
                state: RecorderState::Idle,
            }),
            Err(e) => {
                tracing::warn!(error = %e, "could not add the tray icon");
                None
            }
        }
    }

    /// Mirror the recorder.
    pub fn set_state(&mut self, state: RecorderState) {
        if state == self.state {
            return;
        }
        self.state = state;
        if let Err(e) = self.icon.set_icon(icon(state)) {
            tracing::debug!(error = %e, "tray icon update failed");
        }
        let _ = self.icon.set_tooltip(Some(tooltip(state)));
        self.record.set_text(match state {
            RecorderState::Recording | RecorderState::Paused => "Stop recording",
            _ => "Start recording",
        });
    }

    /// Commands picked since the last call.
    pub fn poll(&self) -> Vec<TrayCommand> {
        let mut out = Vec::new();
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == *self.record.id() {
                out.push(TrayCommand::ToggleRecording);
            } else if ev.id == self.show {
                out.push(TrayCommand::Show);
            } else if ev.id == self.quit {
                out.push(TrayCommand::Quit);
            }
        }
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } = ev
            {
                out.push(TrayCommand::Show);
            }
        }
        out
    }
}

fn tooltip(state: RecorderState) -> &'static str {
    match state {
        RecorderState::Idle => "switchscribe",
        RecorderState::Armed => "switchscribe: Teams meeting detected",
        RecorderState::Recording => "switchscribe: recording",
        RecorderState::Paused => "switchscribe: paused",
        RecorderState::Processing => "switchscribe: processing",
    }
}

/// A disc or ring in the state's color: gray ring idle, amber ring armed, red disc
/// recording, red ring paused, teal ring processing.
fn icon(state: RecorderState) -> Option<Icon> {
    let rgba = pixels(state);
    match Icon::from_rgba(rgba, SIZE, SIZE) {
        Ok(i) => Some(i),
        Err(e) => {
            tracing::warn!(error = %e, "bad tray icon");
            None
        }
    }
}

fn pixels(state: RecorderState) -> Vec<u8> {
    let (color, filled): ([u8; 3], bool) = match state {
        RecorderState::Idle => ([0xb9, 0xbc, 0xc3], false),
        RecorderState::Armed => ([0xeb, 0xa9, 0x41], false),
        RecorderState::Recording => ([0xed, 0x40, 0x42], true),
        RecorderState::Paused => ([0xed, 0x40, 0x42], false),
        RecorderState::Processing => ([0x6e, 0xd9, 0xd2], false),
    };
    let mut out = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let c = (SIZE as f32 - 1.0) / 2.0;
    let outer = SIZE as f32 / 2.0 - 2.0;
    let inner = if filled { 0.0 } else { outer - 4.0 };
    for y in 0..SIZE {
        for x in 0..SIZE {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            // One pixel of antialiasing on each edge.
            let edge = (outer + 0.5 - d).clamp(0.0, 1.0);
            let a = if filled {
                edge
            } else {
                edge * (d - inner + 0.5).clamp(0.0, 1.0)
            };
            out.extend_from_slice(&color);
            out.push((a * 255.0) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_are_square_rgba() {
        for s in [
            RecorderState::Idle,
            RecorderState::Recording,
            RecorderState::Processing,
        ] {
            let p = pixels(s);
            assert_eq!(p.len(), (SIZE * SIZE * 4) as usize);
            // Corners are transparent, the ring is drawn.
            assert_eq!(p[3], 0);
            assert!(p.as_chunks::<4>().0.iter().any(|px| px[3] == 255));
        }
    }
}
