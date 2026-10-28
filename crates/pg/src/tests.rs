//! These need the Postgres from `dev/compose.yaml`:
//! `cargo test -p kuery-pg -- --ignored`
//!
//! The TLS ones also need a Postgres with `ssl = on` at
//! `KUERY_TLS_DATABASE_URL` (without `sslmode`); if it is not set, they do nothing.

use std::future::Future;
use std::time::{Duration, Instant};

use kuery_core::adapter::Discard;
use kuery_core::edit::WriteStatement;
use kuery_core::plan;
use kuery_core::read_only::Rejection;
use kuery_core::session::{Event, Opener, Session, StatementImpact, Summary};

use super::*;

/// The tests' way in: sessions opened the way the app opens them.
struct Engine {
    runtime: &'static Runtime,
}

impl Engine {
    fn new() -> std::io::Result<Self> {
        Ok(Self { runtime: runtime() })
    }

    fn connect(&self, url: String) -> impl Future<Output = Result<Session, String>> + use<> {
        self.connect_with(url, Access::ReadOnly)
    }

    fn connect_with(
        &self,
        url: String,
        access: Access,
    ) -> impl Future<Output = Result<Session, String>> + use<> {
        let opener: Opener = Arc::new(move |access| open(&url, access));
        Session::open(opener, access)
    }
}

#[test]
fn builds_a_quoted_conninfo_with_the_password_in_memory() {
    let mut options = ConnectOptions {
        host: "localhost".into(),
        port: 54329,
        database: "kuery".into(),
        user: "o'brien".into(),
        password: Some(r"p\ss".into()),
        tls: Some("require".into()),
        tunnel: None,
        options: Default::default(),
    };
    assert_eq!(
        conninfo(&options),
        r"host='localhost' port=54329 dbname='kuery' user='o\'brien' application_name=Kuery password='p\\ss' sslmode='require'"
    );
    // Through a tunnel: the host still names the server (for TLS).
    options.password = None;
    options.tunnel = Some(40001);
    let through = conninfo(&options);
    assert!(through.starts_with("host='localhost' hostaddr=127.0.0.1 port=40001 "));
    assert!(!through.contains("password"));
}

fn database_url() -> String {
    std::env::var("KUERY_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://kuery:kuery@localhost:54329/kuery".into())
}

async fn session(engine: &Engine) -> Session {
    engine
        .connect(database_url())
        .await
        .expect("connects to the dev Postgres")
}

async fn collect(rx: async_channel::Receiver<Event>) -> (Vec<String>, usize, usize, Event) {
    let (mut columns, mut rows, mut batches) = (Vec::new(), 0, 0);
    loop {
        match rx
            .recv()
            .await
            .expect("the driver always ends with a final event")
        {
            Event::Columns(names) => columns = names,
            Event::Types(_) => {}
            Event::Batch(batch) => {
                rows += batch.rows();
                batches += 1;
            }
            last => return (columns, rows, batches, last),
        }
    }
}

/// Runs without the upfront analysis, to test the second layer alone: in
/// the dialect's read-only transaction, on a connection of its own.
async fn run_unchecked(_: &Session, sql: &str) -> Result<u64, QueryError> {
    let sql = sql.to_string();
    std::thread::spawn(move || {
        let mut adapter = open_postgres(&database_url(), Access::ReadWrite)
            .map_err(|error| QueryError::Connection(error.to_string()))?;
        for statement in POSTGRES.transactions.begin_read_only {
            adapter.execute(statement, &mut Discard)?;
        }
        let outcome = adapter.execute(&sql, &mut Discard);
        let _ = adapter.execute("rollback", &mut Discard);
        outcome
    })
    .join()
    .unwrap()
}

async fn order_count(session: &Session) -> usize {
    scalar(session, "select count(*) from orders")
        .await
        .parse()
        .unwrap()
}

