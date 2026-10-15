//! Sizes: every fixed size goes through [`rpx`] so the UI zooms as a whole, and text sizes
//! are the named steps in [`ts`] (the same scheme as Switchyard and switchhead).

use gpui_kit::{Pixels, Rems, px};

/// UI font size at 100 % (the rem size).
pub const UI_FONT: f32 = 13.0;

/// `SWITCHSCRIBE_ZOOM` (screenshots, tests), clamped to 70–200 %; 100 % otherwise.
pub fn zoom() -> f32 {
    std::env::var("SWITCHSCRIBE_ZOOM")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|z| z.is_finite())
        .map_or(1.0, |z| z.clamp(0.7, 2.0))
}

/// The UI font size at `zoom`.
pub fn ui_font_size(zoom: f32) -> Pixels {
    px(UI_FONT * zoom)
}

/// A size in design pixels at 100 %, as rems so it scales with the zoom.
pub const fn rpx(v: f32) -> Rems {
    Rems(v / UI_FONT)
}

/// Text sizes from the prototype.
pub mod ts {
    use super::rpx;
    use gpui_kit::Rems;

    /// 7 px: avatar initials.
    pub const AVATAR: Rems = rpx(7.);
    /// 10 px: tiny badges.
    pub const TINY: Rems = rpx(10.);
    /// 11 px: section headings, shortcuts, timestamps.
    pub const SMALL: Rems = rpx(11.);
    /// 11.5 px: prompt chips.
    pub const LABEL: Rems = rpx(11.5);
    /// 12 px: controls and secondary text.
    pub const BODY: Rems = rpx(12.);
    /// 12.5 px: table text, inputs.
    pub const UI: Rems = rpx(12.5);
    /// 13 px: the base size.
    pub const BASE: Rems = rpx(13.);
    /// 14 px: card titles.
    pub const TITLE: Rems = rpx(14.);
    /// 15 px: search field.
    pub const LARGE: Rems = rpx(15.);
    /// 20 px: screen titles.
    pub const DISPLAY: Rems = rpx(20.);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpx_is_relative_to_the_ui_font() {
        assert_eq!(rpx(13.).0, 1.0);
        assert_eq!(ts::BODY.0 * UI_FONT, 12.0);
    }
}
