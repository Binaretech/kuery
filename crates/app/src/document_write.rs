//! Writes on a document database, from its console and its collection
//! tabs. Before a script that writes runs, [`approve`] hands its
//! statements to the shared write review (see [`write_review`]) with a
//! preview of what each would change: exact where the engine runs the
//! script in a transaction and undoes it, estimated where it counts what
//! each statement would touch, and unknown elsewhere, which makes the
//! statement dangerous. The review says whether the script runs all or
//! nothing. Changes staged in a collection tab go through the same review
//! with [`approve_changes`], each shown as the statement it becomes. Every
//! write that runs, every one refused or failed, and every preview goes to
//! the audit log ([`record`], [`record_changes`]).

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use futures::FutureExt as _;
use gpui_kit::{App, Window};
use kuery_core::analysis::StatementKind;
use kuery_core::document::adapter::Outcome;
use kuery_core::document::capabilities::Transactions;
use kuery_core::document::change::{Count, DocChange};
use kuery_core::document::language::{Language, Statement, Style};
use kuery_core::document::session::{ApplyFailure, DocumentSession, Estimate, Heavy, Impact};
use kuery_core::document::value::{CollectionRef, Value};
use kuery_core::session::{QueryError, RunOptions};

use crate::audit::{AuditAction, AuditRecord};
use crate::connection::DocDb;
use crate::query_tab::describe_error;
use crate::ui::thousands;
use crate::write_mode;
use crate::write_review::{self, Detail, Planned, Request};

/// What the audit log calls these writes.
pub const MODE: &str = "Document write";

/// Documents of a preview's sample the review names.
const NAMED: usize = 5;

/// A script about to run: where it starts, its text, and its statements
/// as the language read them.
#[derive(Clone)]
pub struct Script {
    /// The container it starts in (`["shop"]`).
    pub start: Vec<String>,
    pub text: String,
    pub statements: Vec<Statement>,
    /// Whether work the language marks heavy may run.
    pub heavy: Heavy,
}

impl Script {
    /// Whether any statement writes.
    pub fn writes(&self) -> bool {
        self.statements.iter().any(writes)
    }

    /// `shop`, for the review and the audit log.
    pub fn place(&self) -> String {
        match self.start.is_empty() {
            true => "the connection's database".into(),
            false => self.start.join("."),
        }
    }
}

fn writes(statement: &Statement) -> bool {
    statement.info.kind != StatementKind::Read
}

/// A script on its way through the review, as its console follows it: the
/// session it previews and runs on, whether its preview runs now (Run
/// waits for it), and whether Cancel stopped it (then nothing more
/// happens).
pub struct Reviewing {
    session: DocumentSession,
    previewing: Cell<bool>,
    stopped: Cell<bool>,
    previewed: Previewed,
}

/// Hears that a preview ended, with its audit record.
type Previewed = Box<dyn Fn(AuditRecord, &mut App)>;

impl Reviewing {
    pub fn new(
        session: DocumentSession,
        previewed: impl Fn(AuditRecord, &mut App) + 'static,
    ) -> Self {
        Reviewing {
            session,
            previewing: Cell::new(false),
            stopped: Cell::new(false),
            previewed: Box::new(previewed),
        }
    }

    /// Its preview runs now.
    pub fn previewing(&self) -> bool {
        self.previewing.get()
    }

    /// Stops its preview on the server: neither the review nor the run
    /// follows.
    pub fn stop(&self) {
        self.stopped.set(true);
        self.previewing.set(false);
        self.session.cancel();
    }
}

