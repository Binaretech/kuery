//! Kuery's log: lines a person can read on stderr, the latest of which stay
//! in memory so a crash report can tell what led up to the crash.
//!
//! Nothing here leaves the machine. [`crate::crash_report`] sends these lines
//! as Sentry logs only with a report the user agreed to send.
//!
//! Write the message as a fixed sentence and put what varies in attributes,
//! so the same kind of line reads, and groups, the same in every report:
//!
//! ```ignore
//! diagnostics::warn(
//!     "Update check failed",
//!     &[("update.manual", false.into()), ("error.message", error.to_string().into())],
//! );
//! ```
//!
//! Name attributes after the OpenTelemetry conventions where one fits
//! (`error.type`, `db.system.name`, `file.name`). Never put queries, values,
//! hosts, user names or full paths in them: reports are scrubbed, but what is
//! never logged cannot leak.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, TryLockError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How many of the latest lines a crash report carries.
const KEPT: usize = 100;

static LOG: Log = Log::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Something the user did or Kuery did on its own, worth knowing before
    /// a crash: a connection opened, an update found.
    Info,
    /// Something failed that Kuery works around.
    Warn,
    /// Something failed that the user will notice.
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub level: Level,
    pub message: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Value>,
    /// Seconds since the Unix epoch.
    pub time: f64,
}

impl Entry {
    fn new(level: Level, message: &str, attributes: &[(&str, Value)]) -> Self {
        Self {
            level,
            message: message.into(),
            attributes: attributes
                .iter()
                .map(|(key, value)| ((*key).into(), value.clone()))
                .collect(),
            time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0.0, |elapsed| elapsed.as_secs_f64()),
        }
    }

    /// The line on stderr: `kuery: warn: Update check failed (update.manual=false)`.
    fn line(&self) -> String {
        let level = match self.level {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        };
        let mut line = format!("kuery: {level}: {}", self.message);
        if !self.attributes.is_empty() {
            let pairs: Vec<String> = self
                .attributes
                .iter()
                .map(|(key, value)| match value {
                    Value::String(text) => format!("{key}={text}"),
                    other => format!("{key}={other}"),
                })
                .collect();
            line.push_str(&format!(" ({})", pairs.join(", ")));
        }
        line
    }
}

/// The latest entries, oldest first.
struct Log {
    entries: Mutex<VecDeque<Entry>>,
}

impl Log {
    const fn new() -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
        }
    }

    fn push(&self, entry: Entry) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if entries.len() == KEPT {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// Never waits: the panic hook calls it, maybe on a thread that panicked
    /// while holding the lock. Then the crash comes without its log.
    fn recent(&self) -> Vec<Entry> {
        match self.entries.try_lock() {
            Ok(entries) => entries.iter().cloned().collect(),
            Err(TryLockError::Poisoned(poisoned)) => {
                poisoned.into_inner().iter().cloned().collect()
            }
            Err(TryLockError::WouldBlock) => Vec::new(),
        }
    }
}

fn write(level: Level, message: &str, attributes: &[(&str, Value)]) {
    let entry = Entry::new(level, message, attributes);
    eprintln!("{}", entry.line());
    LOG.push(entry);
}

pub fn info(message: &str, attributes: &[(&str, Value)]) {
    write(Level::Info, message, attributes);
}

pub fn warn(message: &str, attributes: &[(&str, Value)]) {
    write(Level::Warn, message, attributes);
}

pub fn error(message: &str, attributes: &[(&str, Value)]) {
    write(Level::Error, message, attributes);
}

/// The latest lines, oldest first, for a crash report.
pub fn recent() -> Vec<Entry> {
    LOG.recent()
}

/// `error.type` for an I/O error: its kind, such as `PermissionDenied`, which
/// says what went wrong without the path or the OS's words.
pub fn io_error_type(error: &std::io::Error) -> Value {
    format!("{:?}", error.kind()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_keeps_the_latest_lines_oldest_first() {
        let log = Log::new();
        for n in 0..KEPT + 2 {
            log.push(Entry::new(Level::Info, &format!("line {n}"), &[]));
        }
        let recent = log.recent();
        assert_eq!(recent.len(), KEPT);
        assert_eq!(recent.first().unwrap().message, "line 2");
        assert_eq!(recent.last().unwrap().message, format!("line {}", KEPT + 1));
    }

    #[test]
    fn a_busy_log_gives_nothing_instead_of_waiting() {
        let log = Log::new();
        log.push(Entry::new(Level::Info, "kept", &[]));
        let _held = log.entries.lock().unwrap();
        assert!(log.recent().is_empty());
    }

    #[test]
    fn a_poisoned_log_still_keeps_and_gives_lines() {
        let log = Log::new();
        let _ = std::panic::catch_unwind(|| {
            let _held = log.entries.lock().unwrap();
            panic!("while holding the log");
        });
        log.push(Entry::new(Level::Warn, "after", &[]));
        assert_eq!(log.recent()[0].message, "after");
    }

    #[test]
    fn the_line_shows_the_level_message_and_attributes() {
        let entry = Entry::new(
            Level::Warn,
            "Update check failed",
            &[
                ("update.manual", false.into()),
                ("error.type", "Timeout".into()),
            ],
        );
        assert_eq!(
            entry.line(),
            "kuery: warn: Update check failed (error.type=Timeout, update.manual=false)"
        );
        assert_eq!(
            Entry::new(Level::Info, "Started", &[]).line(),
            "kuery: info: Started"
        );
    }

    #[test]
    fn an_io_error_type_is_its_kind() {
        let error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(io_error_type(&error), Value::from("PermissionDenied"));
    }
}
