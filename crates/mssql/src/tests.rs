//! Need the dev SQL Server (`docker compose -f dev/compose.yaml up -d mssql
//! mssql-seed`), or one at `KUERY_MSSQL_HOST`/`KUERY_MSSQL_PORT` (password
//! `KUERY_MSSQL_PASSWORD` for `sa`) with the sample `shop`.

use std::time::Instant;

use futures::executor::block_on;
use kuery_core::adapter::{Discard, fetch};
use kuery_core::diagram::OnDelete;
use kuery_core::session::{Opener, Session};

use super::*;

fn target() -> Target {
    let host = std::env::var("KUERY_MSSQL_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    Target {
        server_name: host.clone(),
        host,
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

/// Tests that create or drop things in `shop` (scratch tables, users) run
/// together, but never while `reads_the_catalog` reads it: the catalog
/// queries and that DDL can deadlock (1205).
static SHOP: std::sync::RwLock<()> = std::sync::RwLock::new(());

fn changing_shop() -> std::sync::RwLockReadGuard<'static, ()> {
    SHOP.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn reading_shop_alone() -> std::sync::RwLockWriteGuard<'static, ()> {
    SHOP.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A table of its own for a test that writes (holding [`changing_shop`]);
/// `drop_scratch` drops it.
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

type Streamed = (Vec<String>, Vec<Vec<Option<String>>>);

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
        proxy: None,
        options: Default::default(),
    };
    let target = Target::from(&options);
    assert_eq!(target.host, "db.example.com");
    assert_eq!(target.server_name, "db.example.com");
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
    // The certificate still names the server, not the tunnel.
    assert_eq!(tunnelled.server_name, "db.example.com");
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
    let _shop = changing_shop();
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
    let _shop = changing_shop();
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
    // A login that can only read has nothing to detect.
    for sql in [
        "if suser_id('kuery_reader') is null \
         create login kuery_reader with password = 'Kuery_reader_2026!', check_policy = off",
        "use master; revoke view server state from kuery_reader; \
         revoke view server performance state from kuery_reader",
        "use shop; if user_id('kuery_reader') is null create user kuery_reader for login kuery_reader; \
         alter role db_datareader add member kuery_reader",
    ] {
        run(sa.as_mut(), sql);
    }
    let reader = Target {
        user: "kuery_reader".into(),
        password: Some("Kuery_reader_2026!".into()),
        ..target()
    };
    assert_eq!(session(reader, Access::ReadOnly).notice(), None);

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

/// The notice a login without the writes check gets, after `setup` (run as
/// `sa` in `shop`, with `{user}` for the user) on top of `db_datareader`;
/// `cleanup` undoes what `setup` made before the user and login go.
fn notice_for(login: &str, setup: &[&str], cleanup: &[&str]) -> Option<String> {
    let mut sa = adapter();
    let password = format!("{login}_Pw_2026!");
    let drop_user = format!("use shop; if user_id('{login}') is not null drop user {login}");
    let tidy = |sa: &mut dyn Adapter| {
        for sql in cleanup {
            let _ = sa.execute(
                &format!("use shop; {}", sql.replace("{user}", login)),
                &mut Discard,
            );
        }
        run(sa, &drop_user);
        drop_login(sa, login);
    };
    tidy(sa.as_mut());
    run(
        sa.as_mut(),
        &format!("create login {login} with password = '{password}', check_policy = off"),
    );
    run(
        sa.as_mut(),
        &format!(
            "use shop; create user {login} for login {login}; \
             alter role db_datareader add member {login}"
        ),
    );
    for sql in setup {
        run(
            sa.as_mut(),
            &format!("use shop; {}", sql.replace("{user}", login)),
        );
    }
    let limited = Target {
        user: login.into(),
        password: Some(password),
        ..target()
    };
    let notice = open(&limited, Access::ReadOnly).unwrap().notice();
    tidy(sa.as_mut());
    notice
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn the_notice_shows_for_every_login_that_can_write() {
    let _shop = changing_shop();
    assert_eq!(notice_for("kuery_n_reader", &[], &[]), None);
    let can_write: [(&str, &[&str], &[&str]); 8] = [
        (
            "kuery_n_ddl",
            &["alter role db_ddladmin add member {user}"],
            &[],
        ),
        (
            "kuery_n_schema",
            &["exec ('create schema kuery_n_owned authorization {user}')"],
            &["if schema_id('kuery_n_owned') is not null drop schema kuery_n_owned"],
        ),
        (
            "kuery_n_table",
            &[
                "create table dbo.kuery_n_owned (i int)",
                "alter authorization on dbo.kuery_n_owned to {user}",
            ],
            &["drop table if exists dbo.kuery_n_owned"],
        ),
        (
            "kuery_n_insert",
            &["grant insert on dbo.customers to {user}"],
            &[],
        ),
        (
            "kuery_n_execute",
            &["grant execute on schema::dbo to {user}"],
            &[],
        ),
        (
            "kuery_n_impersonate",
            &["use master; grant impersonate on login::sa to {user}"],
            &["use master; revoke impersonate on login::sa from {user}"],
        ),
        (
            "kuery_n_secadmin",
            &["alter server role securityadmin add member {user}"],
            &["alter server role securityadmin drop member {user}"],
        ),
        (
            "kuery_n_control",
            &["use master; grant control on login::sa to {user}"],
            &["use master; revoke control on login::sa from {user}"],
        ),
    ];
    let silent: Vec<&str> = can_write
        .into_iter()
        .filter(|(login, setup, cleanup)| notice_for(login, setup, cleanup).is_none())
        .map(|(login, _, _)| login)
        .collect();
    assert!(silent.is_empty(), "can write but got no notice: {silent:?}");
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn refuses_writes_that_slip_into_read_mode() {
    let _shop = changing_shop();
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
    let _shop = changing_shop();
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
    let _shop = changing_shop();
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
    assert!([300, 297, 262, 229].into_iter().all(lacks_permission));
    assert!(![208, 1222, 18456].into_iter().any(lacks_permission));
    assert_eq!(probe_verdict(Ok(())), Ok(true));
    let broken = tiberius::error::Error::Protocol("cut short".into());
    let refused = probe_verdict(Err(broken)).unwrap_err();
    assert!(refused.contains("cut short"), "{refused}");
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn a_dropped_connection_ends_the_session_transaction_inside_a_read() {
    let _shop = changing_shop();
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

#[test]
#[ignore = "needs the dev SQL Server"]
fn reads_the_catalog() {
    let _shop = reading_shop_alone();
    use kuery_core::catalog::{ConstraintKind, RoutineKind, TableKind};
    let reader = session(target(), Access::ReadOnly);
    let overview = block_on(reader.overview()).unwrap();
    assert!(overview.schemas.contains(&"dbo".to_string()));
    assert!(overview.schemas.contains(&"sales".to_string()));
    for hidden in ["sys", "INFORMATION_SCHEMA", "guest", "db_owner"] {
        assert!(!overview.schemas.contains(&hidden.to_string()), "{hidden}");
    }
    let dbo: Vec<(&str, TableKind)> = overview
        .tables
        .iter()
        .filter(|t| {
            t.table.schema == "dbo"
                && !t.table.name.starts_with("scratch")
                && !t.table.name.starts_with("kuery_")
        })
        .map(|t| (t.table.name.as_str(), t.kind))
        .collect();
    assert_eq!(
        dbo,
        [
            ("customers", TableKind::Table),
            ("order_lines", TableKind::Table),
            ("orders", TableKind::Table),
            ("paid_orders", TableKind::View),
            ("products", TableKind::Table)
        ]
    );
    let customers = overview
        .tables
        .iter()
        .find(|t| t.table.name == "customers")
        .unwrap();
    assert_eq!(customers.comment.as_deref(), Some("People who buy"));
    assert_eq!(customers.estimated_rows, Some(2000));

    let in_sales = |name: &str| {
        overview
            .functions
            .iter()
            .find(|f| f.schema == "sales" && f.name == name)
            .unwrap()
    };
    assert_eq!(in_sales("raise_quota").kind, RoutineKind::Procedure);
    assert!(
        in_sales("raise_quota")
            .arguments
            .contains("@by decimal(12,2)")
    );
    let reached = in_sales("quota_reached");
    assert_eq!(reached.kind, RoutineKind::Function);
    assert_eq!(reached.result, "bit");
    assert_eq!(reached.arguments, "@country char(2)");
    let source = block_on(reader.routine_source(reached.clone())).unwrap();
    assert!(source.contains("create function"), "{source}");
    assert!(
        overview
            .tables
            .iter()
            .any(|t| t.table.schema == "sales" && t.kind == TableKind::View)
    );
    assert!(
        overview
            .triggers
            .iter()
            .any(|t| t.table == TableRef::new("sales", "quotas") && t.name == "quotas_changed")
    );
    let money = overview
        .enums
        .iter()
        .find(|e| e.name == "money_amount")
        .unwrap();
    assert_eq!(
        (money.schema.as_str(), money.values.as_slice()),
        ("sales", ["decimal(12,2) not null".to_string()].as_slice())
    );

    let orders = block_on(reader.definition(TableRef::new("dbo", "orders"))).unwrap();
    assert_eq!(orders.primary_key(), ["id"]);
    let customer = orders.column("customer_id").unwrap();
    assert_eq!(
        customer.foreign_key.as_ref().unwrap().table.name,
        "customers"
    );
    assert_eq!(
        orders.column("shipping").unwrap().data_type,
        "nvarchar(max)"
    );
    assert!(orders.column("shipping").unwrap().nullable);
    assert_eq!(orders.column("total").unwrap().data_type, "decimal(10,2)");
    assert!(orders.constraints.iter().any(
        |c| c.kind == ConstraintKind::ForeignKey && c.definition.contains("ON DELETE CASCADE")
    ));
    assert!(
        orders
            .constraints
            .iter()
            .any(|c| c.kind == ConstraintKind::Check)
    );
    assert!(orders.indexes.iter().any(|i| i.name == "orders_by_created"));
    let customers = block_on(reader.definition(TableRef::new("dbo", "customers"))).unwrap();
    assert_eq!(customers.column("email").unwrap().data_type, "varchar(120)");
    assert_eq!(
        customers.column("full_name").unwrap().data_type,
        "nvarchar(120)"
    );
    assert!(
        customers
            .constraints
            .iter()
            .any(|c| c.kind == ConstraintKind::Unique)
    );
    let lines = block_on(reader.definition(TableRef::new("dbo", "order_lines"))).unwrap();
    assert_eq!(lines.primary_key(), ["order_id", "line"]);
    assert_eq!(lines.triggers[0].name, "order_lines_total");
    assert!(lines.triggers[0].definition.contains("create trigger"));
    assert!(lines.triggers[0].enabled);
    let quotas = block_on(reader.definition(TableRef::new("sales", "quotas"))).unwrap();
    assert_eq!(
        quotas.column("amount").unwrap().data_type,
        "sales.money_amount"
    );

    let diagram = block_on(reader.diagram("dbo".into())).unwrap();
    let to_orders = diagram
        .relations
        .iter()
        .find(|r| r.name == "lines_order")
        .unwrap();
    assert_eq!(to_orders.on_delete, OnDelete::Cascade);

    let completion = block_on(reader.completion_catalog()).unwrap();
    assert_eq!(completion.search_path, ["dbo"]);
    assert!(
        completion
            .tables
            .iter()
            .any(|t| t.table.name == "paid_orders" && t.kind == TableKind::View)
    );
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn a_column_named_with_a_bracket_round_trips_through_edits() {
    let _shop = changing_shop();
    use kuery_core::edit::{self, CellChange, EditTarget, KeyValue, RowChange};
    let table = format!("scratch_bracket_{}", std::process::id());
    let mut sa = adapter();
    run(
        sa.as_mut(),
        &format!(
            "create table dbo.{table} (id int primary key, [a]]b] nvarchar(20)); \
             insert into dbo.{table} values (1, N'before')"
        ),
    );
    let writer = session(target(), Access::ReadWrite);
    let definition = block_on(writer.definition(TableRef::new("dbo", table.clone()))).unwrap();
    let names: Vec<&str> = definition.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "a]b"]);
    let edit_target = EditTarget {
        table: TableRef::new("dbo", table.clone()),
        columns: names.iter().map(|n| n.to_string()).collect(),
        primary_key: definition.primary_key(),
        binary_columns: Vec::new(),
        dialect: &MSSQL,
    };
    let change = edit::build(
        &edit_target,
        &RowChange::Update {
            key: vec![KeyValue {
                column: "id".into(),
                value: "1".into(),
            }],
            cells: vec![CellChange {
                column: "a]b".into(),
                old: Some("before".into()),
                new: Some("after".into()),
            }],
        },
    )
    .unwrap();
    assert_eq!(block_on(writer.apply(vec![change])), Ok(1));
    assert_eq!(
        scalar(
            sa.as_mut(),
            &format!("select [a]]b] from dbo.{table} where id = 1")
        )
        .as_deref(),
        Some("after")
    );
    drop(writer);
    run(sa.as_mut(), &format!("drop table dbo.{table}"));
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn contains_and_starts_with_filters_match_brackets_literally() {
    let _shop = changing_shop();
    use kuery_core::browse::{Browse, Filter, FilterOp};
    let table = format!("scratch_like_{}", std::process::id());
    let mut sa = adapter();
    run(
        sa.as_mut(),
        &format!(
            "create table dbo.{table} (id int primary key, note nvarchar(20)); \
             insert into dbo.{table} values (1, N'[abc]'), (2, N'a'), (3, N'b'), \
                 (4, N'50% off'), (5, N'a_b'), (6, N'axb'), (7, N'[^x]')"
        ),
    );
    let mut count = |op: FilterOp, value: &str| {
        let mut browse = Browse::new(
            TableRef::new("dbo", table.clone()),
            vec!["id".into(), "note".into()],
        )
        .in_dialect(&MSSQL);
        browse.filters = vec![Filter {
            column: "note".into(),
            op,
            value: value.into(),
        }];
        scalar(sa.as_mut(), &browse.count_sql().unwrap())
    };
    // `[abc]` is the text, not "any of a, b or c".
    assert_eq!(count(FilterOp::Contains, "[abc]").as_deref(), Some("1"));
    assert_eq!(count(FilterOp::StartsWith, "[").as_deref(), Some("2"));
    assert_eq!(count(FilterOp::Contains, "[^x]").as_deref(), Some("1"));
    assert_eq!(count(FilterOp::Contains, "%").as_deref(), Some("1"));
    assert_eq!(count(FilterOp::Contains, "_").as_deref(), Some("1"));
    assert_eq!(count(FilterOp::Contains, "c]").as_deref(), Some("1"));
    run(sa.as_mut(), &format!("drop table dbo.{table}"));
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn works_like_the_other_drivers() {
    let _shop = changing_shop();
    kuery_core::conformance::run(&kuery_core::conformance::Fixture {
        opener: Arc::new(|access| open(&target(), access)),
        schema: "dbo".into(),
        slow_query: "waitfor delay '00:00:20'; select 1".into(),
    });
}

/// The plan of `statement` from a read-only session on `shop`.
fn plan_of(statement: &str, analyze: bool) -> kuery_core::plan::Plan {
    let reader = session(target(), Access::ReadOnly);
    let xml = block_on(reader.explain(statement.into(), analyze, RunOptions::default())).unwrap();
    kuery_core::plan::parse(&xml).unwrap_or_else(|error| panic!("{error}\n{xml}"))
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn explains_estimated_and_measured() {
    let statement =
        "select o.id, c.full_name from dbo.orders o join dbo.customers c on c.id = o.customer_id";
    let estimated = plan_of(statement, false);
    assert!(!estimated.analyzed);
    assert!(estimated.root.actual.is_none());
    let measured = plan_of(statement, true);
    assert!(measured.analyzed);
    assert!(measured.root.actual.is_some());
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn runs_go_separated_batches_in_order() {
    let mut adapter = adapter();
    let mut seen = Seen::default();
    adapter
        .execute("select 1 as a\nGO\nselect 2 as b", &mut seen)
        .unwrap();
    assert_eq!(seen.0.len(), 2);
    assert_eq!(seen.0[1].0, ["b"]);
    // A statement that fails to compile does not leave showplan on.
    let failed = adapter.execute(
        "set showplan_xml on\nGO\nselect * from dbo.no_such_table\nGO\nset showplan_xml off",
        &mut Discard,
    );
    assert!(
        matches!(failed, Err(QueryError::Database { .. })),
        "{failed:?}"
    );
    let mut seen = Seen::default();
    adapter.execute("select 7 as seven", &mut seen).unwrap();
    assert_eq!(seen.0[0].1, [[Some("7".to_string())]]);
}

/// The sessions of two connections, for tests that need a blocker.
struct Blocking {
    table: String,
    holder: Box<dyn Adapter>,
    holder_pid: i32,
}

impl Blocking {
    /// A holds an uncommitted update of row 1.
    fn start(name: &str) -> Self {
        let table = scratch(name);
        let mut holder = adapter();
        run(holder.as_mut(), "begin transaction");
        run(
            holder.as_mut(),
            &format!("update {table} set qty = 10 where id = 1"),
        );
        let holder_pid = scalar(holder.as_mut(), "select @@spid")
            .unwrap()
            .parse()
            .unwrap();
        Blocking {
            table,
            holder,
            holder_pid,
        }
    }

    /// A second connection that updates the same row, waiting on the first;
    /// returns its session id and the thread that finishes with it.
    fn blocked(&self) -> (i32, std::thread::JoinHandle<Result<u64, QueryError>>) {
        let mut waiter = adapter();
        let pid = scalar(waiter.as_mut(), "select @@spid")
            .unwrap()
            .parse()
            .unwrap();
        let update = format!("update {} set qty = 20 where id = 1", self.table);
        (
            pid,
            std::thread::spawn(move || waiter.execute(&update, &mut Discard)),
        )
    }

    fn finish(mut self) {
        run(self.holder.as_mut(), "rollback");
        drop(self.holder);
        drop_scratch(&self.table);
    }
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn sees_a_blocker() {
    let _shop = changing_shop();
    let blocking = Blocking::start("blocker");
    let (waiter, update) = blocking.blocked();
    let mut monitor = adapter();
    assert!(monitor.has_activity());
    let mut seen = None;
    for _ in 0..50 {
        let activity = monitor.activity().unwrap();
        if let Some(backend) = activity.backend(waiter)
            && !backend.blocked_by.is_empty()
        {
            seen = Some((
                backend.clone(),
                activity.backend(blocking.holder_pid).cloned(),
            ));
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let (blocked, holder) = seen.expect("the waiting update shows as blocked");
    assert_eq!(blocked.blocked_by, [blocking.holder_pid]);
    assert!(blocked.is_client() && !blocked.own);
    assert!(blocked.waiting_for.is_some(), "{blocked:?}");
    assert!(
        blocked.query.to_lowercase().contains("update"),
        "{blocked:?}"
    );
    let holder = holder.unwrap();
    assert_eq!(holder.state, kuery_core::activity::State::IdleInTransaction);
    assert!(holder.transaction_seconds.is_some());
    let activity = monitor.activity().unwrap();
    // Other tests of the suite may block each other meanwhile.
    let roots: Vec<i32> = activity.blockers().iter().map(|b| b.pid).collect();
    assert!(roots.contains(&blocking.holder_pid), "{roots:?}");
    assert!(
        activity
            .backends
            .iter()
            .any(|b| b.own && b.state == kuery_core::activity::State::Active)
    );
    assert!(activity.backends.iter().any(|b| !b.is_client()));
    blocking.finish();
    assert_eq!(update.join().unwrap(), Ok(1));
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn kills_a_session() {
    let mut victim = adapter();
    let pid: i32 = scalar(victim.as_mut(), "select @@spid")
        .unwrap()
        .parse()
        .unwrap();
    let mut monitor = adapter();
    assert_eq!(monitor.signal(pid, Signal::Terminate), Ok(true));
    // The victim finds out on its next statement.
    assert!(victim.execute("select 1", &mut Discard).is_err());
    drop(victim);
    // A session that does not exist: not the victim's number, which another
    // connection may already have taken.
    let backends = monitor.activity().unwrap().backends;
    let missing = backends.iter().map(|b| b.pid).max().unwrap() + 1000;
    assert_eq!(monitor.signal(missing, Signal::Terminate), Ok(false));
}

/// Drops `login` if it exists, after ending the sessions it still has.
fn drop_login(adapter: &mut dyn Adapter, login: &str) {
    let end_sessions = format!(
        "declare @kill nvarchar(max) = N''; \
         select @kill += N'kill ' + cast(session_id as nvarchar(10)) + N';' \
         from sys.dm_exec_sessions where login_name = '{login}'; exec (@kill)"
    );
    for _ in 0..30 {
        // A session may end by itself in between.
        let _ = adapter.execute(&end_sessions, &mut Discard);
        let dropped = adapter.execute(
            &format!("if suser_id('{login}') is not null drop login {login}"),
            &mut Discard,
        );
        if dropped.is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("could not drop the login {login}");
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn activity_without_view_server_state_shows_own_sessions() {
    let mut sa = adapter();
    drop_login(sa.as_mut(), "kuery_nomonitor");
    run(
        sa.as_mut(),
        "create login kuery_nomonitor with password = 'Kuery_nomonitor_2026!', check_policy = off",
    );
    let limited = Target {
        user: "kuery_nomonitor".into(),
        password: Some("Kuery_nomonitor_2026!".into()),
        database: "master".into(),
        ..target()
    };
    let mut limited = open(&limited, Access::ReadOnly).unwrap();
    let activity = limited.activity();
    drop(limited);
    drop_login(sa.as_mut(), "kuery_nomonitor");
    let activity = activity.unwrap();
    assert!(!activity.backends.is_empty());
    assert!(activity.backends.iter().all(|b| b.own), "{activity:?}");
}

/// A scratch database that allows snapshot isolation but does not use it by
/// default, with one row (id 1, qty 1) in `t`.
const SNAPSHOT_DB: &str = "kuery_snapshot";

fn create_snapshot_db() {
    let mut sa = adapter();
    drop_snapshot_db();
    run(sa.as_mut(), &format!("create database {SNAPSHOT_DB}"));
    run(
        sa.as_mut(),
        &format!(
            "alter database {SNAPSHOT_DB} set allow_snapshot_isolation on; \
             alter database {SNAPSHOT_DB} set read_committed_snapshot off with rollback immediate"
        ),
    );
    let mut inside = open(
        &Target {
            database: SNAPSHOT_DB.into(),
            ..target()
        },
        Access::ReadWrite,
    )
    .unwrap();
    run(
        inside.as_mut(),
        "create table t (id int primary key, qty int); insert into t values (1, 1)",
    );
}

fn drop_snapshot_db() {
    run(
        adapter().as_mut(),
        &format!(
            "if db_id('{SNAPSHOT_DB}') is not null begin \
             alter database {SNAPSHOT_DB} set single_user with rollback immediate; \
             drop database {SNAPSHOT_DB} end"
        ),
    );
}

fn isolation_level(adapter: &mut dyn Adapter) -> Option<String> {
    scalar(
        adapter,
        "select transaction_isolation_level from sys.dm_exec_sessions where session_id = @@spid",
    )
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn reads_do_not_wait_for_uncommitted_rows_where_snapshots_are_allowed() {
    create_snapshot_db();
    let inside = Target {
        database: SNAPSHOT_DB.into(),
        ..target()
    };
    let mut writer = open(&inside, Access::ReadWrite).unwrap();
    run(writer.as_mut(), "begin transaction");
    run(writer.as_mut(), "update t set qty = 2 where id = 1");

    let reader = session(inside.clone(), Access::ReadOnly);
    let options = RunOptions {
        lock_timeout: Duration::from_secs(2),
        ..RunOptions::default()
    };
    let started = Instant::now();
    let rows = block_on(reader.fetch_all("select qty from t where id = 1".into(), options));
    assert_eq!(rows.unwrap().cell(0, 0), Some(Some("1")));
    assert!(started.elapsed() < Duration::from_secs(2));

    // Snapshot only while the read transaction is open.
    let mut direct = open(&inside, Access::ReadOnly).unwrap();
    assert_eq!(isolation_level(direct.as_mut()).as_deref(), Some("2"));
    for statement in MSSQL.transactions.begin_read_only {
        run(direct.as_mut(), statement);
    }
    assert_eq!(isolation_level(direct.as_mut()).as_deref(), Some("5"));
    for statement in MSSQL.transactions.end_read_only {
        run(direct.as_mut(), statement);
    }
    assert_eq!(isolation_level(direct.as_mut()).as_deref(), Some("2"));

    // Moving to another database (`use` in Full write) checks it again.
    let mut moving = open(&target(), Access::ReadWrite).unwrap();
    let read_level = |adapter: &mut dyn Adapter| {
        for statement in MSSQL.transactions.begin_read_only {
            run(adapter, statement);
        }
        let level = isolation_level(adapter);
        for statement in MSSQL.transactions.end_read_only {
            run(adapter, statement);
        }
        level
    };
    assert_eq!(read_level(moving.as_mut()).as_deref(), Some("2"));
    run(moving.as_mut(), &format!("use {SNAPSHOT_DB}"));
    assert_eq!(read_level(moving.as_mut()).as_deref(), Some("5"));
    assert_eq!(isolation_level(moving.as_mut()).as_deref(), Some("2"));
    run(moving.as_mut(), "use shop");
    assert_eq!(read_level(moving.as_mut()).as_deref(), Some("2"));

    run(writer.as_mut(), "rollback");
    drop((writer, reader, direct, moving));
    drop_snapshot_db();
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn reads_keep_the_default_level_where_snapshots_are_not_allowed() {
    // `shop` reads committed rows through row versions (RCSI), but does not
    // allow snapshot isolation.
    let mut direct = open(&target(), Access::ReadOnly).unwrap();
    assert_eq!(
        scalar(
            direct.as_mut(),
            "select snapshot_isolation_state from sys.databases where database_id = db_id()"
        )
        .as_deref(),
        Some("0")
    );
    for statement in MSSQL.transactions.begin_read_only {
        run(direct.as_mut(), statement);
    }
    assert_eq!(isolation_level(direct.as_mut()).as_deref(), Some("2"));
    for statement in MSSQL.transactions.end_read_only {
        run(direct.as_mut(), statement);
    }
}

const QUERY_STORE_DB: &str = "kuery_query_store";

#[test]
#[ignore = "needs the dev SQL Server"]
fn reads_are_not_refused_when_query_store_starts_an_interval() {
    // The first query of each Query Store interval records it in a system
    // transaction nested in the running one, which logs that one's start.
    let mut sa = adapter();
    let drop_db = format!(
        "if db_id('{QUERY_STORE_DB}') is not null begin \
         alter database {QUERY_STORE_DB} set single_user with rollback immediate; \
         drop database {QUERY_STORE_DB} end"
    );
    run(sa.as_mut(), &drop_db);
    run(sa.as_mut(), &format!("create database {QUERY_STORE_DB}"));
    run(
        sa.as_mut(),
        &format!(
            "alter database {QUERY_STORE_DB} set query_store = on (operation_mode = read_write)"
        ),
    );
    let mut reader = open(&target(), Access::ReadOnly).unwrap();
    run(
        reader.as_mut(),
        &format!(
            "use {QUERY_STORE_DB}; \
             create table t (id int primary key, qty int); insert into t values (1, 1)"
        ),
    );
    // A new interval starts with the next query.
    run(
        sa.as_mut(),
        &format!("alter database {QUERY_STORE_DB} set query_store clear"),
    );
    for statement in MSSQL.transactions.begin_read_only {
        run(reader.as_mut(), statement);
    }
    let read = fetch(reader.as_mut(), "select qty from t where id = 1");
    assert_eq!(read.unwrap().get(0, 0), Some("1"));
    // A write after it still counts.
    let blocked = reader.execute("update t set qty = 2 where id = 1", &mut Discard);
    assert_eq!(blocked, Err(QueryError::WritesDisabled));
    for statement in MSSQL.transactions.end_read_only {
        run(reader.as_mut(), statement);
    }
    drop(reader);
    run(sa.as_mut(), &drop_db);
}

fn result_sets(adapter: &mut dyn Adapter, sql: &str) -> usize {
    let mut seen = Seen::default();
    adapter.execute(sql, &mut seen).unwrap();
    seen.0.len()
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn a_trailing_go_is_not_sent_to_the_server() {
    let mut adapter = adapter();
    assert_eq!(result_sets(adapter.as_mut(), "select 1\nGO"), 1);
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn go_text_cannot_write_inside_the_read_only_transaction() {
    let _shop = changing_shop();
    let table = scratch("goreadonly");
    let write = format!("update {table} set qty = 0");
    let texts = [
        format!("{write}\nGO\nselect 1\nGO\nselect 2"),
        format!("select 1\nGO\n{write}\nGO\nselect 2"),
        format!("select 1\nGO\nselect 2\nGO\n{write}"),
    ];
    let mut adapter = adapter();
    for text in &texts {
        for statement in MSSQL.transactions.begin_read_only {
            run(adapter.as_mut(), statement);
        }
        let blocked = adapter.execute(text, &mut Discard);
        assert_eq!(blocked, Err(QueryError::WritesDisabled), "{text}");
        for statement in MSSQL.transactions.end_read_only {
            run(adapter.as_mut(), statement);
        }
        let sum = scalar(adapter.as_mut(), &format!("select sum(qty) from {table}"));
        assert_eq!(sum.as_deref(), Some("6"), "{text}");
    }
    drop(adapter);
    drop_scratch(&table);
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn measured_plans_of_writes_are_refused_or_undone() {
    let _shop = changing_shop();
    let table = scratch("explainwrite");
    let update = format!("update {table} set qty = 0");
    let reader = session(target(), Access::ReadOnly);
    let refused = block_on(reader.explain(update.clone(), true, RunOptions::default()));
    assert!(
        matches!(refused, Err(QueryError::Rejected(_))),
        "{refused:?}"
    );

    let writer = session(target(), Access::ReadWrite);
    let xml = block_on(writer.explain_and_undo(update, RunOptions::default())).unwrap();
    let plan = kuery_core::plan::parse(&xml).unwrap();
    assert!(plan.analyzed);
    let mut sa = adapter();
    let sum = scalar(sa.as_mut(), &format!("select sum(qty) from {table}"));
    assert_eq!(sum.as_deref(), Some("6"));
    drop(writer);
    drop_scratch(&table);
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn a_refused_batch_does_not_leave_plan_mode_on() {
    let _shop = changing_shop();
    let table = scratch("planmode");
    let mut adapter = adapter();
    for statement in MSSQL.transactions.begin_read_only {
        run(adapter.as_mut(), statement);
    }
    let blocked = adapter.execute(
        &format!(
            "set statistics xml on\nGO\nupdate {table} set qty = 0\nGO\nset statistics xml off"
        ),
        &mut Discard,
    );
    assert_eq!(blocked, Err(QueryError::WritesDisabled));
    assert_eq!(result_sets(adapter.as_mut(), "select 1"), 1);
    for statement in MSSQL.transactions.end_read_only {
        run(adapter.as_mut(), statement);
    }
    drop(adapter);
    drop_scratch(&table);
}

/// A sink whose reader goes away after the first row.
struct GoneAfterFirstRow(bool);

impl RowSink for GoneAfterFirstRow {
    fn wants_kinds(&self) -> bool {
        false
    }

    fn columns(&mut self, _: Vec<String>, _: Option<Vec<ColumnKind>>) -> Result<(), QueryError> {
        Ok(())
    }

    fn row(&mut self, _: &[Option<&str>]) -> Result<(), QueryError> {
        match std::mem::replace(&mut self.0, true) {
            true => Err(QueryError::Cancelled),
            false => Ok(()),
        }
    }
}

const ENDLESS: &str = "select a.object_id, b.name from sys.all_objects a \
                       cross join sys.all_objects b cross join sys.all_objects c";

#[test]
#[ignore = "needs the dev SQL Server"]
fn a_dropped_reader_stops_the_statement() {
    let _shop = changing_shop();
    // Through a session, as the app drops a run's receiver.
    let reader = session(target(), Access::ReadOnly);
    let events = reader.run_read_only(
        ENDLESS.into(),
        RunOptions {
            first_batch_rows: 100,
            ..RunOptions::default()
        },
    );
    loop {
        match block_on(events.recv()).unwrap() {
            kuery_core::session::Event::Batch(_) => break,
            kuery_core::session::Event::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    drop(events);
    let started = Instant::now();
    let set = block_on(reader.fetch_all("select 1".into(), RunOptions::default())).unwrap();
    assert_eq!(set.cell(0, 0), Some(Some("1")));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );

    // In a write transaction the rest is read instead, keeping it.
    let table = scratch("gone");
    let mut writer = adapter();
    run(writer.as_mut(), "begin transaction");
    run(
        writer.as_mut(),
        &format!("update {table} set qty = 7 where id = 1"),
    );
    let gone = writer.execute(
        &format!("select top 50000 a.object_id from {table} cross join sys.all_objects a cross join sys.all_objects b"),
        &mut GoneAfterFirstRow(false),
    );
    assert_eq!(gone, Err(QueryError::Cancelled));
    assert_eq!(
        scalar(writer.as_mut(), "select @@trancount").as_deref(),
        Some("1")
    );
    assert_eq!(
        scalar(
            writer.as_mut(),
            &format!("select qty from {table} where id = 1")
        )
        .as_deref(),
        Some("7")
    );
    run(writer.as_mut(), "rollback");
    drop(writer);
    drop_scratch(&table);
}

#[test]
#[ignore = "needs the dev SQL Server"]
fn a_plan_inside_a_read_keeps_the_writes_check() {
    let _shop = changing_shop();
    let table = scratch("planread");
    let mut adapter = adapter();
    for statement in MSSQL.transactions.begin_read_only {
        run(adapter.as_mut(), statement);
    }
    let plan = (MSSQL.explain.unwrap())(&format!("select * from {table}"), false).unwrap();
    let mut seen = Seen::default();
    adapter.execute(&plan, &mut seen).unwrap();
    assert!(seen.0.iter().any(|(_, rows)| {
        rows.iter()
            .flatten()
            .flatten()
            .any(|cell| cell.contains("ShowPlanXML"))
    }));
    // Still in the read-only transaction, still checked.
    let blocked = adapter.execute(&format!("update {table} set qty = 0"), &mut Discard);
    assert_eq!(blocked, Err(QueryError::WritesDisabled));
    for statement in MSSQL.transactions.end_read_only {
        run(adapter.as_mut(), statement);
    }
    drop(adapter);
    drop_scratch(&table);
}

#[test]
fn plan_modes_are_read_from_their_set_batches() {
    assert_eq!(plan_mode("set showplan_xml on"), Some(true));
    assert_eq!(plan_mode(" SET NOEXEC OFF; "), Some(false));
    assert_eq!(plan_mode("set statistics xml on"), None);
    assert_eq!(plan_mode("set showplan_xml on; select 1"), None);
    assert_eq!(plan_mode("select 1"), None);
}