/// Calls `approved` once the script may run on the reviewing session,
/// asking first what the environment's policy asks for. Nothing happens if
/// the user cancels, or stops the preview.
pub fn approve(
    db: &DocDb,
    project: &Path,
    script: Script,
    reviewing: Rc<Reviewing>,
    window: &mut Window,
    cx: &mut App,
    approved: impl Fn(&mut Window, &mut App) + 'static,
) {
    let planned = Rc::new(planned(&script));
    let session = reviewing.session.clone();
    let request = Request {
        connection: db.name.clone(),
        environment: db.environment,
        allows: allows(db),
        unlock: unlock(db),
        heading: heading(
            &script.place(),
            script.statements.len(),
            session.all_or_nothing(&script.statements),
            session.capabilities().transactions,
        ),
        planned: planned.clone(),
        preview: Rc::new({
            let (db, project, reviewing) = (db.clone(), project.to_path_buf(), reviewing.clone());
            let cx = cx.to_async();
            move |_| {
                reviewing.previewing.set(true);
                let previewed = reviewing.session.preview_in(
                    &script.start,
                    script.text.clone(),
                    script.heavy,
                    RunOptions::default(),
                );
                let (db, project, script, planned, reviewing) = (
                    db.clone(),
                    project.clone(),
                    script.clone(),
                    planned.clone(),
                    reviewing.clone(),
                );
                let language = reviewing.session.language();
                let cx = cx.clone();
                async move {
                    let impacts = previewed.await;
                    reviewing.previewing.set(false);
                    let (outcome, failed) = previewed_outcome(&impacts);
                    let record = record(
                        &db,
                        &project,
                        &script,
                        AuditAction::Preview,
                        outcome,
                        None,
                        failed,
                    );
                    cx.update(|cx| (reviewing.previewed)(record, cx));
                    Ok(review_details(&planned, impacts, language))
                }
                .boxed_local()
            }
        }),
        stopped: Rc::new(move || reviewing.stopped.get()),
    };
    write_review::approve(request, window, cx, approved);
}

/// Whether writes on the connection run without allowing them first.
fn allows(db: &DocDb) -> Rc<dyn Fn() -> bool> {
    let db = db.clone();
    Rc::new(move || db.allows_writes())
}

/// Allows writes on the connection (typing its name, on production).
fn unlock(db: &DocDb) -> write_review::Unlock {
    let db = db.clone();
    Rc::new(move |window, cx, allowed| {
        write_mode::allow_document_writes(db.clone(), window, cx, move |window, cx| {
            allowed(window, cx)
        })
    })
}

/// Calls `approved` once the changes staged in a collection tab may apply,
/// asking first what the environment's policy asks for: the review shows
/// each change as the statement it becomes, and says whether they apply
/// all or nothing. Nothing happens if the user cancels.
pub fn approve_changes(
    db: &DocDb,
    collection: &CollectionRef,
    changes: &[DocChange],
    window: &mut Window,
    cx: &mut App,
    approved: impl Fn(&mut Window, &mut App) + 'static,
) {
    let language = db.catalog.language();
    let steps = changes_planned(language, collection, changes);
    let request = Request {
        connection: db.name.clone(),
        environment: db.environment,
        allows: allows(db),
        unlock: unlock(db),
        heading: changes_heading(
            &collection.display(),
            changes.len(),
            db.catalog.capabilities().transactions,
        ),
        planned: Rc::new(steps),
        // The statements say all there is: each touches one document, and
        // only if it still holds what was loaded.
        preview: Rc::new(|planned| {
            let none = vec![Vec::new(); planned.len()];
            async move { Ok(none) }.boxed_local()
        }),
        stopped: Rc::new(|| false),
    };
    write_review::approve(request, window, cx, approved);
}

/// The review's steps for staged changes: each as the statement it
/// becomes.
pub fn changes_planned(
    language: &Language,
    collection: &CollectionRef,
    changes: &[DocChange],
) -> Vec<Planned> {
    changes
        .iter()
        .map(|change| {
            let line = (language.change)(collection, change);
            Planned {
                shown: line.clone(),
                line,
                writes: true,
                danger: None,
            }
        })
        .collect()
}

/// The review's subtitle for staged changes: where they apply, that each
/// applies only to a document still as it was loaded, and whether they
/// apply all or nothing.
pub fn changes_heading(place: &str, changes: usize, transactions: Transactions) -> String {
    let checked = "Each applies only to a document that still holds what you loaded.";
    match (transactions, changes > 1) {
        (Transactions::Multi { .. }, true) => format!(
            "In {place}, in this order. {checked} They apply in one transaction: all of them or \
             none."
        ),
        (_, false) => format!("In {place}. {checked}"),
        (Transactions::None, true) => format!(
            "In {place}, in this order. {checked} This server has no transactions, so they are \
             not all or nothing: each applies as soon as it runs, and Apply stops at the first \
             failure, keeping the rest staged."
        ),
    }
}

