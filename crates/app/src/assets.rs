//! Assets embedded in the binary: fonts (OFL) and our own icons.
//!
//! [`AppAssets`] serves our files first and delegates the rest to
//! `gpui_kit::assets::Assets`, gpui-kit's default icon set.

use std::borrow::Cow;

use gpui_kit::assets::{Assets as KitAssets, IconNamed};
use gpui_kit::{App, AssetSource, Result, SharedString};

/// User interface font family.
pub const SANS_FAMILY: &str = "IBM Plex Sans";
/// Editor and table font family.
pub const MONO_FAMILY: &str = "JetBrains Mono";

macro_rules! embed {
    ($($path:literal),* $(,)?) => {
        &[$(($path, include_bytes!(concat!("../assets/", $path)))),*]
    };
}

const FONTS: &[(&str, &[u8])] = embed![
    "fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf",
    "fonts/ibm-plex-sans/IBMPlexSans-Italic.ttf",
    "fonts/ibm-plex-sans/IBMPlexSans-Medium.ttf",
    "fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf",
    "fonts/ibm-plex-sans/IBMPlexSans-Bold.ttf",
    "fonts/jetbrains-mono/JetBrainsMono-Regular.ttf",
    "fonts/jetbrains-mono/JetBrainsMono-Italic.ttf",
    "fonts/jetbrains-mono/JetBrainsMono-Bold.ttf",
];

// Lucide (ISC, see assets/icons/LICENSE-LUCIDE): these are in gpui-kit's full
// catalog but not in its default set.
const ICONS: &[(&str, &[u8])] = embed![
    "icons/lock.svg",
    "icons/database.svg",
    "icons/table.svg",
    "icons/key-round.svg",
    "icons/link.svg",
    "icons/funnel.svg",
    "icons/arrow-down-up.svg",
    "icons/trash.svg",
    "icons/refresh-cw.svg",
    "icons/panel-bottom-close.svg",
    "icons/panel-bottom-open.svg",
    "icons/code.svg",
    "icons/pencil.svg",
];

/// Kuery's own icons, for `Icon::new(AppIcon::Lock)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppIcon {
    Lock,
    Database,
    Table,
    Key,
    Link,
    Filter,
    Sort,
    Trash,
    Reload,
    PanelBottomClose,
    PanelBottomOpen,
    Code,
    Pencil,
}

#[cfg(test)]
impl AppIcon {
    const ALL: [AppIcon; 13] = [
        AppIcon::Lock,
        AppIcon::Database,
        AppIcon::Table,
        AppIcon::Key,
        AppIcon::Link,
        AppIcon::Filter,
        AppIcon::Sort,
        AppIcon::Trash,
        AppIcon::Reload,
        AppIcon::PanelBottomClose,
        AppIcon::PanelBottomOpen,
        AppIcon::Code,
        AppIcon::Pencil,
    ];
}

impl IconNamed for AppIcon {
    fn path(self) -> SharedString {
        match self {
            AppIcon::Lock => "icons/lock.svg",
            AppIcon::Database => "icons/database.svg",
            AppIcon::Table => "icons/table.svg",
            AppIcon::Key => "icons/key-round.svg",
            AppIcon::Link => "icons/link.svg",
            AppIcon::Filter => "icons/funnel.svg",
            AppIcon::Sort => "icons/arrow-down-up.svg",
            AppIcon::Trash => "icons/trash.svg",
            AppIcon::Reload => "icons/refresh-cw.svg",
            AppIcon::PanelBottomClose => "icons/panel-bottom-close.svg",
            AppIcon::PanelBottomOpen => "icons/panel-bottom-open.svg",
            AppIcon::Code => "icons/code.svg",
            AppIcon::Pencil => "icons/pencil.svg",
        }
        .into()
    }
}

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match own(path) {
            Some(bytes) => Ok(Some(Cow::Borrowed(bytes))),
            None => KitAssets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut names = KitAssets.list(path)?;
        for (name, _) in FONTS.iter().chain(ICONS) {
            if name.starts_with(path) && !names.iter().any(|n| n == name) {
                names.push((*name).into());
            }
        }
        Ok(names)
    }
}

fn own(path: &str) -> Option<&'static [u8]> {
    FONTS
        .iter()
        .chain(ICONS)
        .find(|(name, _)| *name == path)
        .map(|(_, bytes)| *bytes)
}

/// Registers the embedded fonts with gpui's text system.
pub fn load_fonts(cx: &App) -> Result<()> {
    cx.text_system().add_fonts(
        FONTS
            .iter()
            .map(|(_, bytes)| Cow::Borrowed(*bytes))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_our_icons_and_falls_back_to_the_kit() {
        for icon in AppIcon::ALL {
            let svg = AppAssets.load(&icon.path()).unwrap().unwrap();
            assert!(svg.starts_with(b"<svg"), "{icon:?}");
        }
        let search = gpui_kit::assets::IconName::Search.path();
        assert!(AppAssets.load(&search).unwrap().is_some());
        assert!(AppAssets.load("icons/does-not-exist.svg").is_err());
    }

    #[test]
    fn lists_our_files_next_to_the_kit_ones() {
        let icons = AppAssets.list("icons/").unwrap();
        assert!(icons.iter().any(|n| n == "icons/lock.svg"));
        assert!(icons.iter().any(|n| n == "icons/search.svg"));
        assert_eq!(AppAssets.list("fonts/").unwrap().len(), FONTS.len());
    }

    #[test]
    fn embeds_truetype_fonts() {
        for (name, bytes) in FONTS {
            assert_eq!(&bytes[..4], &[0, 1, 0, 0], "{name}");
        }
    }
}
