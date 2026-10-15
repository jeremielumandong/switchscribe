//! Actions and their default keybindings (the UI spec's shortcut table).

use gpui_kit::{App, KeyBinding, actions};

actions!(
    switchscribe,
    [
        /// Start or stop recording (M1).
        ToggleRecording,
        /// Show or hide the Ask panel.
        ToggleAsk,
        /// Show or hide the meetings sidebar.
        ToggleSidebar,
        /// Search all meetings.
        SearchAll,
        /// Open Settings.
        ShowSettings,
    ]
);

/// Bind the default keys.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-shift-r", ToggleRecording, None),
        KeyBinding::new("ctrl-j", ToggleAsk, None),
        KeyBinding::new("ctrl-b", ToggleSidebar, None),
        KeyBinding::new("ctrl-shift-f", SearchAll, None),
        KeyBinding::new("ctrl-,", ShowSettings, None),
    ]);
}