/// What applying staged changes did, in one line: `Applied 2 changes ·
/// matched 1 · modified 1 · inserted 1`.
pub fn applied(changes: usize, outcome: Outcome) -> String {
    let what = match changes {
        1 => "Applied 1 change".to_string(),
        n => format!("Applied {n} changes"),
    };
    match counts(outcome) {
        Some(counts) => format!("{what} · {counts}"),
        None => what,
    }
}

/// Why applying staged changes stopped, and what it kept: a document that
/// changed since it was loaded says so, with the change it stopped at, and
/// changes whose answer never came may have applied, which only a reload
/// tells. `statements` are the changes as they were sent, in order.
pub fn apply_failed(failure: &ApplyFailure, statements: &[String]) -> String {
    let mut why = match &failure.error {
        QueryError::RowMismatch { sql, .. } => {
            format!("{sql}: This document changed since you loaded it. Reload it and edit again.")
        }
        error => describe_error(error.clone()),
    };
    if !why.ends_with('.') {
        why.push('.');
    }
    let applied = failure.applied.min(statements.len());
    let unknown = &statements[applied..(applied + failure.unknown).min(statements.len())];
    let mut said = vec![why];
    match applied {
        0 if unknown.is_empty() => said.push("Nothing was applied.".into()),
        0 => {}
        n => said.push(format!(
            "Applied {} of {}, which are no longer staged: {}.",
            n,
            statements.len(),
            statements[..n].join("; ")
        )),
    }
    match unknown {
        [] => {}
        [one] if statements.len() > 1 => said.push(format!(
            "{one} may have applied: reload before applying again."
        )),
        [_] => said.push("It may have applied: reload before applying again.".into()),
        _ => said.push("They may have applied: reload before applying again.".into()),
    }
    said.join(" ")
}

/// Why Apply waits after an apply whose outcome is unknown.
pub const RELOAD_FIRST: &str =
    "The last Apply may have applied: reload before applying again, or it could apply twice";

/// The audit record of staged changes applied to a collection: the
/// statements they became, which hold the diff, not whole documents.
pub fn record_changes(
    db: &DocDb,
    project: &Path,
    collection: &CollectionRef,
    statements: &[String],
    outcome: String,
    changed: Option<(usize, u64)>,
    failed: bool,
) -> AuditRecord {
    AuditRecord::new(
        project,
        format!("{} ({})", db.name, collection.display()),
        db.environment,
        MODE,
        AuditAction::Apply,
        statements.join("\n"),
    )
    .finished(outcome, changed, failed)
}

/// What the audit log says of a preview, and whether it failed.
fn previewed_outcome(impacts: &Result<Vec<Impact>, QueryError>) -> (String, bool) {
    match impacts {
        Ok(impacts) => (
            undone(impacts).unwrap_or_else(|| "Previewed without running it".into()),
            false,
        ),
        Err(error) => (
            format!("Preview failed: {}", describe_error(error.clone())),
            true,
        ),
    }
}

/// The review's steps, one per statement.
pub fn planned(script: &Script) -> Vec<Planned> {
    script
        .statements
        .iter()
        .map(|statement| {
            let text = script.text.get(statement.span.clone()).unwrap_or_default();
            let line = one_line(text);
            Planned {
                shown: line.clone(),
                line,
                writes: writes(statement),
                danger: statement
                    .info
                    .dangers
                    .first()
                    .map(|danger| danger.to_string()),
            }
        })
        .collect()
}

