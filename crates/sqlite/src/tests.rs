use std::time::Duration;

use futures::executor::block_on;
use kuery_core::adapter::Discard;
use kuery_core::catalog::{ConstraintKind, TableKind, TableRef};
use kuery_core::diagram::OnDelete;
use kuery_core::edit::{self, CellChange, EditTarget, KeyValue, RowChange};
use kuery_core::read_only::Rejection;
use kuery_core::result_set::{ColumnKind, ResultSet};
use kuery_core::session::{Event, Opener, Session, StatementImpact, Summary};
use std::path::PathBuf;

use super::*;

/// A session on the database at `path`, the way the app opens one.
fn session_at(path: PathBuf, access: Access) -> Result<Session, String> {
    let opener: Opener = Arc::new(move |access| open(&path, access));
    block_on(Session::open(opener, access))
}

const SCHEMA: &str = r"
create table customers (
    id integer primary key,
    email text not null unique,
    name text,
    credit real default 0
);
create table orders (
    id integer primary key,
    customer_id integer references customers on delete cascade,
    total numeric not null,
    placed_at datetime
);
create table order_lines (
    order_id integer not null references orders(id),
    line integer not null,
    note text,
    primary key (order_id, line)
);
create index orders_by_customer on orders (customer_id);
create view big_orders as select * from orders where total > 100;
create trigger orders_placed after insert on orders begin
    update orders set placed_at = '2026-01-01' where id = new.id and placed_at is null;
end;
insert into customers (id, email, name, credit) values
    (1, 'ada@example.com', 'Ada', 12.5),
    (2, 'bob@example.com', NULL, 1e20);
insert into orders (id, customer_id, total, placed_at) values
    (10, 1, 250, '2026-03-01 10:00:00'),
    (11, 1, 40, '2026-03-02 11:00:00'),
    (12, 2, 99.9, NULL);
insert into order_lines values (10, 1, 'gift'), (10, 2, NULL);
";

/// A database file of its own, gone when dropped.
struct Database(PathBuf);

