//! What every SQL driver must do, checked against its real database: the
//! same tests for every [`crate::adapter::Adapter`], through the
//! [`Session`] the app uses. A driver's own tests call [`run`] with a way to
//! open it; passing them is what "works like the others" means.
//!
//! The SQL here is portable (a table of an integer key, a text and a
//! number), and each check cleans up after itself.

use std::time::{Duration, Instant};

use futures::executor::block_on;

use crate::adapter::Discard;
use crate::catalog::TableRef;
use crate::edit::{self, CellChange, EditTarget, KeyValue, RowChange};
use crate::read_only::Rejection;
use crate::result_set::ColumnKind;
use crate::session::{
    Access, Event, Opener, QueryError, RunOptions, Session, StatementImpact, Summary,
};

/// How to reach the database under test.
pub struct Fixture {
    pub opener: Opener,
    /// The schema scratch tables go in (`public`, `main`, a MySQL database).
    pub schema: String,
    /// A read that runs for seconds, to cancel.
    pub slow_query: String,
}

/// Runs every check.
pub fn run(fixture: &Fixture) {
    reads_and_streams(fixture);
    refuses_writes_twice_when_reading(fixture);
    applies_row_changes_checking_each_one(fixture);
    keeps_write_transactions_until_commit(fixture);
    keeps_whole_scripts_or_nothing(fixture);
    previews_and_undoes(fixture);
    reads_its_catalog(fixture);
    cancels_a_running_query(fixture);
}

fn open(fixture: &Fixture, access: Access) -> Session {
    block_on(Session::open(fixture.opener.clone(), access)).expect("opens the database")
}

fn drain(rx: async_channel::Receiver<Event>) -> Result<Summary, QueryError> {
    block_on(async {
        while let Ok(event) = rx.recv().await {
            match event {
                Event::Finished(summary) => return Ok(summary),
                Event::Failed(error) => return Err(error),
                _ => {}
            }
        }
        Err(QueryError::Cancelled)
    })
}

fn scalar(session: &Session, sql: &str) -> Option<String> {
    let set = block_on(session.fetch_all(sql.into(), RunOptions::default())).unwrap();
    set.cell(0, 0).flatten().map(str::to_string)
}

/// A table of three rows for one check: its qualified name.
struct Scratch {
    writer: Session,
    name: String,
}

impl Scratch {
    fn new(fixture: &Fixture, check: &str) -> Self {
        let writer = open(fixture, Access::ReadWrite);
        let dialect = writer.dialect();
        let table = format!("kuery_{check}_{}", std::process::id());
        let name = dialect.quote_qualified(&fixture.schema, &table);
        let _ = drain(writer.run_autocommit(format!("drop table {name}"), RunOptions::default()));
        drain(writer.run_autocommit(
            format!("create table {name} (id integer primary key, label varchar(40), qty integer)"),
            RunOptions::default(),
        ))
        .unwrap();
        drain(writer.run_autocommit(
            format!("insert into {name} values (1, 'one', 1), (2, 'two', 2), (3, NULL, 3)"),
            RunOptions::default(),
        ))
        .unwrap();
        Self { writer, name }
    }

    fn table(&self, fixture: &Fixture) -> TableRef {
        let unquoted = self.name.rsplit('.').next().unwrap_or_default();
        TableRef::new(fixture.schema.clone(), unquoted.trim_matches(['"', '`']))
    }

    fn sum(&self, session: &Session) -> Option<String> {
        scalar(session, &format!("select sum(qty) from {}", self.name))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = block_on(self.writer.rollback());
        let _ = drain(
            self.writer
                .run_autocommit(format!("drop table {}", self.name), RunOptions::default()),
        );
    }
}

pub fn reads_and_streams(fixture: &Fixture) {
    let scratch = Scratch::new(fixture, "reads");
    let reader = open(fixture, Access::ReadOnly);
    let set = block_on(reader.fetch_all(
        format!("select id, label, qty from {} order by id", scratch.name),
        RunOptions {
            first_batch_rows: 2,
            ..RunOptions::default()
        },
    ))
    .unwrap();
    assert_eq!(set.columns(), ["id", "label", "qty"]);
    assert_eq!(set.rows(), 3);
    assert_eq!(set.cell(0, 1), Some(Some("one")));
    assert_eq!(set.cell(2, 1), Some(None), "NULL comes as NULL");
    assert!(set.kinds()[0].is_numeric(), "{:?}", set.kinds());
    assert_ne!(set.kinds()[1], ColumnKind::Integer);
}