/// A statement on one line, its runs of spaces and line breaks made one
/// space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The review's subtitle: where the script runs, and whether it is all or
/// nothing (`atomic`, see [`DocumentSession::all_or_nothing`]). Whatever is
/// not all or nothing says so.
pub fn heading(place: &str, statements: usize, atomic: bool, transactions: Transactions) -> String {
    let many = statements > 1;
    match (atomic, many, transactions) {
        (true, true, _) => format!(
            "In {place}, in this order. The script runs in one transaction: all of it or none."
        ),
        (true, false, _) => format!("In {place}. It runs in a transaction: all of it or none."),
        (false, true, Transactions::None) => format!(
            "In {place}, in this order. This server has no transactions, so the script is not \
             all or nothing: each statement applies as soon as it runs, and the script stops at \
             the first failure."
        ),
        (false, false, Transactions::None) => format!(
            "In {place}. This server has no transactions, so it is not all or nothing: what it \
             changes applies as it runs, and a failure can leave part of it done."
        ),
        (false, true, Transactions::Multi { .. }) => format!(
            "In {place}, in this order. The script runs outside a transaction, so it is not all \
             or nothing: each statement applies as soon as it runs, and the script stops at the \
             first failure."
        ),
        (false, false, Transactions::Multi { .. }) => format!(
            "In {place}. It runs outside a transaction, so it is not all or nothing: what it \
             changes applies as it runs, and a failure can leave part of it done."
        ),
    }
}

/// The details of each step: what the preview found, or, when it could
/// not run, that what each write changes is unknown.
fn review_details(
    planned: &[Planned],
    impacts: Result<Vec<Impact>, QueryError>,
    language: &Language,
) -> Vec<Vec<Detail>> {
    match impacts {
        Ok(impacts) => planned
            .iter()
            .enumerate()
            .map(|(index, step)| match (step.writes, impacts.get(index)) {
                (true, Some(impact)) => details(impact, language),
                (true, None) => vec![unknown(None)],
                (false, _) => Vec::new(),
            })
            .collect(),
        Err(error) => {
            let why = format!("The preview failed: {}", describe_error(error));
            planned
                .iter()
                .map(|step| match step.writes {
                    true => vec![unknown(Some(why.clone()))],
                    false => Vec::new(),
                })
                .collect()
        }
    }
}

/// What a preview found about one statement, as the review's lines; the
/// documents it would touch go by what the engine calls their identity.
pub fn details(impact: &Impact, language: &Language) -> Vec<Detail> {
    let mut details = Vec::new();
    match &impact.estimate {
        Estimate::Reads => return details,
        Estimate::Exact(outcome) => details.push(Detail {
            subject: "Exact (run and undone in a transaction)".into(),
            text: counts(*outcome).unwrap_or_else(|| "changes nothing".into()),
            danger: None,
        }),
        Estimate::Estimated(count) => match estimated(*count) {
            Some(text) => details.push(Detail {
                subject: "Estimated".into(),
                text,
                danger: None,
            }),
            None => details.push(unknown(None)),
        },
        Estimate::Unknown => details.push(unknown(None)),
    }
    if let Some(note) = &impact.note {
        details.push(Detail {
            subject: "Note".into(),
            text: note.clone(),
            danger: None,
        });
    }
    if let Some(named) = sample(impact, language.render) {
        details.push(Detail {
            subject: language.key_name.into(),
            text: named,
            danger: None,
        });
    }
    details
}

fn unknown(why: Option<String>) -> Detail {
    Detail {
        subject: "Unknown".into(),
        text: why.unwrap_or_else(|| "Kuery could not tell what it would change".into()),
        danger: Some("What it would change is unknown".into()),
    }
}

/// `about 125 documents`, or `None` when the count is unknown.
fn estimated(count: Count) -> Option<String> {
    match count {
        Count::Exact(n) => Some(format!("about {}", documents(n))),
        Count::AtLeast(n) => Some(format!("at least {}", documents(n))),
        Count::Unknown => None,
    }
}

fn documents(n: u64) -> String {
    match n {
        1 => "1 document".into(),
        n => format!("{} documents", thousands(n)),
    }
}

/// The keys of the first documents the statement would touch: `1, 2, 3,
/// 4, 5 and 15 more`.
fn sample(impact: &Impact, render: fn(&Value, Style) -> String) -> Option<String> {
    let keys: Vec<String> = impact
        .sample
        .iter()
        .filter_map(|document| document.key())
        .map(|key| render(&key.0, Style::Compact))
        .collect();
    if keys.is_empty() {
        return None;
    }
    let named = keys[..keys.len().min(NAMED)].join(", ");
    Some(match keys.len().saturating_sub(NAMED) {
        0 => named,
        more => format!("{named} and {more} more"),
    })
}

