//! Design tokens from the switchscribe prototype (`docs/design/switchscribe.dc.html`):
//! a dense, dark-first palette in the Switchyard / switchhead family, the 8 speaker
//! colors, and the bridge into gpui-component's theme so built-in controls match.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Global, Hsla, SharedString, Window, px, rgb};

/// Sans-serif UI font (bundled; shared with Switchyard and switchhead).
pub const SANS: &str = "Geist";
/// Monospace font (timestamps, shortcuts, durations).
pub const MONO: &str = "Geist Mono";

/// Colors used across the app. Names follow the prototype's roles.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// Window and main pane background (#0d0e10).
    pub bg: Hsla,
    /// Title bar, sidebar, Ask panel (#111215).
    pub panel: Hsla,
    /// Cards and inputs (#121316).
    pub surface: Hsla,
    /// Chips and secondary buttons (#16171a).
    pub chip: Hsla,
    /// Raised controls and hover (#1a1b1f).
    pub elev: Hsla,
    /// Selected row (#1d1e22).
    pub sel: Hsla,
    /// User chat bubbles and row action buttons (#1f2024).
    pub raised: Hsla,
    /// Pane borders (#222429).
    pub bd: Hsla,
    /// Control borders (#26282d).
    pub bd2: Hsla,
    /// Button borders (#2e3036).
    pub bd3: Hsla,
    /// Primary text and the light primary button (#e4e5e8).
    pub fg: Hsla,
    /// Body text (#d4d6da).
    pub text: Hsla,
    /// Text on chips (#c9ccd2).
    pub fg_chip: Hsla,
    /// Secondary controls text (#b9bcc3).
    pub fg_ctl: Hsla,
    /// Inactive nav text (#a9adb5).
    pub fg_nav: Hsla,
    /// Secondary text (#8b8f98).
    pub fg2: Hsla,
    /// Muted text, section headings (#6f737c).
    pub fg3: Hsla,
    /// Faint text, shortcut hints (#5a5e66).
    pub fg4: Hsla,
    /// Unnamed speakers (#6b6f78).
    pub unknown: Hsla,
    /// Recording, and only recording (oklch 0.63 0.21 25).
    pub rec: Hsla,
    /// Text on the recording button and the elapsed time (#ffd9d4).
    pub rec_fg: Hsla,
    /// The mini recorder's pill (#18191c).
    pub pill: Hsla,
    /// The mini recorder's border (#33363c).
    pub pill_bd: Hsla,
    /// Empty part of a level meter (#2a2c31).
    pub meter_bg: Hsla,
    /// Warnings and errors; never red (oklch 0.78 0.14 75).
    pub warn: Hsla,
    /// Warning text (oklch 0.86 0.12 80).
    pub warn_fg: Hsla,
    /// Timestamp chips' text (oklch 0.82 0.1 190).
    pub stamp: Hsla,
    /// Timestamp chips' fill (#14211f).
    pub stamp_bg: Hsla,
    /// Timestamp chips' border (#213432).
    pub stamp_bd: Hsla,
    /// The active CLI's status dot (oklch 0.74 0.13 150).
    pub ok: Hsla,
    /// Dark text on light fills (#0d0e10).
    pub on_light: Hsla,
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

impl Palette {
    /// The dark theme (the only one in M0).
    pub fn dark() -> Self {
        Self {
            bg: c(0x0d0e10),
            panel: c(0x111215),
            surface: c(0x121316),
            chip: c(0x16171a),
            elev: c(0x1a1b1f),
            sel: c(0x1d1e22),
            raised: c(0x1f2024),
            bd: c(0x222429),
            bd2: c(0x26282d),
            bd3: c(0x2e3036),
            fg: c(0xe4e5e8),
            text: c(0xd4d6da),
            fg_chip: c(0xc9ccd2),
            fg_ctl: c(0xb9bcc3),
            fg_nav: c(0xa9adb5),
            fg2: c(0x8b8f98),
            fg3: c(0x6f737c),
            fg4: c(0x5a5e66),
            unknown: c(0x6b6f78),
            rec: c(0xed4042),
            rec_fg: c(0xffd9d4),
            pill: c(0x18191c),
            pill_bd: c(0x33363c),
            meter_bg: c(0x2a2c31),
            warn: c(0xeba941),
            warn_fg: c(0xfac871),
            stamp: c(0x6ed9d2),
            stamp_bg: c(0x14211f),
            stamp_bd: c(0x213432),
            ok: c(0x69c27e),
            on_light: c(0x0d0e10),
        }
    }

