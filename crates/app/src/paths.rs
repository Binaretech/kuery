//! Where the app keeps per-user files (keymap, recent projects) and what
//! it can throw away (downloaded updates).

use std::ffi::OsString;
use std::path::PathBuf;

/// The folder's name before the app was called Kuery.
const LEGACY_NAME: &str = "dbadmin";

/// `~/Library/Application Support/kuery` on macOS, `%APPDATA%\kuery` on
/// Windows and `$XDG_CONFIG_HOME/kuery` (or `~/.config/kuery`) elsewhere.
pub fn config_dir() -> PathBuf {
    config_dir_named("kuery")
}

/// `~/Library/Caches/kuery` on macOS, `%LOCALAPPDATA%\kuery` on Windows and
/// `$XDG_CACHE_HOME/kuery` (or `~/.cache/kuery`) elsewhere. On Windows
/// without `%LOCALAPPDATA%` it falls back to `%APPDATA%`, then `HOME`.
/// `None` when the environment gives no absolute folder to start from.
pub fn cache_dir() -> Option<PathBuf> {
    cache_dir_in(|name| std::env::var_os(name))
}

fn cache_dir_in(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    // A relative variable is ignored, as the XDG spec asks.
    let var = |name| env(name).map(PathBuf::from).filter(|p| p.is_absolute());
    let dir = if cfg!(target_os = "macos") {
        var("HOME")?.join("Library/Caches")
    } else if cfg!(target_os = "windows") {
        var("LOCALAPPDATA")
            .or_else(|| var("APPDATA"))
            .or_else(|| var("HOME"))?
    } else {
        var("XDG_CACHE_HOME").or_else(|| Some(var("HOME")?.join(".cache")))?
    };
    Some(dir.join("kuery"))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.into())
        }
    }

    #[test]
    fn no_cache_dir_without_an_absolute_base() {
        // Otherwise downloads would land relative to the working directory.
        assert_eq!(cache_dir_in(env(&[])), None);
        assert_eq!(
            cache_dir_in(env(&[
                ("HOME", "relative"),
                ("XDG_CACHE_HOME", "also/relative"),
                ("LOCALAPPDATA", "relative"),
                ("APPDATA", "relative"),
            ])),
            None
        );
    }

    #[test]
    fn the_cache_dir_is_the_platform_cache() {
        let local = r"C:\Users\me\AppData\Local";
        let home = if cfg!(windows) {
            r"C:\Users\me"
        } else {
            "/home/me"
        };
        let expected = if cfg!(target_os = "macos") {
            "/home/me/Library/Caches/kuery"
        } else if cfg!(windows) {
            r"C:\Users\me\AppData\Local\kuery"
        } else {
            "/home/me/.cache/kuery"
        };
        assert_eq!(
            cache_dir_in(env(&[("HOME", home), ("LOCALAPPDATA", local)])),
            Some(PathBuf::from(expected))
        );
    }
}
