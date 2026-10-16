//! Need the dev MySQL (`docker compose -f dev/compose.yaml up -d mysql`), or
//! one at `KUERY_MYSQL_HOST`/`KUERY_MYSQL_PORT` with the sample `shop`.

use kuery_core::catalog::{ConstraintKind, RoutineKind, TableKind, TableRef};
use kuery_core::diagram::OnDelete;
use kuery_core::edit::{self, CellChange, EditTarget, KeyValue, RowChange};
use kuery_core::read_only::Rejection;

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

fn session(engine: &Engine, access: Access) -> Session {
    engine
        .runtime
        .block_on(engine.connect(target(), access))
        .expect("connects to the dev MySQL")
}

fn scalar(engine: &Engine, session: &Session, sql: &str) -> Option<String> {
    let set = engine
        .runtime
        .block_on(session.fetch_all(sql.into(), RunOptions::default()))
        .unwrap();
    set.cell(0, 0).flatten().map(str::to_string)
}

fn drain(engine: &Engine, rx: async_channel::Receiver<Event>) -> Result<Summary, QueryError> {
    engine.runtime.block_on(async {
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
fn scratch(engine: &Engine, writer: &Session, name: &str) -> String {
    let table = format!("scratch_{name}_{}", std::process::id());
    drain(
        engine,
        writer.run_autocommit(
            format!(
                "drop table if exists {table}; \
                 create table {table} (id int primary key, name varchar(20), qty int); \
                 insert into {table} values (1, 'one', 1), (2, 'two', 2), (3, NULL, 3)"
            ),
            RunOptions::default(),
        ),
    )
    .unwrap();
    table
}

fn drop_scratch(engine: &Engine, writer: &Session, table: &str) {
    drain(
        engine,
        writer.run_autocommit(format!("drop table {table}"), RunOptions::default()),
    )
    .unwrap();
}

#[test]
#[ignore = "needs the dev MySQL"]
fn reads_rows_as_the_server_writes_them() {
    let engine = Engine::new().unwrap();
    let reader = session(&engine, Access::ReadOnly);
    assert!(!reader.server_version().is_empty());
    let set = engine
        .runtime
        .block_on(
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
    let engine = Engine::new().unwrap();
    let reader = session(&engine, Access::ReadOnly);
    let rejected = engine
        .runtime
        .block_on(reader.fetch_all("delete from orders".into(), RunOptions::default()));
    assert!(matches!(
        rejected,
        Err(QueryError::Rejected(Rejection::Statement(_)))
    ));
    // The second layer on its own: the READ ONLY transaction refuses it.
    let writer = session(&engine, Access::ReadWrite);
    let table = scratch(&engine, &writer, "readonly");
    let blocked = engine
        .runtime
        .block_on(writer.fetch_trusted(format!("delete from {table}")));
    assert!(
        matches!(blocked, Err(QueryError::Database { .. })),
        "{blocked:?}"
    );
    assert_eq!(
        scalar(&engine, &writer, &format!("select count(*) from {table}")).as_deref(),
        Some("3")
    );
    // A read-only session refuses write mode.
    let refused = drain(
        &engine,
        reader.run_in_transaction("delete from orders".into(), RunOptions::default()),
    );
    assert!(matches!(refused, Err(QueryError::WritesDisabled)));
    drop_scratch(&engine, &writer, &table);
}

#[test]
#[ignore = "needs the dev MySQL"]
fn applies_row_changes_with_optimistic_checks() {
    let engine = Engine::new().unwrap();
    let writer = session(&engine, Access::ReadWrite);
    let table = scratch(&engine, &writer, "apply");
    let target = EditTarget {
        table: TableRef::new("shop", table.clone()),
        columns: vec!["id".into(), "name".into(), "qty".into()],
        primary_key: vec!["id".into()],
        dialect: Dialect::Mysql,
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
    assert_eq!(
        engine
            .runtime
            .block_on(writer.apply(vec![change(None, r"a\b")])),
        Ok(1)
    );
    assert_eq!(
        scalar(
            &engine,
            &writer,
            &format!("select name from {table} where id = 3")
        )
        .as_deref(),
        Some(r"a\b")
    );
    // `03` is the 3 it already has: the row matches though nothing changes.
    assert_eq!(
        engine
            .runtime
            .block_on(writer.apply(vec![change_in("qty", Some("3"), "03")])),
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
        engine.runtime.block_on(writer.apply(vec![delete, stale])),
        Err(QueryError::RowMismatch { actual: 0, .. })
    ));
    assert_eq!(
        scalar(&engine, &writer, &format!("select count(*) from {table}")).as_deref(),
        Some("3")
    );
    drop_scratch(&engine, &writer, &table);
}

#[test]
#[ignore = "needs the dev MySQL"]
fn keeps_write_transactions_open_until_commit() {
    let engine = Engine::new().unwrap();
    let writer = session(&engine, Access::ReadWrite);
    let other = session(&engine, Access::ReadOnly);
    let table = scratch(&engine, &writer, "transaction");
    let summary = drain(
        &engine,
        writer.run_in_transaction(
            format!("update {table} set qty = qty + 10; delete from {table} where id = 1"),
            RunOptions::default(),
        ),
    )
    .unwrap();
    assert_eq!((summary.affected, summary.statements), (4, 2));
    assert!(writer.in_transaction());
    let sum = format!("select sum(qty) from {table}");
    assert_eq!(scalar(&engine, &writer, &sum).as_deref(), Some("25"));
    assert_eq!(scalar(&engine, &other, &sum).as_deref(), Some("6"));

    // A failing script undoes only itself.
    let failed = drain(
        &engine,
        writer.run_in_transaction(
            format!("update {table} set qty = 0; insert into nowhere values (1)"),
            RunOptions::default(),
        ),
    );
    assert!(matches!(failed, Err(QueryError::Database { .. })));
    assert!(writer.in_transaction());
    assert_eq!(scalar(&engine, &writer, &sum).as_deref(), Some("25"));

    // DDL would commit by itself: refused here.
    let ddl = drain(
        &engine,
        writer.run_in_transaction(
            format!("alter table {table} add column x int"),
            RunOptions::default(),
        ),
    );
    assert!(matches!(ddl, Err(QueryError::Unsupported(_))), "{ddl:?}");

    engine.runtime.block_on(writer.rollback()).unwrap();
    assert_eq!(scalar(&engine, &writer, &sum).as_deref(), Some("6"));
    drain(
        &engine,
        writer.run_in_transaction(format!("delete from {table}"), RunOptions::default()),
    )
    .unwrap();
    engine.runtime.block_on(writer.commit()).unwrap();
    assert_eq!(
        scalar(&engine, &other, &format!("select count(*) from {table}")).as_deref(),
        Some("0")
    );
    drop_scratch(&engine, &writer, &table);
}

#[test]
#[ignore = "needs the dev MySQL"]
fn previews_count_changes_and_undo_them() {
    let engine = Engine::new().unwrap();
    let writer = session(&engine, Access::ReadWrite);
    let table = scratch(&engine, &writer, "preview");
    let impacts = engine
        .runtime
        .block_on(writer.preview(
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
        scalar(&engine, &writer, &format!("select sum(qty) from {table}")).as_deref(),
        Some("6")
    );
    let ddl = engine
        .runtime
        .block_on(writer.preview(format!("truncate table {table}"), RunOptions::default()));
    assert!(matches!(ddl, Err(QueryError::Unsupported(_))));
    drop_scratch(&engine, &writer, &table);
}

#[test]
#[ignore = "needs the dev MySQL"]
fn cancels_and_times_out_long_queries() {
    let engine = Engine::new().unwrap();
    let reader = session(&engine, Access::ReadOnly);
    let slow = engine.runtime.block_on(reader.fetch_all(
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
    let cancelled = drain(&engine, rx);
    assert!(
        matches!(cancelled, Err(QueryError::Cancelled)),
        "{cancelled:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(scalar(&engine, &reader, "select 1").as_deref(), Some("1"));
}

#[test]
#[ignore = "needs the dev MySQL"]
fn reads_the_catalog() {
    let engine = Engine::new().unwrap();
    let reader = session(&engine, Access::ReadOnly);
    let overview = engine
        .runtime
        .block_on(catalog::overview(reader.clone()))
        .unwrap();
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
    let body = engine
        .runtime
        .block_on(catalog::function_definition(reader.clone(), count.clone()))
        .unwrap();
    assert!(
        body.contains("CREATE") && body.contains("order_count"),
        "{body}"
    );
    assert_eq!(overview.triggers.len(), 1);

    let orders = engine
        .runtime
        .block_on(catalog::definition(
            reader.clone(),
            TableRef::new("shop", "orders"),
        ))
        .unwrap();
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
    let products = engine
        .runtime
        .block_on(catalog::definition(
            reader.clone(),
            TableRef::new("shop", "products"),
        ))
        .unwrap();
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
    let lines = engine
        .runtime
        .block_on(catalog::definition(
            reader.clone(),
            TableRef::new("shop", "order_lines"),
        ))
        .unwrap();
    assert_eq!(lines.primary_key(), ["order_id", "line"]);
    assert_eq!(lines.triggers[0].name, "order_lines_total");

    let diagram = engine
        .runtime
        .block_on(catalog::diagram(reader.clone(), "shop".into()))
        .unwrap();
    let to_orders = diagram
        .relations
        .iter()
        .find(|r| r.name == "lines_order")
        .unwrap();
    assert_eq!(to_orders.on_delete, OnDelete::Cascade);
    assert!(!to_orders.optional);

    let completion = engine
        .runtime
        .block_on(catalog::completion_catalog(reader))
        .unwrap();
    assert_eq!(completion.search_path, ["shop"]);
    assert!(
        completion
            .tables
            .iter()
            .any(|t| t.table.name == "paid_orders" && t.kind == TableKind::View)
    );
}
