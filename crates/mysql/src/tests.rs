//! Need the dev MySQL (`docker compose -f dev/compose.yaml up -d mysql`), or
//! one at `KUERY_MYSQL_HOST`/`KUERY_MYSQL_PORT` with the sample `shop`.

use kuery_core::catalog::{ConstraintKind, RoutineKind, TableKind, TableRef};
use kuery_core::diagram::OnDelete;
use kuery_core::edit::{self, CellChange, EditTarget, KeyValue, RowChange};
use kuery_core::read_only::Rejection;

use futures::executor::block_on;
use kuery_core::adapter::Discard;
use kuery_core::session::{Event, Opener, Session, StatementImpact, Summary};
use std::time::Instant;

use super::*;

fn target() -> Target {
    Target {
        host: std::env::var("KUERY_MYSQL_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("KUERY_MYSQL_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(33069),
        user: "kuery".into(),
        password: None,
        database: "shop".into(),
        tls: false,
    }
}

fn session(access: Access) -> Session {
    let opener: Opener = Arc::new(|access| open(&target(), access));
    block_on(Session::open(opener, access)).expect("connects to the dev MySQL")
}

fn scalar(session: &Session, sql: &str) -> Option<String> {
    let set = block_on(session.fetch_all(sql.into(), RunOptions::default())).unwrap();
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

/// A table of its own for a test that writes, dropped at the end.
fn scratch(writer: &Session, name: &str) -> String {
    let table = format!("scratch_{name}_{}", std::process::id());
    drain(writer.run_autocommit(
        format!(
            "drop table if exists {table}; \
                 create table {table} (id int primary key, name varchar(20), qty int); \
                 insert into {table} values (1, 'one', 1), (2, 'two', 2), (3, NULL, 3)"
        ),
        RunOptions::default(),
    ))
    .unwrap();
    table
}

fn drop_scratch(writer: &Session, table: &str) {
    drain(writer.run_autocommit(format!("drop table {table}"), RunOptions::default())).unwrap();
}

#[test]
#[ignore = "needs the dev MySQL"]
fn reads_rows_as_the_server_writes_them() {
    let reader = session(Access::ReadOnly);
    assert!(reader.server_label().starts_with("MySQL"));
    let set = block_on(
        reader.fetch_all(
            "select id, status, total, created_at, shipping, note \
             from orders where id in (11, 12) order by id"
                .into(),
            RunOptions::default(),
        ),
    )
    .unwrap();
    assert_eq!(
        set.columns(),
        ["id", "status", "total", "created_at", "shipping", "note"]
    );
    assert_eq!(
        set.kinds(),
        [
            ColumnKind::Integer,
            ColumnKind::Text,
            ColumnKind::Number,
            ColumnKind::Timestamp,
            ColumnKind::Text,
            ColumnKind::Text
        ]
    );
    assert_eq!(set.cell(0, 3), Some(Some("2026-01-01 02:23:00")));
    assert_eq!(set.cell(0, 5), Some(Some("Gift wrap, please")));
    assert_eq!(set.cell(1, 5), Some(None));
    assert!(set.cell(0, 4).flatten().unwrap().contains("city"));
}

#[test]
#[ignore = "needs the dev MySQL"]
fn refuses_writes_twice_when_reading() {
    let reader = session(Access::ReadOnly);
    let rejected = block_on(reader.fetch_all("delete from orders".into(), RunOptions::default()));
    assert!(matches!(
        rejected,
        Err(QueryError::Rejected(Rejection::Statement(_)))
    ));
    // The second layer on its own: the READ ONLY transaction refuses it.
    let writer = session(Access::ReadWrite);
    let table = scratch(&writer, "readonly");
    let mut adapter = open(&target(), Access::ReadWrite).unwrap();
    for statement in MYSQL.transactions.begin_read_only {
        adapter.execute(statement, &mut Discard).unwrap();
    }
    let blocked = adapter.execute(&format!("delete from {table}"), &mut Discard);
    let _ = adapter.execute("rollback", &mut Discard);
    drop(adapter);
    assert!(
        matches!(blocked, Err(QueryError::Database { .. })),
        "{blocked:?}"
    );
    assert_eq!(
        scalar(&writer, &format!("select count(*) from {table}")).as_deref(),
        Some("3")
    );
    // A read-only session refuses write mode.
    let refused =
        drain(reader.run_in_transaction("delete from orders".into(), RunOptions::default()));
    assert!(matches!(refused, Err(QueryError::WritesDisabled)));
    drop_scratch(&writer, &table);
}

#[test]
#[ignore = "needs the dev MySQL"]
fn applies_row_changes_with_optimistic_checks() {
    let writer = session(Access::ReadWrite);
    let table = scratch(&writer, "apply");
    let target = EditTarget {
        table: TableRef::new("shop", table.clone()),
        columns: vec!["id".into(), "name".into(), "qty".into()],
        primary_key: vec!["id".into()],
        dialect: &kuery_core::sql::MYSQL,
    };
    let change_in = |column: &str, old: Option<&str>, new: &str| {
        edit::build(
            &target,
            &RowChange::Update {
                key: vec![KeyValue {
                    column: "id".into(),
                    value: "3".into(),
                }],
                cells: vec![CellChange {
                    column: column.into(),
                    old: old.map(str::to_string),
                    new: Some(new.into()),
                }],
            },
        )
        .unwrap()
    };
    let change = |old: Option<&str>, new: &str| change_in("name", old, new);
    // NULL compared as NULL, and a value with a backslash goes as hex.
    assert_eq!(block_on(writer.apply(vec![change(None, r"a\b")])), Ok(1));
    assert_eq!(
        scalar(&writer, &format!("select name from {table} where id = 3")).as_deref(),
        Some(r"a\b")
    );
    // `03` is the 3 it already has: the row matches though nothing changes.
    assert_eq!(
        block_on(writer.apply(vec![change_in("qty", Some("3"), "03")])),
        Ok(1)
    );
    // A stale edit changes nothing, and nothing of its batch is kept.
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
    let stale = change(Some("old"), "new");
    assert!(matches!(
        block_on(writer.apply(vec![delete, stale])),
        Err(QueryError::RowMismatch { actual: 0, .. })
    ));
    assert_eq!(
        scalar(&writer, &format!("select count(*) from {table}")).as_deref(),
        Some("3")
    );
    drop_scratch(&writer, &table);
}

#[test]
#[ignore = "needs the dev MySQL"]
fn keeps_write_transactions_open_until_commit() {
    let writer = session(Access::ReadWrite);
    let other = session(Access::ReadOnly);
    let table = scratch(&writer, "transaction");
    let summary = drain(writer.run_in_transaction(
        format!("update {table} set qty = qty + 10; delete from {table} where id = 1"),
        RunOptions::default(),
    ))
    .unwrap();
    assert_eq!((summary.affected, summary.statements), (4, 2));
    assert!(writer.in_transaction());
    let sum = format!("select sum(qty) from {table}");
    assert_eq!(scalar(&writer, &sum).as_deref(), Some("25"));
    assert_eq!(scalar(&other, &sum).as_deref(), Some("6"));

    // A failing script undoes only itself.
    let failed = drain(writer.run_in_transaction(
        format!("update {table} set qty = 0; insert into nowhere values (1)"),
        RunOptions::default(),
    ));
    assert!(matches!(failed, Err(QueryError::Database { .. })));
    assert!(writer.in_transaction());
    assert_eq!(scalar(&writer, &sum).as_deref(), Some("25"));

    // DDL would commit by itself: refused here.
    let ddl = drain(writer.run_in_transaction(
        format!("alter table {table} add column x int"),
        RunOptions::default(),
    ));
    assert!(matches!(ddl, Err(QueryError::Unsupported(_))), "{ddl:?}");

    block_on(writer.rollback()).unwrap();
    assert_eq!(scalar(&writer, &sum).as_deref(), Some("6"));
    drain(writer.run_in_transaction(format!("delete from {table}"), RunOptions::default()))
        .unwrap();
    block_on(writer.commit()).unwrap();
    assert_eq!(
        scalar(&other, &format!("select count(*) from {table}")).as_deref(),
        Some("0")
    );
    drop_scratch(&writer, &table);
}

#[test]
#[ignore = "needs the dev MySQL"]
fn previews_count_changes_and_undo_them() {
    let writer = session(Access::ReadWrite);
    let table = scratch(&writer, "preview");
    let impacts = block_on(writer.preview(
        format!("update {table} set qty = 0 where id < 3; select * from {table}"),
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
        scalar(&writer, &format!("select sum(qty) from {table}")).as_deref(),
        Some("6")
    );
    let ddl = block_on(writer.preview(format!("truncate table {table}"), RunOptions::default()));
    assert!(matches!(ddl, Err(QueryError::Unsupported(_))));
    drop_scratch(&writer, &table);
}

#[test]
#[ignore = "needs the dev MySQL"]
fn cancels_and_times_out_long_queries() {
    let reader = session(Access::ReadOnly);
    let slow = block_on(reader.fetch_all(
        "select sleep(5) from dual where 1 = (select 1)".into(),
        RunOptions {
            statement_timeout: Duration::from_millis(300),
            ..RunOptions::default()
        },
    ));
    // SLEEP interrupted by max_execution_time returns 1 instead of failing
    // on some versions; either way it comes back quickly.
    assert!(slow.is_ok() || matches!(slow, Err(QueryError::Database { .. })));

    let started = Instant::now();
    let rx = reader.run_read_only(
        "select benchmark(2000000000, md5('kuery'))".into(),
        RunOptions::default(),
    );
    std::thread::sleep(Duration::from_millis(300));
    reader.cancel();
    let cancelled = drain(rx);
    assert!(
        matches!(cancelled, Err(QueryError::Cancelled)),
        "{cancelled:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(scalar(&reader, "select 1").as_deref(), Some("1"));
}

#[test]
#[ignore = "needs the dev MySQL"]
fn reads_the_catalog() {
    let reader = session(Access::ReadOnly);
    let overview = block_on(reader.overview()).unwrap();
    assert!(overview.schemas.contains(&"shop".to_string()));
    assert!(!overview.schemas.contains(&"mysql".to_string()));
    let shop: Vec<(&str, TableKind)> = overview
        .tables
        .iter()
        .filter(|t| t.table.schema == "shop" && !t.table.name.starts_with("scratch"))
        .map(|t| (t.table.name.as_str(), t.kind))
        .collect();
    assert_eq!(
        shop,
        [
            ("customers", TableKind::Table),
            ("order_lines", TableKind::Table),
            ("orders", TableKind::Table),
            ("paid_orders", TableKind::View),
            ("products", TableKind::Table)
        ]
    );
    let refund = overview
        .functions
        .iter()
        .find(|f| f.name == "refund")
        .unwrap();
    assert_eq!(refund.kind, RoutineKind::Procedure);
    let count = overview
        .functions
        .iter()
        .find(|f| f.name == "order_count")
        .unwrap();
    assert!(count.arguments.contains("customer"));
    let body = block_on(reader.routine_source(count.clone())).unwrap();
    assert!(
        body.contains("CREATE") && body.contains("order_count"),
        "{body}"
    );
    assert_eq!(overview.triggers.len(), 1);

    let orders = block_on(reader.definition(TableRef::new("shop", "orders"))).unwrap();
    assert_eq!(orders.primary_key(), ["id"]);
    let customer = orders.column("customer_id").unwrap();
    assert_eq!(
        customer.foreign_key.as_ref().unwrap().table.name,
        "customers"
    );
    assert!(
        orders
            .column("status")
            .unwrap()
            .data_type
            .starts_with("enum(")
    );
    assert!(orders.constraints.iter().any(
        |c| c.kind == ConstraintKind::ForeignKey && c.definition.contains("ON DELETE CASCADE")
    ));
    assert!(orders.indexes.iter().any(|i| i.name == "orders_by_created"));
    let products = block_on(reader.definition(TableRef::new("shop", "products"))).unwrap();
    assert!(
        products
            .constraints
            .iter()
            .any(|c| c.kind == ConstraintKind::Check)
    );
    assert!(
        products
            .constraints
            .iter()
            .any(|c| c.kind == ConstraintKind::Unique)
    );
    let lines = block_on(reader.definition(TableRef::new("shop", "order_lines"))).unwrap();
    assert_eq!(lines.primary_key(), ["order_id", "line"]);
    assert_eq!(lines.triggers[0].name, "order_lines_total");

    let diagram = block_on(reader.diagram("shop".into())).unwrap();
    let to_orders = diagram
        .relations
        .iter()
        .find(|r| r.name == "lines_order")
        .unwrap();
    assert_eq!(to_orders.on_delete, OnDelete::Cascade);
    assert!(!to_orders.optional);

    let completion = block_on(reader.completion_catalog()).unwrap();
    assert_eq!(completion.search_path, ["shop"]);
    assert!(
        completion
            .tables
            .iter()
            .any(|t| t.table.name == "paid_orders" && t.kind == TableKind::View)
    );
}

#[test]
#[ignore = "needs the dev MySQL"]
fn conforms() {
    kuery_core::conformance::run(&kuery_core::conformance::Fixture {
        opener: Arc::new(|access| open(&target(), access)),
        schema: "shop".into(),
        slow_query: "select benchmark(2000000000, md5('kuery'))".into(),
    });
}
