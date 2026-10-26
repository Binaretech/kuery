//! The session's rules, against a fake adapter that logs what it is asked
//! to run: `select N` returns N rows, `update N` changes N rows,
//! `select fail` and `update fail` fail, `select slow` runs until cancelled
//! and then ends as if nothing happened (like MySQL's killed `SLEEP`), and
//! `select interrupted` runs until cancelled and then fails as cancelled
//! (like Postgres). `select endless` streams rows until the reader goes away;
//! `select stubborn` ignores that and then fails some other way.
//! Every cancel the session sends is logged as `cancel`.
//! `select deadlock` fails the way a deadlock victim does on SQL Server (the
//! server rolls back the whole transaction, so the next `rollback to` fails),
//! and `select lost` the way a dropped connection does. `select panic` and
//! `update panic` panic, like a driver with a bug. A session opened with
//! [`open_failing`] also fails the statements it names.

use std::sync::Mutex;

use futures::executor::block_on;

use super::*;
use crate::catalog::{CatalogColumn, ForeignKey, TableInfo, TableKind};
use crate::diagram::OnDelete;
use crate::edit::{self, EditTarget, KeyValue, RowChange};
use crate::sql::{MYSQL, POSTGRES, SQLITE};

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

struct Fake {
    dialect: &'static Dialect,
    log: Log,
    stop: Arc<AtomicBool>,
    /// Why the next `rollback to` fails: the transaction is gone.
    gone: Option<QueryError>,
    failing: &'static [&'static str],
}

