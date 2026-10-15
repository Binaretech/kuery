//! Where the app keeps per-user files (keymap, recent projects).

use std::path::PathBuf;

/// `~/Library/Application Support/dbadmin` on macOS, `%APPDATA%\dbadmin` on
/// Windows and `$XDG_CONFIG_HOME/dbadmin` (or `~/.config/dbadmin`) elsewhere.
pub fn config_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/dbadmin")
    } else if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or(home)
            .join("dbadmin")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("dbadmin")
    }
}
