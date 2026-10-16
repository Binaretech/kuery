//! The session's rules, against a fake adapter that logs what it is asked
//! to run: `select N` returns N rows, `update N` changes N rows,
//! `select fail` and `update fail` fail, `select slow` runs until cancelled
//! and then ends as if nothing happened (like MySQL's killed `SLEEP`).

use std::sync::Mutex;

use futures::executor::block_on;

use super::*;
use crate::catalog::{CatalogColumn, ForeignKey, TableInfo, TableKind};
use crate::diagram::OnDelete;
use crate::edit::{self, EditTarget, KeyValue, RowChange};
use crate::sql::{MYSQL, POSTGRES};

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
        let words: Vec<&str> = sql.split_whitespace().collect();
        match words.as_slice() {
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
        let stop = self.stop.clone();
        Arc::new(move || stop.store(true, Ordering::SeqCst))
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
    let log = Log::default();
    let opener_log = log.clone();
    let opener: Opener = Arc::new(move |_| {
        Ok(Box::new(Fake {
            dialect,
            log: opener_log.clone(),
            stop: Arc::new(AtomicBool::new(false)),
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
