//! Writes on Redis, from the console or a key tab. Redis has no
//! transactions: each command applies as soon as it runs, and nothing can
//! be rolled back. So before any runs, [`approve`] hands the commands to the
//! shared write review (see [`write_review`]), with what their keys hold
//! now. Every write that runs goes to the audit log ([`audit`]).

use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;

use futures::FutureExt as _;
use gpui_kit::{App, Window};
use kuery_core::redis::{Class, classify, danger, join_command, overwritten_key, written_keys};
use kuery_redis::{KeyKind, KeyPreview};

use crate::audit::{self, AuditAction, AuditRecord};
use crate::connection::RedisDb;
use crate::redis_key_tab::ttl_text;
use crate::ui::{format_bytes, thousands};
use crate::write_mode;
use crate::write_review::{self, Detail, Planned, Preview, Request};

/// Keys previewed in a review at most.
const MAX_PREVIEWS: usize = 20;

/// A command about to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub args: Vec<String>,
    pub planned: Planned,
}

impl Command {
    pub fn new(args: Vec<String>) -> Self {
        let line = join_command(&args);
        Self {
            planned: Planned {
                shown: line.clone(),
                line,
                writes: classify(&args) == Class::Write,
                danger: danger(&args),
            },
            args,
        }
    }

    /// The same command, shown as `shown` rather than in full.
    pub fn described(args: Vec<String>, shown: String) -> Self {
        let mut command = Self::new(args);
        command.planned.shown = shown;
        command
    }
}

/// Calls `approved` once `commands` may run on `db`, asking first what the
/// environment's policy asks for. Nothing happens if the user cancels.
pub fn approve(
    db: RedisDb,
    database: i64,
    commands: Vec<Command>,
    window: &mut Window,
    cx: &mut App,
    approved: impl Fn(&mut Window, &mut App) + 'static,
) {
    let commands = Rc::new(commands);
    let request = Request {
        connection: db.name.clone(),
        environment: db.environment,
        allows: Rc::new({
            let db = db.clone();
            move || db.allows_writes()
        }),
        unlock: Rc::new({
            let db = db.clone();
            move |window, cx, allowed| {
                write_mode::allow_redis_writes(db.clone(), window, cx, move |window, cx| {
                    allowed(window, cx)
                })
            }
        }),
        heading: format!(
            "In db{database}, in this order. Redis has no rollback: each command applies as \
             soon as it runs."
        ),
        planned: Rc::new(
            commands
                .iter()
                .map(|command| command.planned.clone())
                .collect(),
        ),
        preview: Rc::new(move |_| preview(&db, database, commands.clone())),
        stopped: Rc::new(|| false),
    };
    write_review::approve(request, window, cx, approved);
}

/// What the keys the commands write hold now, read off the UI thread.
fn preview(db: &RedisDb, database: i64, commands: Rc<Vec<Command>>) -> Preview {
    let mut seen = HashSet::new();
    let keys: Vec<String> = commands
        .iter()
        .filter(|command| command.planned.writes)
        .flat_map(|command| written_keys(&command.args))
        .filter(|key| seen.insert(key.clone()))
        .take(MAX_PREVIEWS)
        .collect();
    let fetch = db.session.preview_keys(database, keys);
    async move {
        let found = fetch
            .await
            .map_err(|error| format!("Could not read the keys as they are now: {error}"))?;
        Ok(commands
            .iter()
            .map(|command| details(command, &found))
            .collect())
    }
    .boxed_local()
}

/// The keys a command writes, as they are now; the existing key it would
/// replace is what makes it risky.
fn details(command: &Command, found: &[KeyPreview]) -> Vec<Detail> {
    let overwritten = overwritten_key(&command.args);
    written_keys(&command.args)
        .iter()
        .filter_map(|key| found.iter().find(|preview| &preview.key == key))
        .map(|preview| Detail {
            subject: preview.key.clone(),
            text: describe(preview),
            danger: (preview.exists() && overwritten == Some(preview.key.as_str()))
                .then(|| format!("replaces the existing key {}", preview.key)),
        })
        .collect()
}

/// Logs a write that ran (or failed) to the audit log, off the UI thread.
pub fn audit(
    db: &RedisDb,
    project: &Path,
    database: i64,
    line: &str,
    outcome: String,
    failed: bool,
    cx: &App,
) {
    let record = AuditRecord::new(
        project,
        format!("{} (db{database})", db.name),
        db.environment,
        "Redis write",
        AuditAction::Run,
        line,
    )
    .finished(outcome, None, failed);
    audit::append_in_background(record, cx);
}

/// A key as it is now, on one line: `hash · 4 fields · no expiry · a=1, …`.
fn describe(preview: &KeyPreview) -> String {
    if !preview.exists() {
        return "new key".into();
    }
    let size = preview.length.map(|length| match preview.kind {
        KeyKind::String => format_bytes(length),
        KeyKind::Hash => format!("{} fields", thousands(length)),
        KeyKind::List => format!("{} items", thousands(length)),
        KeyKind::Set | KeyKind::ZSet => format!("{} members", thousands(length)),
        KeyKind::Stream => format!("{} entries", thousands(length)),
        KeyKind::Missing | KeyKind::Other => thousands(length),
    });
    let mut parts = vec![preview.kind.label().to_string()];
    parts.extend(size);
    parts.push(ttl_text(preview.ttl_ms));
    if !preview.sample.is_empty() {
        parts.push(preview.sample.clone());
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(line: &str) -> Command {
        Command::new(kuery_core::redis::split_command(line).unwrap())
    }

    fn planned(line: &str) -> Planned {
        command(line).planned
    }

    #[test]
    fn plans_commands() {
        let set = planned("SET greeting \"hello world\"");
        assert!(set.writes && set.danger.is_none());
        assert_eq!(set.line, "SET greeting \"hello world\"");
        // Shown as it runs, unless described otherwise.
        assert_eq!(set.shown, set.line);
        assert!(!planned("GET greeting").writes);
        assert!(planned("FLUSHDB").danger.is_some());
    }

    #[test]
    fn describes_keys_before_they_change() {
        let preview = KeyPreview {
            key: "user:1".into(),
            kind: KeyKind::Hash,
            ttl_ms: None,
            length: Some(4),
            sample: "name=User 1, …".into(),
        };
        assert_eq!(
            describe(&preview),
            "hash · 4 fields · no expiry · name=User 1, …"
        );
        let missing = KeyPreview {
            kind: KeyKind::Missing,
            length: None,
            sample: String::new(),
            ..preview
        };
        assert_eq!(describe(&missing), "new key");
    }

    #[test]
    fn a_rename_onto_an_existing_key_is_risky() {
        let preview = |key: &str, kind: KeyKind| KeyPreview {
            key: key.into(),
            kind,
            ttl_ms: None,
            length: Some(1),
            sample: String::new(),
        };
        let found = [
            preview("old", KeyKind::String),
            preview("new", KeyKind::String),
        ];
        let rename = details(&command("RENAME old new"), &found);
        assert_eq!(rename.len(), 2);
        assert_eq!(rename[0].subject, "old");
        assert!(rename[0].danger.is_none());
        assert_eq!(
            rename[1].danger.as_deref(),
            Some("replaces the existing key new")
        );
        // Onto a key that does not exist yet, nothing is replaced.
        let missing = [
            preview("old", KeyKind::String),
            KeyPreview {
                length: None,
                ..preview("new", KeyKind::Missing)
            },
        ];
        let rename = details(&command("RENAME old new"), &missing);
        assert!(rename.iter().all(|detail| detail.danger.is_none()));
        assert_eq!(rename[1].text, "new key");
    }
}