async fn scalar(session: &Session, sql: &str) -> String {
    let rx = session.run_read_only(sql.into(), RunOptions::default());
    let mut set = None;
    while let Ok(event) = rx.recv().await {
        match event {
            Event::Columns(names) => set = Some(kuery_core::result_set::ResultSet::new(names)),
            Event::Types(kinds) => set.as_mut().unwrap().set_kinds(kinds),
            Event::Batch(batch) => set.as_mut().unwrap().push(batch),
            _ => {}
        }
    }
    set.unwrap().cell(0, 0).unwrap().unwrap().to_owned()
}

fn tls_database_url(sslmode: &str) -> Option<String> {
    match std::env::var("KUERY_TLS_DATABASE_URL") {
        Ok(url) => {
            let separator = if url.contains('?') { '&' } else { '?' };
            Some(format!("{url}{separator}sslmode={sslmode}"))
        }
        Err(_) => {
            eprintln!("KUERY_TLS_DATABASE_URL is not set: skipping the test");
            None
        }
    }
}

const USES_TLS: &str = "select ssl from pg_stat_ssl where pid = pg_backend_pid()";

#[test]
#[ignore = "needs a Postgres with TLS"]
fn sslmode_require_encrypts_the_connection() {
    let Some(url) = tls_database_url("require") else {
        return;
    };
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let session = engine.connect(url).await.expect("connects over TLS");
        assert_eq!(scalar(&session, USES_TLS).await, "t");
    });
}

#[test]
#[ignore = "needs a Postgres with TLS"]
fn sslmode_disable_skips_tls() {
    let Some(url) = tls_database_url("disable") else {
        return;
    };
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let session = engine.connect(url).await.expect("connects without TLS");
        assert_eq!(scalar(&session, USES_TLS).await, "f");
    });
}

#[test]
#[ignore = "needs a Postgres with TLS"]
fn verify_full_rejects_an_untrusted_certificate() {
    // The test server's certificate is self-signed.
    let Some(url) = tls_database_url("verify-full") else {
        return;
    };
    let engine = Engine::new().unwrap();
    let result = engine.runtime.block_on(engine.connect(url));
    let Err(error) = result else {
        panic!("should not accept a self-signed certificate");
    };
    assert!(error.to_string().contains("certificate"), "{error}");
}