/// What a statement changed, in one line (`matched 2 · modified 1`), or
/// `None` when it changed nothing.
pub fn counts(outcome: Outcome) -> Option<String> {
    let counts = [
        ("matched", outcome.matched),
        ("modified", outcome.modified),
        ("inserted", outcome.inserted),
        ("deleted", outcome.deleted),
        ("upserted", outcome.upserted),
    ];
    let parts: Vec<String> = counts
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(what, n)| format!("{what} {}", thousands(*n)))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The documents a run changed, for the audit log.
pub fn changed(outcome: Outcome) -> u64 {
    outcome.modified + outcome.inserted + outcome.deleted + outcome.upserted
}

/// What a preview ran in a transaction and undid, for the audit log; `None`
/// when it ran nothing.
fn undone(impacts: &[Impact]) -> Option<String> {
    let mut total = Outcome::default();
    let mut ran = false;
    for impact in impacts {
        if let Estimate::Exact(outcome) = impact.estimate {
            total.add(outcome);
            ran = true;
        }
    }
    ran.then(|| {
        let what = counts(total).unwrap_or_else(|| "changed nothing".into());
        format!("Previewed in a transaction and undone: {what}")
    })
}

/// A write's audit record: the script, as it ran in its place.
pub fn record(
    db: &DocDb,
    project: &Path,
    script: &Script,
    action: AuditAction,
    outcome: String,
    changed: Option<(usize, u64)>,
    failed: bool,
) -> AuditRecord {
    AuditRecord::new(
        project,
        format!("{} ({})", db.name, script.place()),
        db.environment,
        MODE,
        action,
        script.text.trim(),
    )
    .finished(outcome, changed, failed)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kuery_core::document::value::Document;

    use super::*;

    fn script(text: &str) -> Script {
        Script {
            start: vec!["shop".into()],
            text: text.into(),
            statements: (kuery_mongo::LANGUAGE.analyze)(text).unwrap(),
            heavy: Heavy::Refuse,
        }
    }

    const MULTI: Transactions = Transactions::Multi {
        lifetime: Duration::from_secs(60),
    };

    #[test]
    fn plans_each_statement_of_a_script() {
        let script = script(
            "db.orders.find({status: 'paid'})\ndb.orders.updateMany({status: 'paid'},\n  {$set: {shipped: true}})\ndb.orders.deleteMany({})",
        );
        assert!(script.writes());
        let planned = planned(&script);
        assert_eq!(planned.len(), 3);
        assert!(!planned[0].writes && planned[0].danger.is_none());
        assert_eq!(
            planned[1].line,
            "db.orders.updateMany({status: 'paid'}, {$set: {shipped: true}})"
        );
        assert!(planned[1].writes && planned[1].danger.is_none());
        assert_eq!(
            planned[2].danger.as_deref(),
            Some("deleteMany with an empty filter removes every document in orders")
        );
        assert!(!self::script("db.orders.find()").writes());
    }

    #[test]
    fn the_heading_says_whether_the_script_is_all_or_nothing() {
        assert_eq!(
            heading("shop", 2, true, MULTI),
            "In shop, in this order. The script runs in one transaction: all of it or none."
        );
        // One statement that changes documents runs in a transaction too.
        assert_eq!(
            heading("shop", 1, true, MULTI),
            "In shop. It runs in a transaction: all of it or none."
        );
        let standalone = heading("shop", 3, false, Transactions::None);
        assert!(
            standalone.starts_with("In shop, in this order.")
                && standalone.contains("not all or nothing")
                && standalone.contains("stops at the first failure"),
            "{standalone}"
        );
        // Whatever does not run in one says it is not all or nothing.
        for (statements, transactions) in [(1, Transactions::None), (1, MULTI), (2, MULTI)] {
            let heading = heading("shop", statements, false, transactions);
            assert!(heading.contains("not all or nothing"), "{heading}");
        }
        assert!(heading("shop", 1, false, Transactions::None).contains("no transactions"));
        assert!(heading("shop", 1, false, MULTI).contains("outside a transaction"));
    }

    fn impact(estimate: Estimate) -> Impact {
        Impact {
            summary: "updateMany shop.orders".into(),
            estimate,
            sample: Vec::new(),
            note: None,
        }
    }

    const MONGO: &Language = &kuery_mongo::LANGUAGE;

    #[test]
    fn labels_each_kind_of_preview_and_an_unknown_one_is_dangerous() {
        let exact = details(
            &impact(Estimate::Exact(Outcome {
                matched: 3,
                modified: 2,
                ..Outcome::default()
            })),
            MONGO,
        );
        assert_eq!(exact[0].subject, "Exact (run and undone in a transaction)");
        assert_eq!(exact[0].text, "matched 3 · modified 2");
        assert!(exact[0].danger.is_none());
        let mut counted = impact(Estimate::Estimated(Count::AtLeast(10_000)));
        counted.sample = (1..=7)
            .map(|n| Document(vec![("_id".into(), Value::Int32(n))]))
            .collect();
        counted.note = Some("It failed in the transaction".into());
        let counted = details(&counted, MONGO);
        assert_eq!(
            (counted[0].subject.as_str(), counted[0].text.as_str()),
            ("Estimated", "at least 10,000 documents")
        );
        assert!(counted.iter().all(|detail| detail.danger.is_none()));
        assert_eq!(counted[1].text, "It failed in the transaction");
        assert_eq!(counted[2].text, "1, 2, 3, 4, 5 and 2 more");
        assert_eq!(counted[2].subject, "_id");
        for unknown in [Estimate::Unknown, Estimate::Estimated(Count::Unknown)] {
            let unknown = details(&impact(unknown), MONGO);
            assert_eq!(unknown[0].subject, "Unknown");
            assert!(unknown[0].danger.is_some());
        }
        assert!(details(&impact(Estimate::Reads), MONGO).is_empty());
    }

    #[test]
    fn names_the_documents_by_what_the_engine_calls_their_identity() {
        let keyed = Language {
            key_name: "key",
            ..kuery_mongo::LANGUAGE
        };
        let mut found = impact(Estimate::Estimated(Count::Exact(1)));
        found.sample = vec![Document(vec![("_id".into(), Value::Int32(7))])];
        let details = details(&found, &keyed);
        assert_eq!(
            (details[1].subject.as_str(), details[1].text.as_str()),
            ("key", "7")
        );
    }

    #[test]
    fn audits_every_preview_and_says_when_one_failed() {
        let exact = impact(Estimate::Exact(Outcome {
            inserted: 2,
            ..Outcome::default()
        }));
        assert_eq!(
            previewed_outcome(&Ok(vec![exact])),
            (
                "Previewed in a transaction and undone: inserted 2".into(),
                false
            )
        );
        assert_eq!(
            previewed_outcome(&Ok(vec![impact(Estimate::Unknown)])),
            ("Previewed without running it".into(), false)
        );
        assert_eq!(
            previewed_outcome(&Err(QueryError::Cancelled)),
            ("Preview failed: Query cancelled".into(), true)
        );
    }

    #[test]
    fn a_preview_that_failed_makes_every_write_unknown() {
        let planned = planned(&script("db.orders.find()\ndb.orders.insertOne({a: 1})"));
        let details = review_details(&planned, Err(QueryError::Cancelled), MONGO);
        assert!(details[0].is_empty());
        assert_eq!(details[1][0].subject, "Unknown");
        assert!(details[1][0].danger.is_some());
        assert!(details[1][0].text.starts_with("The preview failed"));
    }

    #[test]
    fn staged_changes_are_reviewed_as_their_statements() {
        let orders = CollectionRef::new(&["shop", "orders"]);
        let changes = [
            DocChange::Insert {
                document: Document(vec![("a".into(), Value::Int32(1))]),
            },
            DocChange::Delete {
                key: kuery_core::document::value::DocKey(Value::Int32(7)),
                version: None,
                expect: Vec::new(),
            },
        ];
        let planned = changes_planned(MONGO, &orders, &changes);
        let lines: Vec<&str> = planned.iter().map(|step| step.line.as_str()).collect();
        assert_eq!(
            lines,
            [
                "db.orders.insertOne({a: 1})",
                "db.orders.deleteOne({_id: 7})"
            ]
        );
        assert!(
            planned
                .iter()
                .all(|step| step.writes && step.danger.is_none())
        );

        let atomic = changes_heading("shop.orders", 2, MULTI);
        assert!(atomic.contains("all of them or none"), "{atomic}");
        let standalone = changes_heading("shop.orders", 2, Transactions::None);
        assert!(
            standalone.contains("not all or nothing") && standalone.contains("keeping the rest"),
            "{standalone}"
        );
        assert!(changes_heading("shop.orders", 1, MULTI).contains("still holds what you loaded"));
    }

    #[test]
    fn says_what_an_apply_kept() {
        let statements = vec![
            "db.a.insertOne({})".to_string(),
            "db.a.deleteOne({_id: 1})".into(),
        ];
        let changed = ApplyFailure {
            applied: 0,
            unknown: 0,
            error: QueryError::RowMismatch {
                sql: "update of 7 (qty)".into(),
                expected: 1,
                actual: 0,
            },
        };
        assert_eq!(
            apply_failed(&changed, &statements),
            "update of 7 (qty): This document changed since you loaded it. Reload it and edit \
             again. Nothing was applied."
        );
        let partial = ApplyFailure {
            applied: 1,
            unknown: 0,
            error: QueryError::Database {
                code: "11000".into(),
                message: "E11000 duplicate key".into(),
            },
        };
        let said = apply_failed(&partial, &statements);
        assert!(
            said.contains("Applied 1 of 2") && said.contains("db.a.insertOne({})"),
            "{said}"
        );
        assert!(!said.contains("may have applied"), "{said}");
        assert_eq!(
            applied(
                2,
                Outcome {
                    matched: 1,
                    modified: 1,
                    inserted: 1,
                    ..Outcome::default()
                }
            ),
            "Applied 2 changes · matched 1 · modified 1 · inserted 1"
        );
    }

    #[test]
    fn an_apply_without_an_answer_says_what_may_have_applied() {
        let statements = vec![
            "db.a.insertOne({})".to_string(),
            "db.a.deleteOne({_id: 1})".into(),
        ];
        let lost = |applied, unknown| ApplyFailure {
            applied,
            unknown,
            error: QueryError::Connection("no answer".into()),
        };
        // The first one got no answer: never "Nothing was applied".
        assert_eq!(
            apply_failed(&lost(0, 1), &statements),
            "Connection lost: no answer. db.a.insertOne({}) may have applied: reload before \
             applying again."
        );
        let second = apply_failed(&lost(1, 1), &statements);
        assert!(
            second.contains("Applied 1 of 2")
                && second.ends_with(
                    "db.a.deleteOne({_id: 1}) may have applied: reload before applying again."
                ),
            "{second}"
        );
        // A commit that did not say leaves all of them unknown.
        assert!(
            apply_failed(&lost(0, 2), &statements)
                .ends_with("They may have applied: reload before applying again."),
        );
        assert!(
            apply_failed(&lost(0, 1), &statements[..1])
                .ends_with(" It may have applied: reload before applying again."),
        );
    }

    #[test]
    fn counts_what_a_write_changed() {
        let outcome = Outcome {
            matched: 4,
            modified: 3,
            inserted: 2,
            deleted: 1,
            upserted: 1,
            returned: 9,
        };
        assert_eq!(changed(outcome), 7);
        assert_eq!(counts(Outcome::default()), None);
        assert_eq!(
            undone(&[
                impact(Estimate::Reads),
                impact(Estimate::Exact(Outcome {
                    inserted: 2,
                    ..Outcome::default()
                })),
            ])
            .as_deref(),
            Some("Previewed in a transaction and undone: inserted 2")
        );
        assert_eq!(undone(&[impact(Estimate::Unknown)]), None);
    }
}