    /// A person's speaker color: their palette index, or neutral gray when unnamed.
    pub fn speaker(&self, color: Option<u8>) -> Hsla {
        color.map_or(self.unknown, |i| {
            c(SPEAKER_COLORS[usize::from(i) % SPEAKER_COLORS.len()])
        })
    }
}

/// The 8 speaker colors (oklch ~0.74 0.12 at hues 250, 150, 300, 340, 110, 200, 55,
/// 275). The first five are the prototype's; the last three are provisional. None sits
/// near red (recording) or amber (warnings).
pub const SPEAKER_COLORS: [u32; 8] = [
    0x60aaf3, 0x69c27e, 0xb191ea, 0xdd8cc3, 0xb6b857, 0x24c1c9, 0xe2a35c, 0x8f9eef,
];

/// The active palette, as a GPUI global.
pub struct ActivePalette(pub Palette);

impl Global for ActivePalette {}

/// The active palette (the dark theme before [`apply`] runs).
pub fn palette(cx: &App) -> Palette {
    cx.try_global::<ActivePalette>()
        .map_or_else(Palette::dark, |p| p.0)
}

/// Make `p` the active palette and push it into gpui-component's theme.
pub fn apply(p: Palette, ui_font: gpui_kit::Pixels, window: Option<&mut Window>, cx: &mut App) {
    cx.set_global(ActivePalette(p));
    Theme::change(ThemeMode::Dark, None, cx);
    Theme::update(cx, |t| {
        t.font_family = SharedString::from(SANS);
        t.mono_font_family = SharedString::from(MONO);
        t.font_size = ui_font;
        t.radius = px(6.);
        t.radius_lg = px(8.);
        t.shadow = true;
        t.focus_ring = false;
        t.background = p.surface;
        t.foreground = p.fg;
        t.border = p.bd2;
        t.input = p.bd2;
        t.ring = p.fg3;
        t.caret = p.fg;
        t.selection = p.stamp.opacity(0.25);
        t.primary = p.fg;
        t.primary_hover = p.fg;
        t.primary_active = p.fg;
        t.primary_foreground = p.on_light;
        t.secondary = p.chip;
        t.secondary_hover = p.elev;
        t.secondary_active = p.elev;
        t.secondary_foreground = p.fg_chip;
        t.muted = p.elev;
        t.muted_foreground = p.fg3;
        t.popover = p.elev;
        t.popover_foreground = p.fg;
        t.accent = p.sel;
        t.accent_foreground = p.fg;
        t.list_hover = p.elev;
        t.list_active = p.sel;
        t.list_active_border = p.fg3;
        t.danger = p.warn;
        t.warning = p.warn;
        t.success = p.ok;
        t.info = p.stamp;
        t.scrollbar = Hsla::transparent_black();
        t.scrollbar_thumb = c(0x2a2c31);
        t.scrollbar_thumb_hover = p.fg4;
        t.title_bar = p.panel;
        t.title_bar_border = p.bd;
        t.sidebar = p.panel;
        t.sidebar_border = p.bd;
        t.tab_bar = p.panel;
        t.table = p.bg;
        t.table_head = p.bg;
        t.table_row_border = p.bd;
    });
    match window {
        Some(w) => w.refresh(),
        None => cx.refresh_windows(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unnamed_speakers_are_gray_and_indices_wrap() {
        let p = Palette::dark();
        assert_eq!(p.speaker(None), p.unknown);
        assert_eq!(p.speaker(Some(8)), p.speaker(Some(0)));
        assert_ne!(p.speaker(Some(0)), p.speaker(Some(1)));
    }

    #[test]
    fn no_speaker_color_is_the_recording_red() {
        let p = Palette::dark();
        for i in 0..SPEAKER_COLORS.len() as u8 {
            let s = p.speaker(Some(i));
            let dh = (s.h - p.rec.h).abs().min(1.0 - (s.h - p.rec.h).abs());
            assert!(
                dh > 0.04,
                "speaker color {i} is too close to the recording red"
            );
        }
    }
}
