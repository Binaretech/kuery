//! Recursos embebidos en el binario: fuentes (OFL) e iconos propios.
//!
//! [`AppAssets`] sirve primero lo nuestro y delega el resto en
//! `gpui_kit::assets::Assets`, el set de iconos por defecto de gpui-kit.

use std::borrow::Cow;

use gpui_kit::assets::{Assets as KitAssets, IconNamed};
use gpui_kit::{App, AssetSource, Result, SharedString};

/// Familia de la interfaz.
pub const SANS_FAMILY: &str = "IBM Plex Sans";
/// Familia del editor y de la tabla.
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

// Lucide (ISC, ver assets/icons/LICENSE-LUCIDE): están en el catálogo completo
// de gpui-kit pero no en el set por defecto.
const ICONS: &[(&str, &[u8])] = embed!["icons/lock.svg", "icons/database.svg", "icons/table.svg"];

/// Iconos propios de dbadmin, para `Icon::new(AppIcon::Lock)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppIcon {
    Lock,
    Database,
    Table,
}

impl IconNamed for AppIcon {
    fn path(self) -> SharedString {
        match self {
            AppIcon::Lock => "icons/lock.svg",
            AppIcon::Database => "icons/database.svg",
            AppIcon::Table => "icons/table.svg",
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

/// Registra las fuentes embebidas en el text system de gpui.
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
        for icon in [AppIcon::Lock, AppIcon::Database, AppIcon::Table] {
            let svg = AppAssets.load(&icon.path()).unwrap().unwrap();
            assert!(svg.starts_with(b"<svg"), "{icon:?}");
        }
        let search = gpui_kit::assets::IconName::Search.path();
        assert!(AppAssets.load(&search).unwrap().is_some());
        assert!(AppAssets.load("icons/no-existe.svg").is_err());
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