#[test]
#[ignore = "needs a Postgres with TLS"]
fn cancels_over_tls() {
    let Some(url) = tls_database_url("require") else {
        return;
    };
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let session = engine.connect(url).await.expect("connects over TLS");
        let rx = session.run_read_only("select pg_sleep(10)".into(), RunOptions::default());
        let started = Instant::now();
        tokio::time::sleep(Duration::from_millis(200)).await;
        session.cancel();
        let (_, _, _, last) = collect(rx).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            matches!(&last, Event::Failed(QueryError::Cancelled)),
            "{last:?}"
        );
        // Cancelling over TLS leaves the connection usable, just as without TLS.
        assert_eq!(scalar(&session, "select 1").await, "1");
        assert!(started.elapsed() < Duration::from_secs(5));
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn streams_a_million_rows_in_growing_batches() {
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let session = session(&engine).await;
        let rx = session.run_read_only("select * from orders".into(), RunOptions::default());
        let (columns, rows, batches, last) = collect(rx).await;
        assert_eq!(columns.first().map(String::as_str), Some("id"));
        assert_eq!(rows, 1_000_000);
        assert!(batches > 10, "{batches} batches");
        let Event::Finished(summary) = last else {
            panic!("{last:?}")
        };
        assert_eq!(summary.rows, 1_000_000);
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn rejects_writes_before_reaching_the_server() {
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let session = session(&engine).await;
        let rx = session.run_read_only("delete from orders".into(), RunOptions::default());
        let (_, _, _, last) = collect(rx).await;
        assert!(matches!(
            last,
            Event::Failed(QueryError::Rejected(Rejection::Statement(_)))
        ));
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn the_transaction_blocks_writes_even_without_the_parser() {
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let session = session(&engine).await;
        let before = order_count(&session).await;

        let error = run_unchecked(&session, "delete from orders where id <= 10")
            .await
            .unwrap_err();
        assert!(
            matches!(&error, QueryError::Database { code, .. } if code == "25006"),
            "{error:?}"
        );

        // Nor can it switch back to read-write from inside.
        let error = run_unchecked(
            &session,
            "select set_config('transaction_read_only', 'off', true)",
        )
        .await
        .unwrap_err();
        assert!(matches!(error, QueryError::Database { .. }), "{error:?}");

        assert_eq!(order_count(&session).await, before);
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn statement_timeout_stops_long_queries() {
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let session = session(&engine).await;
        let options = RunOptions {
            statement_timeout: Duration::from_millis(200),
            ..RunOptions::default()
        };
        let rx = session.run_read_only("select pg_sleep(5)".into(), options);
        let started = Instant::now();
        let (_, _, _, last) = collect(rx).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            matches!(&last, Event::Failed(QueryError::Database { code, .. }) if code == "57014"),
            "{last:?}"
        );
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn dropping_the_receiver_cancels_the_query() {
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let session = session(&engine).await;
        let rx = session.run_read_only("select * from orders".into(), RunOptions::default());
        // Read one batch and give up.
        loop {
            if let Event::Batch(_) = rx.recv().await.unwrap() {
                break;
            }
        }
        drop(rx);
        // The session is free for the next query.
        let started = Instant::now();
        assert_eq!(order_count(&session).await, 1_000_000);
        assert!(started.elapsed() < Duration::from_secs(5));
    });
}

async fn writable_session(engine: &Engine) -> Session {
    engine
        .connect_with(database_url(), Access::ReadWrite)
        .await
        .expect("connects to the dev Postgres")
}

fn playground() -> kuery_core::edit::EditTarget {
    kuery_core::edit::EditTarget {
        table: kuery_core::catalog::TableRef::new("public", "playground_items"),
        columns: vec!["id".into(), "name".into(), "quantity".into(), "note".into()],
        primary_key: vec!["id".into()],
        binary_columns: Vec::new(),
        dialect: &kuery_core::sql::POSTGRES,
    }
}

fn statement(change: kuery_core::edit::RowChange) -> WriteStatement {
    kuery_core::edit::build(&playground(), &change).unwrap()
}

fn key(id: &str) -> Vec<kuery_core::edit::KeyValue> {
    vec![kuery_core::edit::KeyValue {
        column: "id".into(),
        value: id.into(),
    }]
}

#[test]
#[ignore = "needs the dev Postgres"]
fn text_that_is_not_utf8_fails_the_query_and_keeps_the_connection() {
    let mut adapter = open_postgres(&database_url(), Access::ReadOnly).unwrap();
    // In Latin-1, chr(233) is the byte 0xE9: not UTF-8, as text from a
    // SQL_ASCII database can be.
    adapter
        .execute("set client_encoding = 'LATIN1'", &mut Discard)
        .unwrap();
    let error = adapter
        .execute("select chr(233) as name", &mut Discard)
        .unwrap_err();
    assert!(
        matches!(&error, QueryError::Database { code, message }
            if code.is_empty() && message.contains("\"name\"")),
        "{error:?}"
    );
    adapter
        .execute("set client_encoding = 'UTF8'", &mut Discard)
        .unwrap();
    assert_eq!(adapter.execute("select 1", &mut Discard), Ok(0));
}

#[test]
#[ignore = "needs the dev Postgres"]
fn reads_a_schema_for_its_diagram() {
    use kuery_core::catalog::TableRef;
    use kuery_core::diagram::OnDelete;
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let session = session(&engine).await;
        let diagram = session.diagram("public".into()).await.unwrap();
        let orders = diagram
            .tables
            .iter()
            .find(|t| t.table.name == "orders")
            .unwrap();
        let id = orders.columns.iter().find(|c| c.name == "id").unwrap();
        assert!(id.primary_key && !id.nullable);
        let customer = orders
            .columns
            .iter()
            .find(|c| c.name == "customer_id")
            .unwrap();
        assert!(customer.foreign_key && !customer.primary_key);
        assert!(orders.rows.is_some_and(|rows| rows > 0));
        let to_customers = diagram
            .relations
            .iter()
            .find(|r| r.name == "orders_customer_id_fkey")
            .unwrap();
        assert_eq!(to_customers.to, TableRef::new("public", "customers"));
        assert_eq!(to_customers.from_columns, ["customer_id"]);
        assert_eq!(to_customers.to_columns, ["id"]);
        assert!(!to_customers.optional);
        let profiles = diagram
            .relations
            .iter()
            .find(|r| r.from.name == "profiles")
            .unwrap();
        assert_eq!(profiles.on_delete, OnDelete::Cascade);
        // Views have no card.
        assert!(
            !diagram
                .tables
                .iter()
                .any(|t| t.table.name == "v_daily_sales")
        );
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn reads_tables_and_their_definition_from_the_catalog() {
    use kuery_core::catalog::{ConstraintKind, TableKind, TableRef};
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let session = session(&engine).await;
        let tables = session.overview().await.map(|o| o.tables).unwrap();
        let find = |name: &str| tables.iter().find(|t| t.table.name == name).unwrap();
        assert_eq!(find("orders").kind, TableKind::Table);
        assert_eq!(find("v_daily_sales").kind, TableKind::View);
        assert_eq!(
            find("profiles").comment.as_deref(),
            Some("Profile and roles of each customer")
        );

        let orders = session
            .clone()
            .definition(TableRef::new("public", "orders"))
            .await
            .unwrap();
        assert_eq!(orders.primary_key(), vec!["id".to_string()]);
        let id = orders.column("id").unwrap();
        assert!(id.generated && !id.nullable);
        let customer = orders.column("customer_id").unwrap();
        let fk = customer.foreign_key.as_ref().unwrap();
        assert_eq!(
            (fk.table.name.as_str(), fk.column.as_str()),
            ("customers", "id")
        );
        assert_eq!(orders.column("total").unwrap().data_type, "numeric(10,2)");
        assert_eq!(
            orders.column("status").unwrap().default.as_deref(),
            Some("'pending'::order_status")
        );
        assert!(
            orders
                .indexes
                .iter()
                .any(|i| i.primary && i.name == "orders_pkey")
        );
        assert!(
            orders
                .constraints
                .iter()
                .any(|c| c.kind == ConstraintKind::Check)
        );
        assert_eq!(orders.triggers.len(), 1);

        let profiles = session
            .definition(TableRef::new("public", "profiles"))
            .await
            .unwrap();
        assert_eq!(profiles.column("roles").unwrap().data_type, "user_role[]");
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn read_only_connections_refuse_to_apply_changes() {
    use kuery_core::edit::RowChange;
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let session = session(&engine).await;
        assert_eq!(session.access(), Access::ReadOnly);
        let error = session
            .apply(vec![statement(RowChange::Delete { key: key("1") })])
            .await
            .unwrap_err();
        assert_eq!(error, QueryError::WritesDisabled);
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn applies_row_changes_atomically_with_optimistic_checks() {
    use kuery_core::edit::{CellChange, RowChange};
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let session = writable_session(&engine).await;
        let name = format!("test-{}", std::process::id());

        session
            .apply(vec![statement(RowChange::Insert {
                values: vec![("name".into(), Some(name.clone())), ("quantity".into(), Some("3".into()))],
            })])
            .await
            .unwrap();
        let id = scalar(
            &session,
            &format!("select id::text from playground_items where name = '{name}'"),
        )
        .await;

        // A change with the correct original value: it is applied.
        session
            .apply(vec![statement(RowChange::Update {
                key: key(&id),
                cells: vec![CellChange {
                    column: "quantity".into(),
                    old: Some("3".into()),
                    new: Some("4".into()),
                }],
            })])
            .await
            .unwrap();

        // With a stale original it does not touch the row, and it also
        // rolls back the first change in the same batch.
        let error = session
            .apply(vec![
                statement(RowChange::Update {
                    key: key(&id),
                    cells: vec![CellChange {
                        column: "note".into(),
                        old: None,
                        new: Some("should not persist".into()),
                    }],
                }),
                statement(RowChange::Update {
                    key: key(&id),
                    cells: vec![CellChange {
                        column: "quantity".into(),
                        old: Some("3".into()),
                        new: Some("5".into()),
                    }],
                }),
            ])
            .await
            .unwrap_err();
        assert!(matches!(error, QueryError::RowMismatch { expected: 1, actual: 0, .. }), "{error:?}");
        let row = scalar(
            &session,
            &format!("select quantity || '/' || coalesce(note, 'NULL') from playground_items where id = {id}"),
        )
        .await;
        assert_eq!(row, "4/NULL");

        session
            .apply(vec![statement(RowChange::Delete { key: key(&id) })])
            .await
            .unwrap();
        let left = scalar(&session, &format!("select count(*)::text from playground_items where id = {id}")).await;
        assert_eq!(left, "0");
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn lists_schemas_functions_enums_and_triggers() {
    use kuery_core::catalog::RoutineKind;
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let session = session(&engine).await;
        let overview = session.clone().overview().await.unwrap();
        assert!(overview.schemas.iter().any(|s| s == "public"));
        assert!(overview.tables.iter().any(|t| t.table.name == "orders"));

        let trigger_fn = overview
            .functions
            .iter()
            .find(|f| f.name == "set_updated_at")
            .unwrap();
        assert_eq!(trigger_fn.kind, RoutineKind::Function);
        assert_eq!(trigger_fn.result, "trigger");
        assert_eq!(trigger_fn.language, "plpgsql");

        let status = overview
            .enums
            .iter()
            .find(|e| e.name == "order_status")
            .unwrap();
        assert_eq!(status.values, ["pending", "paid", "refunded", "cancelled"]);

        assert!(
            overview
                .triggers
                .iter()
                .any(|t| t.name == "orders_set_updated_at" && t.table.name == "orders")
        );

        let definition = session.routine_source(trigger_fn.clone()).await.unwrap();
        assert!(
            definition.starts_with("CREATE OR REPLACE FUNCTION public.set_updated_at()"),
            "{definition}"
        );
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn loads_tables_and_columns_for_completion() {
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let session = session(&engine).await;
        let catalog = session.completion_catalog().await.unwrap();
        assert_eq!(catalog.search_path, ["public"]);
        assert!(catalog.schemas.iter().any(|s| s == "public"));
        let orders = catalog
            .tables
            .iter()
            .find(|t| t.table.schema == "public" && t.table.name == "orders")
            .unwrap();
        assert_eq!(orders.columns[0].name, "id");
        assert!(
            orders
                .columns
                .iter()
                .any(|c| c.name == "status" && c.data_type == "order_status")
        );
        let view = catalog
            .tables
            .iter()
            .find(|t| t.table.name == "v_daily_sales")
            .unwrap();
        assert_eq!(view.kind, kuery_core::catalog::TableKind::View);
        assert!(!view.columns.is_empty());
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn reads_a_second_database_on_the_same_server() {
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let url = format!("{}/analytics", database_url().rsplit_once('/').unwrap().0);
        let session = engine.connect(url).await.unwrap();
        let overview = session.overview().await.unwrap();
        assert!(overview.schemas.iter().any(|s| s == "events"));
        assert!(
            overview
                .tables
                .iter()
                .any(|t| t.table.schema == "events" && t.table.name == "page_views")
        );
        assert!(
            overview
                .functions
                .iter()
                .any(|f| f.name == "conversion_rate" && f.arguments == "p_day date")
        );
    });
}

async fn finish(rx: async_channel::Receiver<Event>) -> Result<Summary, QueryError> {
    match collect(rx).await.3 {
        Event::Finished(summary) => Ok(summary),
        Event::Failed(error) => Err(error),
        other => panic!("unexpected final event {other:?}"),
    }
}

async fn write_session(engine: &Engine) -> Session {
    engine
        .connect_with(database_url(), Access::ReadWrite)
        .await
        .expect("connects to the dev Postgres")
}

/// A table only this test uses, so tests can run side by side.
fn scratch_table(test: &str) -> String {
    format!("kuery_write_{test}_{}", std::process::id())
}

#[test]
#[ignore = "needs the dev Postgres"]
fn write_transactions_stay_open_until_commit() {
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let writer = write_session(&engine).await;
        let other = session(&engine).await;
        let table = scratch_table("commit");
        let options = RunOptions::default();

        let summary = finish(writer.run_in_transaction(
            format!("create table {table} (n int); insert into {table} values (1), (2)"),
            options.clone(),
        ))
        .await
        .unwrap();
        assert_eq!((summary.affected, summary.statements), (2, 2));
        assert!(writer.in_transaction());

        // The writer sees its own changes; nobody else does yet.
        assert_eq!(
            scalar(&writer, &format!("select count(*) from {table}")).await,
            "2"
        );
        let hidden =
            finish(other.run_read_only(format!("select count(*) from {table}"), options.clone()))
                .await;
        assert!(matches!(hidden, Err(QueryError::Database { code, .. }) if code == "42P01"));

        writer.commit().await.unwrap();
        assert!(!writer.in_transaction());
        assert_eq!(
            scalar(&other, &format!("select count(*) from {table}")).await,
            "2"
        );

        finish(writer.run_in_transaction(format!("drop table {table}"), options))
            .await
            .unwrap();
        writer.commit().await.unwrap();
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn sees_who_blocks_whom_and_ends_the_blocker() {
    use kuery_core::activity::Signal;
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let holder = write_session(&engine).await;
        let waiter = write_session(&engine).await;
        let monitor = session(&engine).await;
        let table = scratch_table("locks");
        let options = RunOptions::default();
        let setup = write_session(&engine).await;
        finish(setup.run_in_transaction(
            format!("create table {table} (n int); insert into {table} values (1)"),
            options.clone(),
        ))
        .await
        .unwrap();
        setup.commit().await.unwrap();

        // The holder keeps the row locked in an open transaction; the
        // waiter's update waits for it.
        finish(holder.run_in_transaction(format!("update {table} set n = 2"), options.clone()))
            .await
            .unwrap();
        let waiting =
            waiter.run_in_transaction(format!("update {table} set n = 3"), options.clone());
        let mut seen = None;
        for _ in 0..50 {
            let now = monitor.activity().await.unwrap();
            // Other sessions on the server may block each other too.
            let ours = now.blockers().into_iter().find(|root| {
                root.waiting
                    .iter()
                    .any(|pid| now.backend(*pid).is_some_and(|b| b.query.contains(&table)))
            });
            if let Some(root) = ours {
                seen = Some((now, root));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let (now, root) = seen.expect("sees the waiter blocked");
        assert!(now.max_connections.is_some_and(|max| max > 0));
        assert!(now.backends.iter().any(|b| b.own));
        let blocked = now.backend(root.waiting[0]).unwrap();
        assert_eq!(blocked.blocked_by, [root.pid]);
        assert!(
            blocked
                .waiting_for
                .as_deref()
                .is_some_and(|w| w.contains("transactionid") || w.contains(&table)),
            "{:?}",
            blocked.waiting_for
        );
        assert!(blocked.query.contains("set n = 3"));

        // Ending the holder lets the waiter through.
        assert!(monitor.signal(root.pid, Signal::Terminate).await.unwrap());
        finish(waiting).await.unwrap();
        waiter.rollback().await.unwrap();
        assert!(!monitor.signal(999_999, Signal::Cancel).await.unwrap());

        finish(setup.run_in_transaction(format!("drop table {table}"), options))
            .await
            .unwrap();
        setup.commit().await.unwrap();
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn rollback_undoes_and_a_failing_script_only_undoes_itself() {
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let writer = write_session(&engine).await;
        let before = order_count(&writer).await;
        let options = RunOptions::default();

        finish(writer.run_in_transaction(
            "delete from orders where id in (select id from orders limit 3)".into(),
            options.clone(),
        ))
        .await
        .unwrap();
        assert_eq!(order_count(&writer).await, before - 3);

        // This script fails halfway: its first delete is undone, the
        // earlier one stays, and the transaction is still usable.
        let failed = finish(writer.run_in_transaction(
            "delete from orders where id in (select id from orders limit 5); select 1/0".into(),
            options.clone(),
        ))
        .await;
        assert!(matches!(failed, Err(QueryError::Database { code, .. }) if code == "22012"));
        assert!(writer.in_transaction());
        assert_eq!(order_count(&writer).await, before - 3);

        writer.rollback().await.unwrap();
        assert!(!writer.in_transaction());
        assert_eq!(order_count(&writer).await, before);
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn read_only_sessions_cannot_open_a_write_transaction() {
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let reader = session(&engine).await;
        let result =
            finish(reader.run_in_transaction("delete from orders".into(), RunOptions::default()))
                .await;
        assert_eq!(result.err(), Some(QueryError::WritesDisabled));
        assert!(!reader.in_transaction());
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn autocommit_keeps_whole_scripts_or_nothing() {
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let writer = write_session(&engine).await;
        let other = session(&engine).await;
        let table = scratch_table("autocommit");
        let options = RunOptions::default();

        let summary = finish(writer.run_autocommit(
            format!("create table {table} (n int); insert into {table} values (1), (2)"),
            options.clone(),
        ))
        .await
        .unwrap();
        assert_eq!(summary.affected, 2);
        assert!(!writer.in_transaction());
        // Kept right away: another session sees it.
        assert_eq!(
            scalar(&other, &format!("select count(*) from {table}")).await,
            "2"
        );

        // A failing statement undoes the whole script.
        let failed = finish(writer.run_autocommit(
            format!("insert into {table} values (3); select 1/0"),
            options.clone(),
        ))
        .await;
        assert!(matches!(failed, Err(QueryError::Database { code, .. }) if code == "22012"));
        assert_eq!(
            scalar(&other, &format!("select count(*) from {table}")).await,
            "2"
        );

        // Statements that cannot run in a transaction block work alone.
        finish(writer.run_autocommit(format!("vacuum {table}"), options.clone()))
            .await
            .unwrap();

        // Not while a transaction is open on the session.
        finish(writer.run_in_transaction(format!("delete from {table}"), options.clone()))
            .await
            .unwrap();
        let refused =
            finish(writer.run_autocommit(format!("drop table {table}"), options.clone())).await;
        assert_eq!(refused.err(), Some(QueryError::TransactionOpen));
        writer.rollback().await.unwrap();

        finish(writer.run_autocommit(format!("drop table {table}"), options))
            .await
            .unwrap();
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn explains_with_estimates_and_measurements() {
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let reader = session(&engine).await;
        let options = RunOptions::default();
        let estimate = reader
            .explain(
                "select * from orders where status = 'paid'".into(),
                false,
                options.clone(),
            )
            .await
            .unwrap();
        let plan = plan::parse(&estimate).unwrap();
        assert!(!plan.analyzed);
        assert!(
            plan.root
                .walk()
                .iter()
                .any(|n| n.relation.as_deref() == Some("orders"))
        );

        let measured = reader
            .explain("select count(*) from orders".into(), true, options.clone())
            .await
            .unwrap();
        let plan = plan::parse(&measured).unwrap();
        assert!(plan.analyzed && plan.execution_ms.is_some());

        // An estimate of a write is safe, even read-only; measuring it is not.
        let before = order_count(&reader).await;
        reader
            .explain("delete from orders".into(), false, options.clone())
            .await
            .unwrap();
        assert_eq!(order_count(&reader).await, before);
        let refused = reader
            .explain("delete from orders".into(), true, options.clone())
            .await;
        assert!(matches!(refused, Err(QueryError::Rejected(_))));
        assert!(matches!(
            reader
                .explain("select 1; select 2".into(), false, options)
                .await,
            Err(QueryError::Explain(_))
        ));
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn measures_writes_and_undoes_them() {
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let writer = write_session(&engine).await;
        let before = order_count(&writer).await;
        let measured = writer
            .explain_and_undo(
                "delete from orders where id in (select id from orders limit 5)".into(),
                RunOptions::default(),
            )
            .await
            .unwrap();
        let plan = plan::parse(&measured).unwrap();
        assert_eq!(plan.root.node_type, "ModifyTable");
        assert_eq!(plan.root.title, "Delete from orders");
        assert!(!writer.in_transaction());
        assert_eq!(order_count(&writer).await, before);
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn previews_count_changes_and_undo_them() {
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let writer = write_session(&engine).await;
        let before = order_count(&writer).await;
        let options = RunOptions::default();

        // Outside a transaction: counted, then rolled back.
        let impacts = writer
            .preview(
                "delete from orders where id in (select id from orders limit 3); \
                 select * from orders limit 2"
                    .into(),
                options.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            impacts,
            [
                StatementImpact {
                    rows: 3,
                    returns_rows: false
                },
                StatementImpact {
                    rows: 2,
                    returns_rows: true
                },
            ]
        );
        assert!(!writer.in_transaction());
        assert_eq!(order_count(&writer).await, before);

        // Inside an open transaction: earlier changes stay, the preview goes.
        finish(writer.run_in_transaction(
            "delete from orders where id in (select id from orders limit 1)".into(),
            options.clone(),
        ))
        .await
        .unwrap();
        writer
            .preview(
                "delete from orders where id in (select id from orders limit 5)".into(),
                options.clone(),
            )
            .await
            .unwrap();
        assert!(writer.in_transaction());
        assert_eq!(order_count(&writer).await, before - 1);

        // A failing script is undone too, and reports the error.
        let failed = writer
            .preview("delete from orders; select 1/0".into(), options)
            .await;
        assert!(matches!(failed, Err(QueryError::Database { code, .. }) if code == "22012"));
        assert!(writer.in_transaction());
        assert_eq!(order_count(&writer).await, before - 1);
        writer.rollback().await.unwrap();
        assert_eq!(order_count(&writer).await, before);
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn reads_report_column_kinds() {
    let engine = Engine::new().unwrap();
    engine.runtime.block_on(async {
        let session = session(&engine).await;
        let set = session
            .fetch_all(
                "select id, total, status::text, created_at, created_at::date, true \
                 from orders limit 3"
                    .into(),
                RunOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            set.kinds(),
            [
                ColumnKind::Integer,
                ColumnKind::Number,
                ColumnKind::Text,
                ColumnKind::Timestamp,
                ColumnKind::Date,
                ColumnKind::Boolean,
            ]
        );
        // A statement that cannot be prepared still runs (and reports
        // its own error).
        let failed = session
            .fetch_all("select * from no_such_table".into(), RunOptions::default())
            .await;
        assert!(matches!(failed, Err(QueryError::Database { code, .. }) if code == "42P01"));
    });
}

#[test]
#[ignore = "needs the dev Postgres"]
fn conforms() {
    let url = database_url();
    kuery_core::conformance::run(&kuery_core::conformance::Fixture {
        opener: Arc::new(move |access| open(&url, access)),
        schema: "public".into(),
        slow_query: "select pg_sleep(30)".into(),
    });
}
