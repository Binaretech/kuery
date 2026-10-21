//! App-wide preferences, kept in `settings.toml` in the user config
//! directory. They apply to every project, unlike what lives in `.kuery/`.
//!
//! ```toml
//! [sidebar]
//! width = 300.0
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The sidebar's width when nothing else was chosen, in pixels.
pub const SIDEBAR_DEFAULT_WIDTH: f32 = 256.;
/// How narrow and how wide the sidebar can be dragged.
pub const SIDEBAR_MIN_WIDTH: f32 = 200.;
pub const SIDEBAR_MAX_WIDTH: f32 = 480.;

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    sidebar: Sidebar,
}

#[derive(Default, Serialize, Deserialize)]
struct Sidebar {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    width: Option<f32>,
}

/// A sidebar width kept within the limits; anything that is not a number
/// gives the default.
pub fn clamp_sidebar_width(width: f32) -> f32 {
    if width.is_finite() {
        width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
    } else {
        SIDEBAR_DEFAULT_WIDTH
    }
}

/// The saved sidebar width, or the default.
pub fn sidebar_width() -> f32 {
    sidebar_width_in(&path())
}

/// Saves the sidebar width for every project.
pub fn set_sidebar_width(width: f32) -> std::io::Result<()> {
    set_sidebar_width_in(&path(), width)
}

fn path() -> PathBuf {
    crate::paths::config_dir().join("settings.toml")
}

fn sidebar_width_in(path: &Path) -> f32 {
    read(path)
        .sidebar
        .width
        .map_or(SIDEBAR_DEFAULT_WIDTH, clamp_sidebar_width)
}

fn set_sidebar_width_in(path: &Path, width: f32) -> std::io::Result<()> {
    let mut file = read(path);
    file.sidebar.width = Some(clamp_sidebar_width(width));
    write(path, &file)
}

/// The file, or the defaults when it is missing or cannot be read.
fn read(path: &Path) -> File {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

fn write(path: &Path, file: &File) -> std::io::Result<()> {
    let text = toml::to_string(file).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "kuery-settings-{name}-{}",
                crate::project::new_id()
            ))
            .join("settings.toml")
    }

    #[test]
    fn clamps_the_sidebar_width() {
        assert_eq!(clamp_sidebar_width(256.), 256.);
        assert_eq!(clamp_sidebar_width(120.), SIDEBAR_MIN_WIDTH);
        assert_eq!(clamp_sidebar_width(900.), SIDEBAR_MAX_WIDTH);
        assert_eq!(clamp_sidebar_width(-5.), SIDEBAR_MIN_WIDTH);
        assert_eq!(clamp_sidebar_width(f32::NAN), SIDEBAR_DEFAULT_WIDTH);
        assert_eq!(clamp_sidebar_width(f32::INFINITY), SIDEBAR_DEFAULT_WIDTH);
    }

    #[test]
    fn the_sidebar_width_round_trips() {
        let file = temp_file("round-trip");
        assert_eq!(sidebar_width_in(&file), SIDEBAR_DEFAULT_WIDTH);
        set_sidebar_width_in(&file, 312.5).unwrap();
        assert_eq!(sidebar_width_in(&file), 312.5);
        set_sidebar_width_in(&file, 9_000.).unwrap();
        assert_eq!(sidebar_width_in(&file), SIDEBAR_MAX_WIDTH);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn a_hand_edited_or_broken_file_falls_back_to_the_default() {
        let file = temp_file("broken");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "[sidebar]\nwidth = 50.0\n").unwrap();
        assert_eq!(sidebar_width_in(&file), SIDEBAR_MIN_WIDTH);
        std::fs::write(&file, "not toml at all [").unwrap();
        assert_eq!(sidebar_width_in(&file), SIDEBAR_DEFAULT_WIDTH);
        // Saving over a broken file replaces it with a valid one.
        set_sidebar_width_in(&file, 300.).unwrap();
        assert_eq!(sidebar_width_in(&file), 300.);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }
}