impl Database {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kuery-sqlite-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shop.sqlite");
        let _ = std::fs::remove_file(&path);
        Connection::open(&path)
            .unwrap()
            .execute_batch(SCHEMA)
            .unwrap();
        Self(path)
    }

    fn open(&self, access: Access) -> Session {
        session_at(self.0.clone(), access).unwrap()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        if let Some(dir) = self.0.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn fetch(session: &Session, sql: &str) -> Result<ResultSet, QueryError> {
    block_on(session.fetch_all(sql.into(), RunOptions::default()))
}

fn scalar(session: &Session, sql: &str) -> Option<String> {
    let set = fetch(session, sql).unwrap();
    set.cell(0, 0).flatten().map(str::to_string)
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

#[test]
fn opens_only_existing_databases() {
    let missing = session_at("/nowhere/at/all.sqlite".into(), Access::ReadOnly);
    assert!(missing.err().unwrap().contains("There is no database file"));
    let dir = std::env::temp_dir().join(format!("kuery-sqlite-not-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let text = dir.join("notes.txt");
    std::fs::write(&text, "not a database at all, just some words in a file").unwrap();
    let wrong = session_at(text, Access::ReadOnly);
    assert!(wrong.err().unwrap().contains("is not a database"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn reads_rows_with_their_kinds() {
    let database = Database::new("reads");
    let session = database.open(Access::ReadOnly);
    assert!(session.server_label().starts_with("SQLite 3"));
    let set = fetch(
        &session,
        "select c.id, c.name, c.credit, o.total, o.placed_at, x'cafe' as raw \
         from customers c join orders o on o.customer_id = c.id order by o.id",
    )
    .unwrap();
    assert_eq!(
        set.columns(),
        ["id", "name", "credit", "total", "placed_at", "raw"]
    );
    assert_eq!(
        set.kinds(),
        [
            ColumnKind::Integer,
            ColumnKind::Text,
            ColumnKind::Number,
            ColumnKind::Number,
            ColumnKind::Timestamp,
            ColumnKind::Unknown
        ]
    );
    assert_eq!(set.rows(), 3);
    assert_eq!(set.cell(0, 2), Some(Some("12.5")));
    assert_eq!(set.cell(0, 3), Some(Some("250")));
    assert_eq!(set.cell(2, 1), Some(None));
    assert_eq!(set.cell(2, 2), Some(Some("1.0e+20")));
    assert_eq!(set.cell(0, 5), Some(Some("\\xcafe")));
}

#[test]
fn streams_in_growing_batches() {
    let database = Database::new("batches");
    let session = database.open(Access::ReadOnly);
    let rx = session.run_read_only(
        "with recursive n(i) as (select 1 union all select i + 1 from n where i < 5000) \
         select i from n"
            .into(),
        RunOptions {
            first_batch_rows: 100,
            ..RunOptions::default()
        },
    );
    let mut sizes = Vec::new();
    let summary = block_on(async {
        loop {
            match rx.recv().await.unwrap() {
                Event::Batch(batch) => sizes.push(batch.rows()),
                Event::Finished(summary) => break summary,
                Event::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    });
    assert_eq!(summary.rows, 5000);
    assert_eq!(sizes[..3], [100, 400, 1600]);
    assert_eq!(sizes.iter().sum::<usize>(), 5000);
}

#[test]
fn refuses_writes_twice_when_reading() {
    let database = Database::new("readonly");
    let session = database.open(Access::ReadWrite);
    assert!(matches!(
        fetch(&session, "delete from orders"),
        Err(QueryError::Rejected(Rejection::Statement(_)))
    ));
    // The second layer holds on its own: inside the dialect's read-only
    // transaction, query_only refuses the change.
    let mut adapter = open(&database.0, Access::ReadWrite).unwrap();
    for statement in SQLITE.transactions.begin_read_only {
        adapter.execute(statement, &mut Discard).unwrap();
    }
    assert!(matches!(
        adapter.execute("delete from orders", &mut Discard),
        Err(QueryError::Database { .. })
    ));
    drop(adapter);
    assert_eq!(
        scalar(&session, "select count(*) from orders").as_deref(),
        Some("3")
    );
    // Read-only sessions cannot even start a write transaction.
    let reader = database.open(Access::ReadOnly);
    let rx = reader.run_in_transaction("delete from orders".into(), RunOptions::default());
    assert!(matches!(drain(rx), Err(QueryError::WritesDisabled)));
}

#[test]
fn applies_row_changes_with_optimistic_checks() {
    let database = Database::new("apply");
    let session = database.open(Access::ReadWrite);
    let target = EditTarget {
        table: TableRef::new("main", "customers"),
        columns: vec!["id".into(), "email".into(), "name".into(), "credit".into()],
        primary_key: vec!["id".into()],
        dialect: &kuery_core::sql::SQLITE,
    };
    let key = |id: &str| {
        vec![KeyValue {
            column: "id".into(),
            value: id.into(),
        }]
    };
    let update = |old: &str, new: &str| {
        edit::build(
            &target,
            &RowChange::Update {
                key: key("1"),
                cells: vec![
                    CellChange {
                        column: "name".into(),
                        old: Some(old.into()),
                        new: Some(new.into()),
                    },
                    CellChange {
                        column: "credit".into(),
                        old: Some("12.5".into()),
                        new: Some("13".into()),
                    },
                ],
            },
        )
        .unwrap()
    };
    let insert = edit::build(
        &target,
        &RowChange::Insert {
            values: vec![("email".into(), Some("cy@example.com".into()))],
        },
    )
    .unwrap();
    assert_eq!(
        block_on(session.apply(vec![update("Ada", "Ada L."), insert])),
        Ok(2)
    );
    assert_eq!(
        scalar(&session, "select name from customers where id = 1").as_deref(),
        Some("Ada L.")
    );

    // Someone changed the row meanwhile: nothing of the batch is kept.
    let stale = update("Ada", "Ada Lovelace");
    let delete = edit::build(&target, &RowChange::Delete { key: key("2") }).unwrap();
    assert!(matches!(
        block_on(session.apply(vec![delete, stale])),
        Err(QueryError::RowMismatch { actual: 0, .. })
    ));
    assert_eq!(
        scalar(&session, "select count(*) from customers").as_deref(),
        Some("3")
    );
}

#[test]
fn keeps_write_transactions_open_until_commit() {
    let database = Database::new("transaction");
    let writer = database.open(Access::ReadWrite);
    let other = database.open(Access::ReadOnly);
    let summary = drain(writer.run_in_transaction(
        "update orders set total = total + 1; delete from order_lines where line = 2".into(),
        RunOptions::default(),
    ))
    .unwrap();
    assert_eq!((summary.affected, summary.statements), (4, 2));
    assert!(writer.in_transaction());
    // The writer reads its own changes; nobody else sees them yet.
    assert_eq!(
        scalar(&writer, "select total from orders where id = 10").as_deref(),
        Some("251")
    );
    assert_eq!(
        scalar(&other, "select total from orders where id = 10").as_deref(),
        Some("250")
    );

    // A failing script undoes only itself.
    let failed = drain(writer.run_in_transaction(
        "update orders set total = 0; insert into nowhere values (1)".into(),
        RunOptions::default(),
    ));
    assert!(matches!(failed, Err(QueryError::Database { .. })));
    assert!(writer.in_transaction());
    assert_eq!(
        scalar(&writer, "select total from orders where id = 10").as_deref(),
        Some("251")
    );

    block_on(writer.rollback()).unwrap();
    assert!(!writer.in_transaction());
    assert_eq!(
        scalar(&writer, "select total from orders where id = 10").as_deref(),
        Some("250")
    );

    drain(writer.run_in_transaction("delete from order_lines".into(), RunOptions::default()))
        .unwrap();
    block_on(writer.commit()).unwrap();
    assert_eq!(
        scalar(&other, "select count(*) from order_lines").as_deref(),
        Some("0")
    );
}

#[test]
fn autocommit_keeps_whole_scripts_or_nothing() {
    let database = Database::new("autocommit");
    let session = database.open(Access::ReadWrite);
    let failed = drain(session.run_autocommit(
        "delete from order_lines; insert into nowhere values (1)".into(),
        RunOptions::default(),
    ));
    assert!(failed.is_err());
    assert_eq!(
        scalar(&session, "select count(*) from order_lines").as_deref(),
        Some("2")
    );
    drain(session.run_autocommit(
        "delete from order_lines; vacuum".into(),
        RunOptions::default(),
    ))
    .unwrap_err();
    // VACUUM alone runs, outside any transaction.
    drain(session.run_autocommit("vacuum".into(), RunOptions::default())).unwrap();
    // Foreign keys are on: deleting a customer cascades to its orders.
    drain(session.run_autocommit(
        "delete from customers where id = 2".into(),
        RunOptions::default(),
    ))
    .unwrap();
    assert_eq!(
        scalar(&session, "select count(*) from orders").as_deref(),
        Some("2")
    );
}

#[test]
fn previews_count_changes_and_undo_them() {
    let database = Database::new("preview");
    let session = database.open(Access::ReadWrite);
    let impacts = block_on(session.preview(
        "update orders set total = 1 where customer_id = 1; select * from customers".into(),
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
                rows: 2,
                returns_rows: true
            }
        ]
    );
    assert_eq!(
        scalar(&session, "select sum(total) from orders").as_deref(),
        Some("389.9")
    );
}

#[test]
fn stops_statements_that_run_too_long() {
    let database = Database::new("timeout");
    let session = database.open(Access::ReadOnly);
    let endless = "with recursive n(i) as (select 1 union all select i + 1 from n) \
                   select count(*) from n";
    let result = block_on(session.fetch_all(
        endless.into(),
        RunOptions {
            statement_timeout: Duration::from_millis(200),
            ..RunOptions::default()
        },
    ));
    assert!(matches!(result, Err(QueryError::Database { code, .. }) if code == "timeout"));
    // The session still works afterwards.
    assert_eq!(scalar(&session, "select 1").as_deref(), Some("1"));

    let rx = session.run_read_only(endless.into(), RunOptions::default());
    std::thread::sleep(Duration::from_millis(100));
    session.cancel();
    assert!(matches!(drain(rx), Err(QueryError::Cancelled)));
}

#[test]
fn reads_the_catalog() {
    let database = Database::new("catalog");
    let session = database.open(Access::ReadOnly);
    let overview = block_on(session.overview()).unwrap();
    assert_eq!(overview.schemas, ["main"]);
    let names: Vec<(&str, TableKind)> = overview
        .tables
        .iter()
        .map(|t| (t.table.name.as_str(), t.kind))
        .collect();
    assert_eq!(
        names,
        [
            ("big_orders", TableKind::View),
            ("customers", TableKind::Table),
            ("order_lines", TableKind::Table),
            ("orders", TableKind::Table)
        ]
    );
    assert_eq!(overview.triggers.len(), 1);

    let orders = block_on(session.definition(TableRef::new("main", "orders"))).unwrap();
    assert_eq!(orders.primary_key(), ["id"]);
    let customer = orders.column("customer_id").unwrap();
    let reference = customer.foreign_key.as_ref().unwrap();
    // `references customers` names no column: it is the primary key.
    assert_eq!(
        (reference.table.name.as_str(), reference.column.as_str()),
        ("customers", "id")
    );
    assert!(orders.constraints.iter().any(
        |c| c.kind == ConstraintKind::ForeignKey && c.definition.contains("ON DELETE CASCADE")
    ));
    assert!(
        orders
            .indexes
            .iter()
            .any(|i| i.name == "orders_by_customer")
    );
    assert_eq!(orders.triggers[0].name, "orders_placed");

    let lines = block_on(session.definition(TableRef::new("main", "order_lines"))).unwrap();
    assert_eq!(lines.primary_key(), ["order_id", "line"]);
    let customers = block_on(session.definition(TableRef::new("main", "customers"))).unwrap();
    assert!(
        customers
            .constraints
            .iter()
            .any(|c| c.kind == ConstraintKind::Unique)
    );

    let diagram = block_on(session.diagram("main".into())).unwrap();
    assert_eq!(diagram.tables.len(), 3);
    let to_customers = diagram
        .relations
        .iter()
        .find(|r| r.to.name == "customers")
        .unwrap();
    assert_eq!(to_customers.from_columns, ["customer_id"]);
    assert_eq!(to_customers.to_columns, ["id"]);
    assert_eq!(to_customers.on_delete, OnDelete::Cascade);
    assert!(to_customers.optional);

    let completion = block_on(session.completion_catalog()).unwrap();
    let customers = completion
        .tables
        .iter()
        .find(|t| t.table.name == "customers")
        .unwrap();
    assert_eq!(customers.columns.len(), 4);
}

#[test]
fn writes_reals_like_sqlite() {
    assert_eq!(real(1.0), "1.0");
    assert_eq!(real(12.5), "12.5");
    assert_eq!(real(0.1), "0.1");
    assert_eq!(real(1e20), "1.0e+20");
    assert_eq!(real(-2.5e-7), "-2.5e-07");
    assert_eq!(real(123456789.25), "123456789.25");
    assert_eq!(kind_of(Some("VARCHAR(20)")), ColumnKind::Text);
    assert_eq!(kind_of(Some("BIGINT")), ColumnKind::Integer);
    assert_eq!(kind_of(Some("DECIMAL(10,2)")), ColumnKind::Number);
    assert_eq!(kind_of(None), ColumnKind::Unknown);
}

#[test]
fn conforms() {
    let dir = std::env::temp_dir().join(format!("kuery-sqlite-conformance-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("empty.sqlite");
    let _ = std::fs::remove_file(&path);
    Connection::open(&path)
        .unwrap()
        .execute_batch("create table seed (n integer)")
        .unwrap();
    let opener: Opener = Arc::new(move |access| open(&path, access));
    kuery_core::conformance::run(&kuery_core::conformance::Fixture {
        opener,
        schema: "main".into(),
        slow_query: "with recursive n(i) as (select 1 union all select i + 1 from n) \
                     select count(*) from n"
            .into(),
    });
    let _ = std::fs::remove_dir_all(dir);
}