impl Adapter for Fake {
    fn dialect(&self) -> &'static Dialect {
        self.dialect
    }

    fn server(&self) -> String {
        "Fake 1.0".into()
    }

    fn execute(&mut self, sql: &str, rows: &mut dyn RowSink) -> Result<u64, QueryError> {
        self.log.0.lock().unwrap().push(sql.to_string());
        if self.failing.contains(&sql) {
            return Err(QueryError::Database {
                code: "42000".into(),
                message: "failed on purpose".into(),
            });
        }
        let words: Vec<&str> = sql.split_whitespace().collect();
        match words.as_slice() {
            ["rollback", "to", ..] if self.gone.is_some() => Err(self.gone.clone().unwrap()),
            ["rollback"] => {
                self.gone = None;
                Ok(0)
            }
            ["select", "deadlock"] => {
                let victim = QueryError::Database {
                    code: "1205".into(),
                    message: "chosen as the deadlock victim".into(),
                };
                self.gone = Some(QueryError::Database {
                    code: "3903".into(),
                    message: "no corresponding begin transaction".into(),
                });
                Err(victim)
            }
            ["select", "lost"] => {
                let lost = QueryError::Connection("reset by peer".into());
                self.gone = Some(lost.clone());
                Err(lost)
            }
            [_, "panic"] => panic!("the fake driver broke"),
            [_, "fail"] => Err(QueryError::Database {
                code: "42000".into(),
                message: "failed on purpose".into(),
            }),
            ["select", "slow"] => {
                rows.columns(vec!["slow".into()], None)?;
                while !self.stop.swap(false, Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                rows.row(&[Some("1")])?;
                Ok(0)
            }
            ["select", "interrupted"] => {
                rows.columns(vec!["interrupted".into()], None)?;
                while !self.stop.swap(false, Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(QueryError::Cancelled)
            }
            ["select", "stubborn"] => {
                // Sends until the reader is surely gone, then fails anyway.
                let _ = rows.columns(vec!["n".into()], None);
                while rows.row(&[Some("1")]).is_ok() {}
                Err(QueryError::Connection("closed after the statement".into()))
            }
            ["select", "endless"] => {
                rows.columns(vec!["n".into()], None)?;
                loop {
                    rows.row(&[Some("1")])?;
                }
            }
            ["select", n] => {
                let count: usize = n.parse().unwrap_or(1);
                let kinds = rows.wants_kinds().then(|| vec![ColumnKind::Integer]);
                rows.columns(vec!["n".into()], kinds)?;
                for i in 0..count {
                    rows.row(&[Some(&i.to_string())])?;
                }
                Ok(0)
            }
            ["update", n] => Ok(n.parse().unwrap_or(0)),
            _ => Ok(0),
        }
    }

    fn limit(&mut self, _: &RunOptions) -> Result<(), QueryError> {
        Ok(())
    }

    fn canceller(&self) -> Canceller {
        let (stop, log) = (self.stop.clone(), self.log.clone());
        Arc::new(move || {
            log.0.lock().unwrap().push("cancel".into());
            stop.store(true, Ordering::SeqCst);
        })
    }

    fn overview(&mut self) -> Result<CatalogOverview, QueryError> {
        let info = |schema: &str, name: &str, kind| TableInfo {
            table: TableRef::new(schema, name),
            kind,
            estimated_rows: Some(10),
            comment: None,
        };
        Ok(CatalogOverview {
            schemas: vec!["shop".into()],
            tables: vec![
                info("shop", "customers", TableKind::Table),
                info("shop", "orders", TableKind::Table),
                info("shop", "big_orders", TableKind::View),
            ],
            ..CatalogOverview::default()
        })
    }

    fn definition(&mut self, _: &TableRef) -> Result<TableDefinition, QueryError> {
        Ok(TableDefinition::default())
    }

    fn columns(&mut self, _: Option<&str>) -> Result<Vec<CatalogColumn>, QueryError> {
        let column = |table: &str, name: &str, nullable, primary_key| CatalogColumn {
            table: TableRef::new("shop", table),
            name: name.into(),
            data_type: "int".into(),
            nullable,
            primary_key,
        };
        Ok(vec![
            column("customers", "id", false, true),
            column("orders", "id", false, true),
            column("orders", "customer id", true, false),
            column("big_orders", "id", false, false),
        ])
    }

    fn foreign_keys(&mut self, _: &str) -> Result<Vec<ForeignKey>, QueryError> {
        Ok(vec![ForeignKey {
            name: "orders_customer".into(),
            from: TableRef::new("shop", "orders"),
            from_columns: vec!["customer id".into()],
            to: TableRef::new("shop", "customers"),
            to_columns: vec!["id".into()],
            on_delete: OnDelete::Cascade,
        }])
    }

    fn search_path(&mut self) -> Result<Vec<String>, QueryError> {
        Ok(vec!["shop".into()])
    }
}

fn open_with(dialect: &'static Dialect, access: Access) -> (Session, Log) {
    open_failing(dialect, access, &[])
}

fn open_failing(
    dialect: &'static Dialect,
    access: Access,
    failing: &'static [&'static str],
) -> (Session, Log) {
    let log = Log::default();
    let opener_log = log.clone();
    let opener: Opener = Arc::new(move |_| {
        Ok(Box::new(Fake {
            dialect,
            log: opener_log.clone(),
            stop: Arc::new(AtomicBool::new(false)),
            gone: None,
            failing,
        }))
    });
    (block_on(Session::open(opener, access)).unwrap(), log)
}

fn open(access: Access) -> (Session, Log) {
    open_with(&POSTGRES, access)
}

fn drain(rx: async_channel::Receiver<Event>) -> (Vec<usize>, Result<Summary, QueryError>) {
    block_on(async {
        let mut batches = Vec::new();
        while let Ok(event) = rx.recv().await {
            match event {
                Event::Batch(batch) => batches.push(batch.rows()),
                Event::Finished(summary) => return (batches, Ok(summary)),
                Event::Failed(error) => return (batches, Err(error)),
                _ => {}
            }
        }
        (batches, Err(QueryError::Cancelled))
    })
}