pub fn refuses_writes_twice_when_reading(fixture: &Fixture) {
    let scratch = Scratch::new(fixture, "readonly");
    let reader = open(fixture, Access::ReadOnly);
    let rejected = block_on(reader.fetch_all(
        format!("delete from {}", scratch.name),
        RunOptions::default(),
    ));
    assert!(
        matches!(rejected, Err(QueryError::Rejected(Rejection::Statement(_)))),
        "{rejected:?}"
    );
    // The second layer on its own: the dialect's read-only transaction
    // refuses the change even without the analysis.
    let mut adapter = (fixture.opener)(Access::ReadWrite).unwrap();
    let dialect = adapter.dialect();
    for statement in dialect.transactions.begin_read_only {
        adapter.execute(statement, &mut Discard).unwrap();
    }
    let blocked = adapter.execute(&format!("delete from {}", scratch.name), &mut Discard);
    for statement in dialect.transactions.end_read_only {
        let _ = adapter.execute(statement, &mut Discard);
    }
    drop(adapter);
    assert!(
        blocked.is_err(),
        "the read-only transaction let a DELETE through"
    );
    assert_eq!(scratch.sum(&reader).as_deref(), Some("6"));
    // Read-only sessions refuse every kind of write.
    let refused = drain(reader.run_in_transaction(
        format!("delete from {}", scratch.name),
        RunOptions::default(),
    ));
    assert!(matches!(refused, Err(QueryError::WritesDisabled)));
    let refused = block_on(reader.apply(Vec::new()));
    assert!(matches!(refused, Err(QueryError::WritesDisabled) | Ok(0)));
}

pub fn applies_row_changes_checking_each_one(fixture: &Fixture) {
    let scratch = Scratch::new(fixture, "apply");
    let writer = &scratch.writer;
    let target = EditTarget {
        table: scratch.table(fixture),
        columns: vec!["id".into(), "label".into(), "qty".into()],
        primary_key: vec!["id".into()],
        dialect: writer.dialect(),
    };
    let key = |id: &str| {
        vec![KeyValue {
            column: "id".into(),
            value: id.into(),
        }]
    };
    let update = |id: &str, column: &str, old: Option<&str>, new: &str| {
        edit::build(
            &target,
            &RowChange::Update {
                key: key(id),
                cells: vec![CellChange {
                    column: column.into(),
                    old: old.map(str::to_string),
                    new: Some(new.into()),
                }],
            },
        )
        .unwrap()
    };
    // NULL compares as NULL; a backslash and a quote arrive as typed.
    let text = r"it's a\b";
    assert_eq!(
        block_on(writer.apply(vec![update("3", "label", None, text)])),
        Ok(1)
    );
    let label = scalar(
        writer,
        &format!("select label from {} where id = 3", scratch.name),
    );
    assert_eq!(label.as_deref(), Some(text));
    // `03` is the 3 it already has: still its one row.
    assert_eq!(
        block_on(writer.apply(vec![update("3", "qty", Some("3"), "03")])),
        Ok(1)
    );
    // A stale change matches no row, and nothing of its batch is kept.
    let delete = edit::build(&target, &RowChange::Delete { key: key("1") }).unwrap();
    let stale = update("2", "label", Some("old"), "new");
    let outcome = block_on(writer.apply(vec![delete, stale]));
    assert!(
        matches!(outcome, Err(QueryError::RowMismatch { actual: 0, .. })),
        "{outcome:?}"
    );
    assert_eq!(
        scalar(writer, &format!("select count(*) from {}", scratch.name)).as_deref(),
        Some("3")
    );
}

