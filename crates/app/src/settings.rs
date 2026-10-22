//! App-wide preferences, kept in `settings.toml` in the user config
//! directory. They apply to every project, unlike what lives in `.kuery/`.
//!
//! ```toml
//! [sidebar]
//! width = 300.0
//!
//! [updates]
//! check_automatically = true
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The sidebar's width when nothing else was chosen, in pixels.
pub const SIDEBAR_DEFAULT_WIDTH: f32 = 256.;
/// How narrow and how wide the sidebar can be dragged.
pub const SIDEBAR_MIN_WIDTH: f32 = 200.;
pub const SIDEBAR_MAX_WIDTH: f32 = 480.;

#[derive(Default, Deserialize)]
struct File {
    #[serde(default)]
    sidebar: Sidebar,
    #[serde(default)]
    updates: Updates,
}

#[derive(Default, Deserialize)]
struct Sidebar {
    #[serde(default)]
    width: Option<f32>,
}

#[derive(Default, Deserialize)]
struct Updates {
    #[serde(default)]
    check_automatically: Option<bool>,
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

/// Whether to look for a new version at startup and every few hours (on
/// unless turned off).
pub fn check_updates_automatically() -> bool {
    check_updates_automatically_in(&path())
}

pub fn set_check_updates_automatically(on: bool) -> std::io::Result<()> {
    set_in(
        &path(),
        "updates",
        "check_automatically",
        toml::Value::Boolean(on),
    )
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

fn check_updates_automatically_in(path: &Path) -> bool {
    read(path).updates.check_automatically.unwrap_or(true)
}

/// Sets `sidebar.width`, keeping everything else in the file.
fn set_sidebar_width_in(path: &Path, width: f32) -> std::io::Result<()> {
    let width = f64::from(clamp_sidebar_width(width));
    set_in(path, "sidebar", "width", toml::Value::Float(width))
}

/// Sets `[section] key` and keeps everything else in the file as it was. A
/// file that cannot be read as TOML is left alone rather than replaced.
fn set_in(path: &Path, section: &str, key: &str, value: toml::Value) -> std::io::Result<()> {
    let mut table = match std::fs::read_to_string(path) {
        Ok(text) => text.parse::<toml::Table>().map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not valid TOML: {}", path.display(), error.message()),
            )
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(error) => return Err(error),
    };
    let section = table
        .entry(section)
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !section.is_table() {
        *section = toml::Value::Table(toml::Table::new());
    }
    if let toml::Value::Table(section) = section {
        section.insert(key.into(), value);
    }
    write(path, &table)
}

/// The file, or the defaults when it is missing or cannot be read.
fn read(path: &Path) -> File {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

/// Writes through a file next to it and a rename, so a crash midway never
/// leaves half a file.
fn write(path: &Path, table: &toml::Table) -> std::io::Result<()> {
    let text = toml::to_string(table).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension("toml.tmp");
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path)
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
        // A broken file is not saved over.
        assert!(set_sidebar_width_in(&file, 300.).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "not toml at all [");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn update_checks_are_on_until_turned_off() {
        let file = temp_file("updates");
        assert!(check_updates_automatically_in(&file));
        set_in(
            &file,
            "updates",
            "check_automatically",
            toml::Value::Boolean(false),
        )
        .unwrap();
        assert!(!check_updates_automatically_in(&file));
        // Turning it off keeps the sidebar width, and the other way round.
        set_sidebar_width_in(&file, 300.).unwrap();
        assert!(!check_updates_automatically_in(&file));
        assert_eq!(sidebar_width_in(&file), 300.);
        std::fs::write(&file, "[updates]\ncheck_automatically = \"no\"\n").unwrap();
        assert!(check_updates_automatically_in(&file));
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn saving_the_width_keeps_the_rest_of_the_file() {
        let file = temp_file("keep");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "[other]\nx = 1\n\n[sidebar]\nwidth = 300.0\nopen = true\n",
        )
        .unwrap();
        set_sidebar_width_in(&file, 320.).unwrap();
        assert_eq!(sidebar_width_in(&file), 320.);
        let table: toml::Table = std::fs::read_to_string(&file).unwrap().parse().unwrap();
        assert_eq!(table["other"]["x"].as_integer(), Some(1));
        assert_eq!(table["sidebar"]["open"].as_bool(), Some(true));
        assert!(!file.with_extension("toml.tmp").exists());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }
}
