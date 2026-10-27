//! A project's query history, kept across sessions.
//!
//! It lives in the user config directory (`history/<id>.toml`, one file per
//! project folder), never inside the project: the SQL may contain data, and
//! the project folder may be committed to Git.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Most entries kept per project.
pub const LIMIT: usize = 200;

/// A query that ran, newest first in the History list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub connection: String,
    pub sql: String,
    pub outcome: String,
    pub failed: bool,
    /// When it ran, in seconds since the Unix epoch.
    pub at: u64,
}

impl HistoryEntry {
    pub fn now(connection: &str, sql: &str, outcome: String, failed: bool) -> Self {
        Self {
            connection: connection.to_string(),
            sql: sql.to_string(),
            outcome,
            failed,
            at: unix_now(),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    /// The project folder, to tell the files apart by eye.
    #[serde(default)]
    project: String,
    #[serde(default)]
    entries: Vec<HistoryEntry>,
}

/// The history saved for the project in `root`, newest first.
pub fn load(root: &Path) -> Vec<HistoryEntry> {
    read(&file_for(root))
}

/// Replaces the saved history of the project in `root`.
pub fn save(root: &Path, entries: &[HistoryEntry]) -> std::io::Result<()> {
    write(&file_for(root), root, entries)
}

fn read(path: &Path) -> Vec<HistoryEntry> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<File>(&text).ok())
        .map(|file| file.entries)
        .unwrap_or_default()
}

fn write(path: &Path, root: &Path, entries: &[HistoryEntry]) -> std::io::Result<()> {
    let file = File {
        project: root.display().to_string(),
        entries: entries.iter().take(LIMIT).cloned().collect(),
    };
    let text = toml::to_string(&file).map_err(std::io::Error::other)?;
    crate::paths::write_atomic(path, text)
}

fn file_for(root: &Path) -> PathBuf {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    crate::paths::config_dir().join("history").join(format!(
        "{:016x}.toml",
        fnv1a(root.to_string_lossy().as_bytes())
    ))
}

/// A stable hash (unlike `DefaultHasher`, it does not change between Rust
/// versions), so a project keeps finding its file.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `just now`, `5 min ago`, `3 h ago`, `2 d ago`.
pub fn ago(at: u64) -> String {
    ago_from(at, unix_now())
}

fn ago_from(at: u64, now: u64) -> String {
    match now.saturating_sub(at) {
        0..60 => "just now".into(),
        seconds @ 60..3_600 => format!("{} min ago", seconds / 60),
        seconds @ 3_600..86_400 => format!("{} h ago", seconds / 3_600),
        seconds => format!("{} d ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sql: &str) -> HistoryEntry {
        HistoryEntry {
            connection: "local".into(),
            sql: sql.into(),
            outcome: "1 rows · 2 ms".into(),
            failed: false,
            at: 1_700_000_000,
        }
    }

    #[test]
    fn saves_and_loads_entries_with_multiline_sql() {
        let dir = std::env::temp_dir().join(format!("kuery-history-{}", crate::project::new_id()));
        let path = dir.join("history.toml");
        let entries = vec![
            entry("select 1;\n-- \"quoted\"\nselect 'x'"),
            entry("select 2"),
        ];
        write(&path, Path::new("/some/project"), &entries).unwrap();
        assert_eq!(read(&path), entries);
        assert!(!path.with_extension("toml.partial").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keeps_the_newest_entries() {
        let dir = std::env::temp_dir().join(format!("kuery-history-{}", crate::project::new_id()));
        let path = dir.join("history.toml");
        let entries: Vec<HistoryEntry> = (0..LIMIT + 5)
            .map(|i| entry(&format!("select {i}")))
            .collect();
        write(&path, Path::new("/p"), &entries).unwrap();
        let loaded = read(&path);
        assert_eq!(loaded.len(), LIMIT);
        assert_eq!(loaded[0].sql, "select 0");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_or_broken_files_mean_no_history() {
        assert!(read(Path::new("/definitely/not/here.toml")).is_empty());
    }

    #[test]
    fn each_project_has_its_own_stable_file() {
        assert_eq!(fnv1a(b"/a"), fnv1a(b"/a"));
        assert_ne!(fnv1a(b"/a"), fnv1a(b"/b"));
        // Known FNV-1a value, so the file name never changes between builds.
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn says_how_long_ago() {
        let now = 1_000_000;
        assert_eq!(ago_from(now - 5, now), "just now");
        assert_eq!(ago_from(now - 300, now), "5 min ago");
        assert_eq!(ago_from(now - 7_200, now), "2 h ago");
        assert_eq!(ago_from(now - 3 * 86_400, now), "3 d ago");
        assert_eq!(ago_from(now + 10, now), "just now");
    }
}