#[test]
fn a_job_that_panics_ends_the_session_and_says_so() {
    let (session, _) = open(Access::ReadWrite);
    let (_, outcome) =
        drain(session.run_in_transaction("update panic".into(), RunOptions::default()));
    assert!(
        matches!(outcome, Err(QueryError::Connection(_))),
        "{outcome:?}"
    );
    assert!(!session.in_transaction());
    let next = block_on(session.fetch_all("select 1".into(), RunOptions::default()));
    assert!(matches!(next, Err(QueryError::Connection(_))), "{next:?}");

    let (session, _) = open(Access::ReadOnly);
    let outcome = block_on(session.fetch_all("select panic".into(), RunOptions::default()));
    assert!(
        matches!(outcome, Err(QueryError::Connection(_))),
        "{outcome:?}"
    );
    let (_, next) = drain(session.run_read_only("select 1".into(), RunOptions::default()));
    assert!(matches!(next, Err(QueryError::Connection(_))), "{next:?}");
}

#[test]
fn reads_in_a_read_only_transaction_in_growing_batches() {
    let (session, log) = open(Access::ReadOnly);
    assert_eq!(session.server_label(), "Fake 1.0");
    let (batches, summary) = drain(session.run_read_only(
        "select 2600".into(),
        RunOptions {
            first_batch_rows: 100,
            ..RunOptions::default()
        },
    ));
    assert_eq!(batches, [100, 400, 1600, 500]);
    assert_eq!(summary.unwrap().rows, 2600);
    assert_eq!(
        log.take(),
        [
            "begin transaction read only",
            "select 1",
            "select 2600",
            "rollback"
        ]
    );
    let set = block_on(session.fetch_all("select 3".into(), RunOptions::default())).unwrap();
    assert_eq!((set.rows(), set.kinds()), (3, &[ColumnKind::Integer][..]));
}

#[test]
fn a_read_whose_setup_fails_ends_what_it_began() {
    // Postgres: the transaction began, so it is rolled back, not left
    // aborted for every later query.
    let (session, log) = open_failing(&POSTGRES, Access::ReadOnly, &["select 1"]);
    let outcome = block_on(session.fetch_all("select 2".into(), RunOptions::default()));
    assert!(outcome.is_err());
    assert_eq!(
        log.take(),
        ["begin transaction read only", "select 1", "rollback"]
    );
    // SQLite: query_only was turned on, so it is turned off again.
    let (session, log) = open_failing(&SQLITE, Access::ReadWrite, &["begin"]);
    let outcome = block_on(session.fetch_all("select 2".into(), RunOptions::default()));
    assert!(outcome.is_err());
    assert_eq!(
        log.take(),
        [
            "pragma query_only = on",
            "begin",
            "rollback",
            "pragma query_only = off"
        ]
    );
}

#[test]
fn ending_a_read_runs_every_step_even_after_one_fails() {
    let (session, log) = open_failing(&SQLITE, Access::ReadWrite, &["rollback"]);
    block_on(session.fetch_all("select 2".into(), RunOptions::default())).unwrap();
    assert_eq!(
        log.take(),
        [
            "pragma query_only = on",
            "begin",
            "select 2",
            "rollback",
            "pragma query_only = off"
        ]
    );
}

#[test]
fn rejects_writes_before_the_adapter_sees_them() {
    let (session, log) = open(Access::ReadWrite);
    let (_, outcome) =
        drain(session.run_read_only("delete from orders".into(), RunOptions::default()));
    assert!(matches!(outcome, Err(QueryError::Rejected(_))));
    assert!(log.take().is_empty());
    let (reader, _) = open(Access::ReadOnly);
    let (_, refused) = drain(reader.run_in_transaction("update 1".into(), RunOptions::default()));
    assert!(matches!(refused, Err(QueryError::WritesDisabled)));
}

