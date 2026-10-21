//! Need the dev SQL Server (`docker compose -f dev/compose.yaml up -d mssql
//! mssql-seed`), or one at `KUERY_MSSQL_HOST`/`KUERY_MSSQL_PORT` (password
//! `KUERY_MSSQL_PASSWORD` for `sa`) with the sample `shop`.

use std::time::Instant;

use futures::executor::block_on;
use kuery_core::adapter::{Discard, fetch};
use kuery_core::session::{Opener, Session};

use super::*;

fn target() -> Target {
    Target {
        host: std::env::var("KUERY_MSSQL_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("KUERY_MSSQL_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(14339),
        instance: None,
        user: "sa".into(),
        password: Some(
            std::env::var("KUERY_MSSQL_PASSWORD").unwrap_or_else(|_| "Kuery_dev_2026!".into()),
        ),
        database: "shop".into(),
        tls: TlsMode::TrustServerCertificate,
    }
}

fn adapter() -> Box<dyn Adapter> {
    open(&target(), Access::ReadWrite).expect("connects to the dev SQL Server")
}

fn session(target: Target, access: Access) -> Session {
    let opener: Opener = Arc::new(move |access| open(&target, access));
    block_on(Session::open(opener, access)).expect("connects to the dev SQL Server")
}

fn scalar(adapter: &mut dyn Adapter, sql: &str) -> Option<String> {
    fetch(adapter, sql).unwrap().get(0, 0).map(str::to_string)
}

fn run(adapter: &mut dyn Adapter, sql: &str) {
    adapter.execute(sql, &mut Discard).unwrap();
}

/// A table of its own for a test that writes; `drop_scratch` drops it.
fn scratch(name: &str) -> String {
    let table = format!("dbo.scratch_{name}_{}", std::process::id());
    run(
        adapter().as_mut(),
        &format!(
            "drop table if exists {table}; \
             create table {table} (id int primary key, name nvarchar(20), qty int); \
             insert into {table} values (1, N'one', 1), (2, N'two', 2), (3, null, 3)"
        ),
    );
    table
}

fn drop_scratch(table: &str) {
    run(adapter().as_mut(), &format!("drop table {table}"));
}

/// One result set: its columns and rows.
type Streamed = (Vec<String>, Vec<Vec<Option<String>>>);

/// Every result set a statement streams, in order.
#[derive(Default)]
struct Seen(Vec<Streamed>);

impl RowSink for Seen {
    fn wants_kinds(&self) -> bool {
        true
    }

    fn columns(
        &mut self,
        names: Vec<String>,
        _: Option<Vec<ColumnKind>>,
    ) -> Result<(), QueryError> {
        self.0.push((names, Vec::new()));
        Ok(())
    }

    fn row(&mut self, values: &[Option<&str>]) -> Result<(), QueryError> {
        let row = values.iter().map(|v| v.map(str::to_string)).collect();
        self.0.last_mut().unwrap().1.push(row);
        Ok(())
    }
}

/// Stops what `adapter` runs after `after`.
fn cancel_after(adapter: &dyn Adapter, after: Duration) {
    let cancel = adapter.canceller();
    std::thread::spawn(move || {
        std::thread::sleep(after);
        cancel();
    });
}

#[test]
fn reads_targets_from_connection_options() {
    let options = ConnectOptions {
        host: r"db.example.com\SQLEXPRESS".into(),
        port: 1433,
        database: " shop ".into(),
        user: " sa ".into(),
        password: Some(String::new()),
        tls: Some("trust server certificate".into()),
        tunnel: None,
    };
    let target = Target::from(&options);
    assert_eq!(target.host, "db.example.com");
    assert_eq!(target.instance.as_deref(), Some("SQLEXPRESS"));
    assert_eq!(
        (target.user.as_str(), target.database.as_str()),
        ("sa", "shop")
    );
    assert_eq!(target.password, None);
    assert_eq!(target.tls, TlsMode::TrustServerCertificate);

    let tunnelled = Target::from(&ConnectOptions {
        tunnel: Some(40000),
        tls: None,
        ..options.clone()
    });
    assert_eq!(
        (tunnelled.host.as_str(), tunnelled.port, tunnelled.instance),
        ("127.0.0.1", 40000, None)
    );
    assert_eq!(tunnelled.tls, TlsMode::Default);
    for (stored, mode) in [
        ("", TlsMode::Default),
        ("disable", TlsMode::Disable),
        ("require", TlsMode::Require),
    ] {
        let tls = Some(stored.to_string());
        let target = Target::from(&ConnectOptions {
            tls,
            ..options.clone()
        });
        assert_eq!(target.tls, mode);
    }
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn connects_and_names_the_server() {
    let adapter = adapter();
    let server = adapter.server();
    assert!(server.starts_with("SQL Server 20"), "{server}");
    assert!(server.contains("(1"), "{server}");
    assert!(adapter.notice().is_none());
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn streams_every_result_set() {
    let mut adapter = adapter();
    let mut seen = Seen::default();
    let changed = adapter
        .execute("select 1 as a; select N'ñandú' as b", &mut seen)
        .unwrap();
    assert_eq!(changed, 0);
    assert_eq!(
        seen.0,
        [
            (vec!["a".to_string()], vec![vec![Some("1".to_string())]]),
            (vec!["b".to_string()], vec![vec![Some("ñandú".to_string())]]),
        ]
    );
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn counts_rows_changed() {
    let table = scratch("count");
    let mut adapter = adapter();
    let changed = adapter.execute(&format!("update {table} set qty = qty + 1"), &mut Discard);
    assert_eq!(changed, Ok(3));
    let changed = adapter.execute(&format!("delete from {table} where id = 1"), &mut Discard);
    assert_eq!(changed, Ok(1));
    // A trigger's own statements do not count.
    run(adapter.as_mut(), "begin transaction");
    let changed = adapter.execute(
        "insert into dbo.order_lines (order_id, line, product_id, quantity) values (1, 3, 1, 1)",
        &mut Discard,
    );
    run(adapter.as_mut(), "rollback");
    assert_eq!(changed, Ok(1));
    drop(adapter);
    drop_scratch(&table);
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn renders_values_like_as_text() {
    let mut cases: Vec<(&str, &str)> = Vec::new();
    let mut add = |ty: &'static str, values: &[&'static str]| {
        cases.extend(values.iter().map(|v| (ty, *v)));
    };
    add("tinyint", &["0", "255", "null"]);
    add("smallint", &["-32768", "32767"]);
    add("int", &["-2147483648", "2147483647", "0", "null"]);
    add("bigint", &["-9223372036854775808", "9223372036854775807"]);
    add("bit", &["0", "1", "null"]);
    add(
        "decimal(10,2)",
        &["0", "-0.05", "12345.60", "99999999.99", "null"],
    );
    add("decimal(18,0)", &["5", "-123456789012345678"]);
    add(
        "decimal(38,10)",
        &[
            "1234567890123456789.1234567891",
            "-0.0000000001",
            "9999999999999999999999999999.9999999999",
        ],
    );
    // Not beyond about 10^14: money arrives as a float (a known difference).
    add(
        "money",
        &[
            "0",
            "1234.5",
            "-0.0001",
            "1000000.5",
            "99999999999.9999",
            "null",
        ],
    );
    add("smallmoney", &["214748.3647", "-214748.3648", "12.5"]);
    add(
        "float",
        &[
            "0",
            "0.1",
            "-1.5e-300",
            "1.7976931348623157e308",
            "123456789.123456789",
            "1e15",
            "1e16",
            "2.2250738585072014e-308",
            "0.7071067811865476",
            "-3.3333333333333335",
            "6.02214076e23",
            "null",
        ],
    );
    add(
        "real",
        &[
            "0",
            "0.1",
            "3.4e38",
            "-1.5e-10",
            "123456.789",
            "16777216",
            "null",
        ],
    );
    add(
        "date",
        &[
            "'0001-01-01'",
            "'1900-01-01'",
            "'2000-02-29'",
            "'9999-12-31'",
            "null",
        ],
    );
    for ty in ["time(0)", "time(1)", "time(3)", "time(7)"] {
        add(
            ty,
            &["'00:00:00'", "'13:45:09'", "'23:59:59.9999999'", "null"],
        );
    }
    add(
        "smalldatetime",
        &[
            "'1900-01-01 00:00'",
            "'2026-10-21 13:45'",
            "'2079-06-06 23:59'",
        ],
    );
    add(
        "datetime",
        &[
            "'1753-01-01 00:00:00.000'",
            "'2026-10-21 13:45:09.003'",
            "'2026-10-21 13:45:09.007'",
            "'2026-10-21 13:45:09.997'",
            "'9999-12-31 23:59:59.997'",
            "'1900-01-01'",
            "null",
        ],
    );
    add(
        "datetime2(0)",
        &["'0001-01-01 00:00:00'", "'9999-12-31 23:59:59'"],
    );
    add("datetime2(3)", &["'2026-10-21 13:45:09.120'"]);
    add(
        "datetime2(7)",
        &[
            "'2026-10-21 13:45:09.1234567'",
            "'9999-12-31 23:59:59.9999999'",
        ],
    );
    add(
        "datetimeoffset(0)",
        &[
            "'2026-10-21 13:45:09 +00:00'",
            "'2026-10-21 13:45:09 +14:00'",
            "'2026-10-21 13:45:09 -12:00'",
        ],
    );
    add(
        "datetimeoffset(7)",
        &[
            "'2026-10-21 13:45:09.1234567 +05:30'",
            "'2026-01-01 00:30:00.5 +05:30'",
            "'2026-12-31 23:30:00.5 -05:30'",
            "null",
        ],
    );
    add(
        "uniqueidentifier",
        &["'6f9619ff-8b86-d011-b42d-00c04fc964ff'", "null"],
    );
    add("nvarchar(50)", &["N''", "N'héllo ''q'' ✓ 😀'", "null"]);
    add("char(5)", &["'ab'"]);
    add("varchar(max)", &["'abc'"]);
    add("nvarchar(max)", &["N'long'"]);
    add("xml", &["'<a x=\"1\"><b/></a>'", "null"]);

    let mut adapter = adapter();
    let as_text = (MSSQL.as_text)("x");
    let mut different = Vec::new();
    for (ty, value) in &cases {
        let sql = format!("select x, {as_text} from (select cast({value} as {ty}) as x) s");
        let rows = fetch(adapter.as_mut(), &sql).unwrap();
        if rows.get(0, 0) != rows.get(0, 1) {
            different.push(format!(
                "{ty} {value}: adapter {:?}, as_text {:?}",
                rows.get(0, 0),
                rows.get(0, 1)
            ));
        }
    }
    assert!(different.is_empty(), "{different:#?}");

    // Binary is the one type `as_text` cannot write: it shows as style 1.
    let rows = fetch(
        adapter.as_mut(),
        "select x, convert(nvarchar(max), x, 1) from (select cast(0x00FF10 as varbinary(10)) as x) s",
    )
    .unwrap();
    assert_eq!(rows.get(0, 0), Some("0x00FF10"));
    assert_eq!(rows.get(0, 0), rows.get(0, 1));
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn notices_when_writes_cannot_be_detected() {
    let table = scratch("notice");
    let mut sa = adapter();
    for sql in [
        "if suser_id('kuery_writer') is null \
         create login kuery_writer with password = 'Kuery_writer_2026!', check_policy = off",
        "use master; revoke view server state from kuery_writer; \
         revoke view server performance state from kuery_writer",
        "use shop; if user_id('kuery_writer') is null create user kuery_writer for login kuery_writer; \
         alter role db_datareader add member kuery_writer; \
         alter role db_datawriter add member kuery_writer",
    ] {
        run(sa.as_mut(), sql);
    }
    let writer = Target {
        user: "kuery_writer".into(),
        password: Some("Kuery_writer_2026!".into()),
        ..target()
    };
    let notice = session(writer.clone(), Access::ReadOnly)
        .notice()
        .map(str::to_string);
    assert!(
        notice
            .as_deref()
            .is_some_and(|n| n.contains("VIEW SERVER PERFORMANCE STATE")),
        "{notice:?}"
    );
    assert_eq!(session(target(), Access::ReadOnly).notice(), None);

    // Undetected, the write is still undone with the read-only transaction.
    let mut adapter = open(&writer, Access::ReadWrite).unwrap();
    for statement in MSSQL.transactions.begin_read_only {
        run(adapter.as_mut(), statement);
    }
    let slipped = adapter.execute(&format!("update {table} set qty = 0"), &mut Discard);
    assert_eq!(slipped, Ok(3));
    for statement in MSSQL.transactions.end_read_only {
        run(adapter.as_mut(), statement);
    }
    drop(adapter);
    let sum = scalar(sa.as_mut(), &format!("select sum(qty) from {table}"));
    assert_eq!(sum.as_deref(), Some("6"));
    drop(sa);
    drop_scratch(&table);
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn refuses_writes_that_slip_into_read_mode() {
    let table = scratch("readonly");
    let mut adapter = adapter();
    let begin = |adapter: &mut dyn Adapter| {
        for statement in MSSQL.transactions.begin_read_only {
            run(adapter, statement);
        }
    };
    let end = |adapter: &mut dyn Adapter| {
        for statement in MSSQL.transactions.end_read_only {
            run(adapter, statement);
        }
    };
    begin(adapter.as_mut());
    let blocked = adapter.execute(&format!("update {table} set qty = 0"), &mut Discard);
    assert_eq!(blocked, Err(QueryError::WritesDisabled));
    // Still reading, in a fresh read-only transaction.
    assert_eq!(
        scalar(adapter.as_mut(), "select @@trancount").as_deref(),
        Some("1")
    );
    let blocked = adapter.execute(&format!("create table {table}_more (i int)"), &mut Discard);
    assert_eq!(blocked, Err(QueryError::WritesDisabled));
    end(adapter.as_mut());
    assert_eq!(
        scalar(adapter.as_mut(), &format!("select sum(qty) from {table}")).as_deref(),
        Some("6")
    );
    assert_eq!(
        scalar(
            adapter.as_mut(),
            &format!("select object_id('{table}_more')")
        ),
        None
    );

    // Temporary tables are not writes.
    begin(adapter.as_mut());
    let temporary = adapter.execute("select * into #t from dbo.orders", &mut Discard);
    assert_eq!(temporary, Ok(20000));
    end(adapter.as_mut());
    drop(adapter);
    drop_scratch(&table);
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn cancels_and_keeps_working() {
    let mut adapter = adapter();
    run(adapter.as_mut(), "create table #marker (i int)");
    cancel_after(adapter.as_ref(), Duration::from_millis(300));
    let started = Instant::now();
    let stopped = adapter.execute("waitfor delay '00:00:20'", &mut Discard);
    assert_eq!(stopped, Err(QueryError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(scalar(adapter.as_mut(), "select 1").as_deref(), Some("1"));
    // On a new connection (its temporary tables are gone), with the
    // settings replayed.
    assert_eq!(
        scalar(adapter.as_mut(), "select object_id('tempdb..#marker')"),
        None
    );
    assert_eq!(
        scalar(adapter.as_mut(), "select @@lock_timeout").as_deref(),
        Some("-1")
    );
    // A cancel when nothing runs does not stop the next statement.
    (adapter.canceller())();
    assert_eq!(scalar(adapter.as_mut(), "select 2").as_deref(), Some("2"));

    // Through a session, in read mode.
    let reader = session(target(), Access::ReadOnly);
    let running = reader.fetch_all("waitfor delay '00:00:20'".into(), RunOptions::default());
    let canceller = reader.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        canceller.cancel();
    });
    let started = Instant::now();
    assert_eq!(block_on(running).err(), Some(QueryError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(5));
    let set = block_on(reader.fetch_all("select 3".into(), RunOptions::default())).unwrap();
    assert_eq!(set.cell(0, 0), Some(Some("3")));
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn cancel_inside_a_write_transaction_says_what_was_lost() {
    let table = scratch("lost");
    let mut adapter = adapter();
    run(adapter.as_mut(), "begin transaction");
    run(
        adapter.as_mut(),
        &format!("insert into {table} values (99, N'lost', 1)"),
    );
    cancel_after(adapter.as_ref(), Duration::from_millis(300));
    let stopped = adapter.execute("waitfor delay '00:00:20'", &mut Discard);
    let Err(QueryError::Connection(message)) = stopped else {
        panic!("{stopped:?}");
    };
    assert!(message.contains("uncommitted"), "{message}");
    assert_eq!(
        scalar(adapter.as_mut(), "select @@trancount").as_deref(),
        Some("0")
    );
    assert_eq!(
        scalar(
            adapter.as_mut(),
            &format!("select count(*) from {table} where id = 99")
        )
        .as_deref(),
        Some("0")
    );
    drop(adapter);
    drop_scratch(&table);
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn untrusted_certificate_points_to_the_option() {
    for tls in [TlsMode::Require, TlsMode::Default] {
        let refused = open(&Target { tls, ..target() }, Access::ReadOnly).err();
        assert!(
            refused.as_deref().is_some_and(
                |e| e.contains("trust server certificate") && !e.contains("trust_cert_ca")
            ),
            "{refused:?}"
        );
    }
    for tls in [TlsMode::TrustServerCertificate, TlsMode::Disable] {
        assert!(open(&Target { tls, ..target() }, Access::ReadOnly).is_ok());
    }
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn lock_timeout_is_reported() {
    let table = scratch("lock");
    let mut holder = adapter();
    run(holder.as_mut(), "begin transaction");
    run(
        holder.as_mut(),
        &format!("update {table} set qty = qty where id = 1"),
    );
    let mut waiter = adapter();
    waiter
        .limit(&RunOptions {
            lock_timeout: Duration::from_secs(1),
            ..RunOptions::default()
        })
        .unwrap();
    let started = Instant::now();
    let blocked = waiter.execute(
        &format!("update {table} set qty = 5 where id = 1"),
        &mut Discard,
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    let Err(QueryError::Database { code, message }) = blocked else {
        panic!("{blocked:?}");
    };
    assert_eq!(code, "1222");
    assert!(message.contains("lock"), "{message}");
    run(holder.as_mut(), "rollback");
    drop((holder, waiter));
    drop_scratch(&table);
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn statement_timeout_stops_reads_but_not_write_transactions() {
    let mut adapter = adapter();
    adapter
        .limit(&RunOptions {
            statement_timeout: Duration::from_secs(1),
            lock_timeout: Duration::from_secs(3),
            ..RunOptions::default()
        })
        .unwrap();
    let started = Instant::now();
    let stopped = adapter.execute("waitfor delay '00:00:20'", &mut Discard);
    assert!(
        matches!(&stopped, Err(QueryError::Database { message, .. }) if message.contains("ran out of time")),
        "{stopped:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    // Reconnected, with the lock timeout replayed.
    assert_eq!(
        scalar(adapter.as_mut(), "select @@lock_timeout").as_deref(),
        Some("3000")
    );
    run(adapter.as_mut(), "begin transaction");
    let waited = adapter.execute("waitfor delay '00:00:02'", &mut Discard);
    assert_eq!(waited, Ok(0));
    assert_eq!(
        scalar(adapter.as_mut(), "select @@trancount").as_deref(),
        Some("1")
    );
    run(adapter.as_mut(), "rollback");
}

#[test]
fn only_a_missing_permission_downgrades_the_writes_check() {
    assert!([300, 297, 262].into_iter().all(lacks_permission));
    assert!(![208, 1222, 18456].into_iter().any(lacks_permission));
    assert_eq!(probe_verdict(Ok(())), Ok(true));
    let broken = tiberius::error::Error::Protocol("cut short".into());
    let refused = probe_verdict(Err(broken)).unwrap_err();
    assert!(refused.contains("cut short"), "{refused}");
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn a_dropped_connection_ends_the_session_transaction_inside_a_read() {
    let table = scratch("dropped");
    let writer = session(target(), Access::ReadWrite);
    drain_all(writer.run_in_transaction(
        format!("insert into {table} values (99, N'lost', 1)"),
        RunOptions::default(),
    ))
    .unwrap();
    assert!(writer.in_transaction());
    let running = writer.run_read_only(
        "waitfor delay '00:00:20'; select 1".into(),
        RunOptions::default(),
    );
    let canceller = writer.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        canceller.cancel();
    });
    let stopped = drain_all(running);
    assert!(
        matches!(stopped, Err(QueryError::Connection(_))),
        "{stopped:?}"
    );
    assert!(!writer.in_transaction());
    drain_all(writer.run_in_transaction(
        format!("insert into {table} values (100, N'new', 1)"),
        RunOptions::default(),
    ))
    .unwrap();
    assert!(writer.in_transaction());
    drop(writer);
    drop_scratch(&table);
}

/// The outcome of a streamed statement.
fn drain_all(rx: async_channel::Receiver<kuery_core::session::Event>) -> Result<(), QueryError> {
    use kuery_core::session::Event;
    block_on(async {
        while let Ok(event) = rx.recv().await {
            match event {
                Event::Finished(_) => return Ok(()),
                Event::Failed(error) => return Err(error),
                _ => {}
            }
        }
        Err(QueryError::Cancelled)
    })
}