pub fn keeps_write_transactions_until_commit(fixture: &Fixture) {
    let scratch = Scratch::new(fixture, "transaction");
    let writer = &scratch.writer;
    let other = open(fixture, Access::ReadOnly);
    let name = &scratch.name;
    let summary = drain(writer.run_in_transaction(
        format!("update {name} set qty = qty + 10; delete from {name} where id = 1"),
        RunOptions::default(),
    ))
    .unwrap();
    assert_eq!((summary.affected, summary.statements), (4, 2));
    assert!(writer.in_transaction());
    assert_eq!(scratch.sum(writer).as_deref(), Some("25"));
    assert_eq!(
        scratch.sum(&other).as_deref(),
        Some("6"),
        "uncommitted changes leaked"
    );

    // A failing script undoes only itself.
    let failed = drain(writer.run_in_transaction(
        format!("update {name} set qty = 0; insert into kuery_nowhere values (1)"),
        RunOptions::default(),
    ));
    assert!(
        matches!(failed, Err(QueryError::Database { .. })),
        "{failed:?}"
    );
    assert!(writer.in_transaction());
    assert_eq!(scratch.sum(writer).as_deref(), Some("25"));

    block_on(writer.rollback()).unwrap();
    assert!(!writer.in_transaction());
    assert_eq!(scratch.sum(writer).as_deref(), Some("6"));

    drain(writer.run_in_transaction(format!("delete from {name}"), RunOptions::default())).unwrap();
    block_on(writer.commit()).unwrap();
    assert_eq!(
        scalar(&other, &format!("select count(*) from {name}")).as_deref(),
        Some("0")
    );
}

pub fn keeps_whole_scripts_or_nothing(fixture: &Fixture) {
    let scratch = Scratch::new(fixture, "autocommit");
    let writer = &scratch.writer;
    let name = &scratch.name;
    let failed = drain(writer.run_autocommit(
        format!("delete from {name}; insert into kuery_nowhere values (1)"),
        RunOptions::default(),
    ));
    assert!(failed.is_err());
    assert_eq!(
        scratch.sum(writer).as_deref(),
        Some("6"),
        "half a script was kept"
    );
    drain(writer.run_autocommit(
        format!("delete from {name} where id = 1"),
        RunOptions::default(),
    ))
    .unwrap();
    assert_eq!(scratch.sum(writer).as_deref(), Some("5"));
}

pub fn previews_and_undoes(fixture: &Fixture) {
    let scratch = Scratch::new(fixture, "preview");
    let writer = &scratch.writer;
    let name = &scratch.name;
    let impacts = block_on(writer.preview(
        format!("update {name} set qty = 0 where id < 3; select * from {name}"),
        RunOptions::default(),
    ))
    .unwrap();
    assert_eq!(
        impacts,
        [
            StatementImpact {
                rows: 2,
                returns_rows: false
            },
            StatementImpact {
                rows: 3,
                returns_rows: true
            }
        ]
    );
    assert_eq!(
        scratch.sum(writer).as_deref(),
        Some("6"),
        "the preview was kept"
    );
}

pub fn reads_its_catalog(fixture: &Fixture) {
    let scratch = Scratch::new(fixture, "catalog");
    let reader = open(fixture, Access::ReadOnly);
    let table = scratch.table(fixture);
    let overview = block_on(reader.overview()).unwrap();
    assert!(overview.schemas.contains(&fixture.schema));
    assert!(overview.tables.iter().any(|t| t.table == table));
    let definition = block_on(reader.definition(table.clone())).unwrap();
    assert_eq!(definition.primary_key(), ["id"]);
    assert_eq!(definition.columns.len(), 3);
    let catalog = block_on(reader.completion_catalog()).unwrap();
    let listed = catalog.tables.iter().find(|t| t.table == table).unwrap();
    assert_eq!(listed.columns.len(), 3);
    let diagram = block_on(reader.diagram(fixture.schema.clone())).unwrap();
    assert!(diagram.tables.iter().any(|t| t.table == table));
}

pub fn cancels_a_running_query(fixture: &Fixture) {
    let reader = open(fixture, Access::ReadOnly);
    let started = Instant::now();
    let rx = reader.run_read_only(fixture.slow_query.clone(), RunOptions::default());
    std::thread::sleep(Duration::from_millis(300));
    reader.cancel();
    let outcome = drain(rx);
    assert!(matches!(outcome, Err(QueryError::Cancelled)), "{outcome:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the cancel did not stop it"
    );
    // The session still works.
    assert_eq!(scalar(&reader, "select 1").as_deref(), Some("1"));
}