#[test]
fn write_transactions_use_a_savepoint_per_script() {
    let (session, log) = open(Access::ReadWrite);
    let (_, summary) =
        drain(session.run_in_transaction("update 2; update 3".into(), RunOptions::default()));
    let summary = summary.unwrap();
    assert_eq!((summary.affected, summary.statements), (5, 2));
    assert!(session.in_transaction());
    assert_eq!(
        log.take(),
        [
            "begin transaction read write",
            "savepoint kuery_run",
            "update 2",
            "update 3",
            "release savepoint kuery_run"
        ]
    );

    // A failing script undoes only itself; the transaction stays.
    let (_, failed) =
        drain(session.run_in_transaction("update 1; update fail".into(), RunOptions::default()));
    assert!(matches!(failed, Err(QueryError::Database { .. })));
    assert!(session.in_transaction());
    assert_eq!(
        log.take(),
        [
            "savepoint kuery_run",
            "update 1",
            "update fail",
            "rollback to savepoint kuery_run",
            "release savepoint kuery_run"
        ]
    );

    // Reads inside see the transaction, under a savepoint they undo.
    drain(session.run_read_only("select 1".into(), RunOptions::default()))
        .1
        .unwrap();
    assert_eq!(
        log.take(),
        [
            "savepoint kuery_read",
            "select 1",
            "rollback to savepoint kuery_read",
            "release savepoint kuery_read"
        ]
    );

    block_on(session.commit()).unwrap();
    assert!(!session.in_transaction());
    assert_eq!(log.take(), ["commit"]);
    // Nothing open: nothing to roll back.
    block_on(session.rollback()).unwrap();
    assert!(log.take().is_empty());
}

#[test]
fn full_writes_keep_whole_scripts_or_nothing() {
    let (session, log) = open(Access::ReadWrite);
    drain(session.run_autocommit("update 1".into(), RunOptions::default()))
        .1
        .unwrap();
    assert_eq!(log.take(), ["update 1"]);
    let (_, failed) =
        drain(session.run_autocommit("update 1; update fail".into(), RunOptions::default()));
    assert!(failed.is_err());
    assert_eq!(
        log.take(),
        [
            "begin transaction read write",
            "update 1",
            "update fail",
            "rollback"
        ]
    );
    // Not while a write transaction is open.
    drain(session.run_in_transaction("update 1".into(), RunOptions::default()))
        .1
        .unwrap();
    let (_, refused) = drain(session.run_autocommit("update 1".into(), RunOptions::default()));
    assert!(matches!(refused, Err(QueryError::TransactionOpen)));
}

#[test]
fn previews_count_and_undo() {
    let (session, log) = open(Access::ReadWrite);
    let impacts =
        block_on(session.preview("update 4; select 2".into(), RunOptions::default())).unwrap();
    assert_eq!(
        impacts,
        [
            StatementImpact {
                rows: 4,
                returns_rows: false
            },
            StatementImpact {
                rows: 2,
                returns_rows: true
            }
        ]
    );
    assert_eq!(
        log.take(),
        [
            "begin transaction read write",
            "update 4",
            "select 2",
            "rollback"
        ]
    );
}

#[test]
fn applies_row_changes_checking_each_one() {
    let (session, log) = open(Access::ReadWrite);
    let target = EditTarget {
        table: TableRef::new("public", "update"),
        columns: vec!["id".into()],
        primary_key: vec!["id".into()],
        binary_columns: Vec::new(),
        dialect: &POSTGRES,
    };
    let delete = edit::build(
        &target,
        &RowChange::Delete {
            key: vec![KeyValue {
                column: "id".into(),
                value: "1".into(),
            }],
        },
    )
    .unwrap();
    // The fake changes no row for a DELETE: the check catches it.
    let outcome = block_on(session.apply(vec![delete]));
    assert!(matches!(
        outcome,
        Err(QueryError::RowMismatch { actual: 0, .. })
    ));
    let logged = log.take();
    assert_eq!(
        logged.first().map(String::as_str),
        Some("begin transaction read write")
    );
    assert_eq!(logged.last().map(String::as_str), Some("rollback"));
}

