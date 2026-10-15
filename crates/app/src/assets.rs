//! Assets: gpui-kit's default icon bundle plus the extra Lucide icons switchscribe uses.

use std::borrow::Cow;

use gpui_kit::{AssetSource, SharedString};

gpui_kit::assets::icon_assets!(ExtraIcons, [AudioLines]);

/// The app's asset source.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_and_default_icons_load() {
        for icon in [
            "icons/audio-lines.svg",
            "icons/mic.svg",
            "icons/settings.svg",
        ] {
            assert!(AppAssets.load(icon).unwrap().is_some(), "{icon}");
        }
    }
}
