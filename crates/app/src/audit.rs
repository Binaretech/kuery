//! Audit log of writes: every run that writes, every preview, commit and
//! rollback, and every apply of staged row changes, one JSON object per line
//! in `audit.jsonl` in the user config directory (never in a project folder,
//! which may be committed).

use std::io::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::{App, actions};
use kuery_core::export::json_string;

actions!(kuery, [OpenAuditLog]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditAction {
    Run,
    Preview,
    Commit,
    Rollback,
    /// Staged row changes from a table tab.
    Apply,
}

impl AuditAction {
    fn name(self) -> &'static str {
        match self {
            AuditAction::Run => "run",
            AuditAction::Preview => "preview",
            AuditAction::Commit => "commit",
            AuditAction::Rollback => "rollback",
            AuditAction::Apply => "apply",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuditRecord {
    /// Seconds since the Unix epoch.
    pub at: u64,
    /// The project folder.
    pub project: String,
    pub connection: String,
    /// `DEVELOPMENT`, `STAGING` or `PRODUCTION`.
    pub environment: String,
    /// `Write` or `Full write`.
    pub mode: String,
    pub action: AuditAction,
    /// Empty for commits and rollbacks.
    pub sql: String,
    pub outcome: String,
    /// Statements and rows changed, when known.
    pub changed: Option<(usize, u64)>,
    pub failed: bool,
}

impl AuditRecord {
    pub fn now_at() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    /// The record as one line of JSON (without the line break).
    pub fn to_json(&self) -> String {
        let (statements, rows) = match self.changed {
            Some((statements, rows)) => (statements.to_string(), rows.to_string()),
            None => ("null".into(), "null".into()),
        };
        format!(
            "{{\"at\": {}, \"project\": {}, \"connection\": {}, \"environment\": {}, \
             \"mode\": {}, \"action\": {}, \"sql\": {}, \"statements\": {statements}, \
             \"rows\": {rows}, \"outcome\": {}, \"failed\": {}}}",
            self.at,
            json_string(&self.project),
            json_string(&self.connection),
            json_string(&self.environment),
            json_string(&self.mode),
            json_string(self.action.name()),
            json_string(&self.sql),
            json_string(&self.outcome),
            self.failed
        )
    }
}

pub fn path() -> PathBuf {
    crate::paths::config_dir().join("audit.jsonl")
}

/// Appends `record` to the log. Called off the UI thread.
pub fn append(record: &AuditRecord) -> std::io::Result<()> {
    append_to(&path(), record)
}

fn append_to(path: &std::path::Path, record: &AuditRecord) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // One write per line, so lines from different tabs never interleave.
    file.write_all(format!("{}\n", record.to_json()).as_bytes())
}

/// Registers "Open Audit Log", which opens the file in the system's app for
/// it (creating it empty if nothing was logged yet).
pub fn init(cx: &mut App) {
    cx.on_action(|_: &OpenAuditLog, cx| {
        let path = path();
        if !path.exists() {
            let created = path
                .parent()
                .map(std::fs::create_dir_all)
                .transpose()
                .and_then(|_| std::fs::write(&path, ""));
            if let Err(error) = created {
                eprintln!("kuery: could not create {}: {error}", path.display());
                return;
            }
        }
        cx.open_with_system(&path);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> AuditRecord {
        AuditRecord {
            at: 1_700_000_000,
            project: "/work/shop".into(),
            connection: "prod".into(),
            environment: "PRODUCTION".into(),
            mode: "Write".into(),
            action: AuditAction::Run,
            sql: "update orders set note = 'a \"b\"'\nwhere id = 1".into(),
            outcome: "1 statement · 1 row changed".into(),
            changed: Some((1, 1)),
            failed: false,
        }
    }

    #[test]
    fn records_are_one_json_line() {
        let json = record().to_json();
        assert!(!json.contains('\n'));
        assert_eq!(
            json,
            "{\"at\": 1700000000, \"project\": \"/work/shop\", \"connection\": \"prod\", \
             \"environment\": \"PRODUCTION\", \"mode\": \"Write\", \"action\": \"run\", \
             \"sql\": \"update orders set note = 'a \\\"b\\\"'\\nwhere id = 1\", \
             \"statements\": 1, \"rows\": 1, \"outcome\": \"1 statement · 1 row changed\", \
             \"failed\": false}"
        );
        let commit = AuditRecord {
            action: AuditAction::Commit,
            sql: String::new(),
            changed: None,
            ..record()
        };
        assert!(
            commit
                .to_json()
                .contains("\"statements\": null, \"rows\": null")
        );
    }

    #[test]
    fn appends_one_line_per_record() {
        let dir = std::env::temp_dir().join(format!("kuery-audit-{}", crate::project::new_id()));
        let path = dir.join("audit.jsonl");
        append_to(&path, &record()).unwrap();
        append_to(&path, &record()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }
}