#[test]
fn refuses_what_mysql_would_commit_by_itself() {
    let (session, log) = open_with(&MYSQL, Access::ReadWrite);
    let (_, refused) = drain(session.run_in_transaction(
        "alter table t add column x int".into(),
        RunOptions::default(),
    ));
    assert!(
        matches!(refused, Err(QueryError::Unsupported(_))),
        "{refused:?}"
    );
    let previewed = block_on(session.preview("truncate table t".into(), RunOptions::default()));
    assert!(matches!(previewed, Err(QueryError::Unsupported(_))));
    assert!(log.take().is_empty());
    // Full write runs it.
    drain(session.run_autocommit(
        "alter table t add column x int".into(),
        RunOptions::default(),
    ))
    .1
    .unwrap();
    assert_eq!(log.take(), ["alter table t add column x int"]);
}

#[test]
fn refuses_on_mysql_what_it_cannot_read() {
    // Valid MySQL DDL, so it commits by itself, but sqlparser cannot read it.
    let sql = "create tablespace ts add datafile 'ts.ibd'";
    assert!(analysis::analyze_in(&MYSQL, sql).is_err());
    let (session, log) = open_with(&MYSQL, Access::ReadWrite);
    let (_, refused) = drain(session.run_in_transaction(sql.into(), RunOptions::default()));
    assert!(
        matches!(refused, Err(QueryError::Unsupported(_))),
        "{refused:?}"
    );
    let previewed = block_on(session.preview(sql.into(), RunOptions::default()));
    assert!(
        matches!(previewed, Err(QueryError::Unsupported(_))),
        "{previewed:?}"
    );
    assert!(log.take().is_empty());
}

#[test]
fn a_cancelled_statement_counts_as_cancelled_even_if_it_ends_well() {
    let (session, _) = open(Access::ReadOnly);
    let rx = session.run_read_only("select slow".into(), RunOptions::default());
    std::thread::sleep(Duration::from_millis(50));
    session.cancel();
    let (_, outcome) = drain(rx);
    assert!(matches!(outcome, Err(QueryError::Cancelled)), "{outcome:?}");
    // And the next run is not.
    drain(session.run_read_only("select 1".into(), RunOptions::default()))
        .1
        .unwrap();
}

#[test]
fn a_cancel_does_not_reach_the_next_run() {
    let (session, _) = open(Access::ReadOnly);
    let rx = session.run_read_only("select slow".into(), RunOptions::default());
    std::thread::sleep(Duration::from_millis(50));
    session.cancel();
    assert!(matches!(drain(rx).1, Err(QueryError::Cancelled)));
    // Started while the cancel's retries could still be waiting.
    let next = session.run_read_only("select slow".into(), RunOptions::default());
    std::thread::sleep(Duration::from_millis(400));
    let finished = std::iter::from_fn(|| next.try_recv().ok())
        .any(|event| matches!(event, Event::Finished(_) | Event::Failed(_)));
    assert!(!finished, "the next run was stopped by the earlier cancel");
    session.cancel();
    assert!(matches!(drain(next).1, Err(QueryError::Cancelled)));
}

#[test]
fn a_cancel_the_server_reports_is_not_sent_again() {
    let (session, log) = open(Access::ReadOnly);
    let rx = session.run_read_only("select interrupted".into(), RunOptions::default());
    std::thread::sleep(Duration::from_millis(50));
    session.cancel();
    assert!(matches!(drain(rx).1, Err(QueryError::Cancelled)));
    // Postgres stops whatever runs when a cancel arrives, so a second one
    // sent after the statement stopped would stop the next one.
    let next = session.run_read_only("select slow".into(), RunOptions::default());
    std::thread::sleep(Duration::from_millis(400));
    let finished = std::iter::from_fn(|| next.try_recv().ok())
        .any(|event| matches!(event, Event::Finished(_) | Event::Failed(_)));
    assert!(!finished, "the next run was stopped by a repeated cancel");
    session.cancel();
    assert!(matches!(drain(next).1, Err(QueryError::Cancelled)));
    let cancels = log.take().into_iter().filter(|sql| sql == "cancel").count();
    assert_eq!(cancels, 2, "one cancel for each statement");
}

