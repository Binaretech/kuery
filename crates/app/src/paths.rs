//! Where the app keeps per-user files (keymap, recent projects) and what
//! it can throw away (downloaded updates).

use std::path::PathBuf;

/// The folder's name before the app was called Kuery.
const LEGACY_NAME: &str = "dbadmin";

/// `~/Library/Application Support/kuery` on macOS, `%APPDATA%\kuery` on
/// Windows and `$XDG_CONFIG_HOME/kuery` (or `~/.config/kuery`) elsewhere.
pub fn config_dir() -> PathBuf {
    config_dir_named("kuery")
}

/// `~/Library/Caches/kuery` on macOS, `%LOCALAPPDATA%\kuery` on Windows and
/// `$XDG_CACHE_HOME/kuery` (or `~/.cache/kuery`) elsewhere.
pub fn cache_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    if cfg!(target_os = "macos") {
        home.join("Library/Caches/kuery")
    } else if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| config_dir_named(""))
            .join("kuery")
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"))
            .join("kuery")
    }
}

/// Moves the config folder from before the rename (`…/dbadmin`) to
/// [`config_dir`], unless that one already exists. Called at startup.
pub fn migrate_legacy_config_dir() {
    let (legacy, current) = (config_dir_named(LEGACY_NAME), config_dir());
    if !current.exists()
        && legacy.is_dir()
        && let Err(error) = std::fs::rename(&legacy, &current)
    {
        eprintln!(
            "kuery: could not move {} to {}: {error}",
            legacy.display(),
            current.display()
        );
    }
}

fn config_dir_named(name: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support").join(name)
    } else if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or(home)
            .join(name)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join(name)
    }
}