#[test]
fn a_reader_that_goes_away_stops_the_statement_on_the_server() {
    let (session, log) = open(Access::ReadOnly);
    drop(session.run_read_only("select endless".into(), RunOptions::default()));
    drain(session.run_read_only("select 1".into(), RunOptions::default()))
        .1
        .unwrap();
    let log = log.take();
    let endless = log.iter().position(|sql| sql == "select endless").unwrap();
    assert_eq!(
        log.get(endless + 1).map(String::as_str),
        Some("cancel"),
        "{log:?}"
    );
}

#[test]
fn only_a_cancelled_statement_is_cancelled_on_the_server() {
    let (session, log) = open(Access::ReadOnly);
    // The reader is gone, but the statement ended with another error: no
    // statement is left to stop, and a cancel could reach the next one.
    drop(session.run_read_only("select stubborn".into(), RunOptions::default()));
    drain(session.run_read_only("select 1".into(), RunOptions::default()))
        .1
        .unwrap();
    let log = log.take();
    assert!(!log.iter().any(|sql| sql == "cancel"), "{log:?}");
}

#[test]
fn composes_completion_and_diagrams_from_the_catalog() {
    let (session, log) = open(Access::ReadOnly);
    let catalog = block_on(session.completion_catalog()).unwrap();
    assert_eq!(catalog.search_path, ["shop"]);
    let orders = catalog
        .tables
        .iter()
        .find(|t| t.table.name == "orders")
        .unwrap();
    assert_eq!(orders.columns.len(), 2);
    // Catalog reads are read-only too.
    let logged = log.take();
    assert_eq!(
        logged.first().map(String::as_str),
        Some("begin transaction read only")
    );

    let diagram = block_on(session.diagram("shop".into())).unwrap();
    // Views are not drawn.
    assert_eq!(diagram.tables.len(), 2);
    let relation = &diagram.relations[0];
    assert!(relation.optional);
    assert_eq!(relation.on_delete, OnDelete::Cascade);
    let orders = diagram
        .tables
        .iter()
        .find(|t| t.table.name == "orders")
        .unwrap();
    assert!(
        orders
            .columns
            .iter()
            .any(|c| c.name == "customer id" && c.foreign_key)
    );
}

#[test]
fn reopens_another_session_alike() {
    let (session, _) = open(Access::ReadWrite);
    let other = block_on(session.reopen(Access::ReadOnly)).unwrap();
    assert_eq!(other.access(), Access::ReadOnly);
    assert_eq!(other.dialect(), &POSTGRES);
    assert!(session.has_plans() && !session.has_activity());
    let (_, refused) = drain(other.run_autocommit("update 1".into(), RunOptions::default()));
    assert!(matches!(refused, Err(QueryError::WritesDisabled)));
}

#[test]
fn a_read_that_ends_the_write_transaction_forgets_it() {
    let (session, log) = open(Access::ReadWrite);
    let write = |sql: &str| drain(session.run_in_transaction(sql.into(), RunOptions::default()));
    let read = |sql: &str| drain(session.run_read_only(sql.into(), RunOptions::default())).1;

    // A read that fails on its own leaves the transaction as it was.
    write("update 1").1.unwrap();
    assert!(matches!(
        read("select fail"),
        Err(QueryError::Database { .. })
    ));
    assert!(session.in_transaction());
    log.take();

    // The server rolled the transaction back: its savepoint is gone too.
    assert!(
        matches!(read("select deadlock"), Err(QueryError::Database { code, .. }) if code == "1205")
    );
    assert!(!session.in_transaction());
    assert_eq!(
        log.take(),
        [
            "savepoint kuery_read",
            "select deadlock",
            "rollback to savepoint kuery_read",
            "release savepoint kuery_read",
            "rollback"
        ]
    );

    // A dropped connection takes it with it.
    write("update 1").1.unwrap();
    assert!(matches!(
        read("select lost"),
        Err(QueryError::Connection(_))
    ));
    assert!(!session.in_transaction());
}
