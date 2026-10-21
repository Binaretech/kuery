//! Throwaway spike: cancel, write detection, value text and TLS against the dev server.
//! Run: `cargo run -p kuery-mssql --example spike -- <mode>` with mode one of
//! cancel, cancel2, socket, write, values, astext, tls, parse, logins, grants, tlsmodes, render.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

type Conn = Client<Compat<TcpStream>>;

fn config() -> Config {
    config_as("sa", "Kuery_dev_2026!")
}

fn config_as(user: &str, password: &str) -> Config {
    let mut c = Config::new();
    c.host("localhost");
    c.port(14339);
    c.authentication(AuthMethod::sql_server(user, password));
    c.trust_cert();
    c.encryption(EncryptionLevel::Required);
    c
}

async fn connect() -> Conn {
    connect_with(config()).await
}

async fn connect_with(c: Config) -> Conn {
    let tcp = TcpStream::connect(c.get_addr()).await.unwrap();
    tcp.set_nodelay(true).unwrap();
    Client::connect(c, tcp.compat_write()).await.unwrap()
}

async fn scalar(c: &mut Conn, sql: &str) -> String {
    let row = c.simple_query(sql).await.unwrap().into_row().await.unwrap().unwrap();
    format!("{:?}", row.into_iter().next().unwrap())
}

#[tokio::main]
async fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "cancel" => cancel().await,
        "socket" => socket_drop().await,
        "write" => write().await,
        "values" => values().await,
        "astext" => astext().await,
        "cancel2" => cancel2().await,
        "logins" => logins().await,
        "grants" => grants().await,
        "tlsmodes" => tlsmodes().await,
        "render" => render_check().await,
        "tls" => {
            let mut c = connect().await;
            println!("encrypted: {}", scalar(&mut c, "select encrypt_option from sys.dm_exec_connections where session_id = @@spid").await);
            println!("version: {}", scalar(&mut c, "select @@version").await);
        }
        "parse" => parse(),
        _ => eprintln!("usage: spike cancel|cancel2|socket|write|values|astext|tls|parse|logins|grants|tlsmodes|render"),
    }
}

/// Attention through `cancel_query`, then check the same connection is still in sync.
async fn cancel() {
    let slow_select = "select count(*) from sys.all_objects a cross join sys.all_objects b cross join sys.all_objects c cross join sys.all_objects d";
    let rows_select = "select a.object_id from sys.all_objects a cross join sys.all_objects b cross join sys.all_objects c";
    // (label, sql, drop while waiting for the first token / after first row)
    for (label, sql, after_first_item) in [
        ("waitfor, nothing sent yet", "waitfor delay '00:00:20'", false),
        ("metadata first, then waitfor", "select 1 as a; waitfor delay '00:00:20'", true),
        ("slow aggregate select", slow_select, false),
        ("slow select streaming rows", rows_select, true),
    ] {
        println!("--- {label}");
        let mut c = connect().await;
        let spid = scalar(&mut c, "select @@spid").await;
        let stop = Arc::new(Notify::new());
        let s2 = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            s2.notify_one();
        });
        let started = Instant::now();
        tokio::select! {
            _ = stop.notified() => println!("stopped waiting after {:?}", started.elapsed()),
            _ = async {
                let mut stream = c.simple_query(sql).await.unwrap();
                let mut seen = 0;
                while let Some(i) = stream.next().await {
                    i.unwrap();
                    seen += 1;
                    if after_first_item && seen >= 2 { std::future::pending::<()>().await; }
                }
            } => println!("statement finished by itself after {:?}", started.elapsed()),
        }
        let t = Instant::now();
        let r = c.cancel_query().await;
        println!("cancel_query -> {r:?} in {:?}", t.elapsed());
        for n in 0..3 {
            let r = c.simple_query("select @@spid, 777").await;
            match r {
                Ok(st) => println!("  extra {n}: {:?}", st.into_results().await.map(|r| r.len())),
                Err(e) => println!("  extra {n}: {e:?}"),
            }
        }
        let t = Instant::now();
        let check = c.simple_query("select @@spid, 777").await.unwrap().into_row().await;
        match check {
            Ok(Some(row)) => {
                let cells: Vec<_> = row.into_iter().collect();
                println!("next query on same connection in {:?}: {:?} (spid before {spid})", t.elapsed(), cells);
            }
            other => println!("next query failed: {:?}", other.map(|r| r.is_some())),
        }
        let mut other = connect().await;
        println!("still running on server: {}", scalar(&mut other, "select count(*) from sys.dm_exec_requests where wait_type = 'WAITFOR' or (session_id <> @@spid and command = 'SELECT' and total_elapsed_time > 200)").await);
    }
}

/// Fallback: drop the socket, then check the server session is gone and time the reconnect.
async fn socket_drop() {
    let slow_select = "select count(*) from sys.all_objects a cross join sys.all_objects b cross join sys.all_objects c cross join sys.all_objects d";
    let mut other = connect().await;
    other.simple_query("drop table if exists dbo.spike_cancel").await.unwrap().into_results().await.unwrap();
    for (label, sql) in [
        ("waitfor", "waitfor delay '00:00:20'"),
        ("slow aggregate select (no rows sent)", slow_select),
        ("open write transaction, then waitfor", "begin transaction; create table dbo.spike_cancel (i int); insert into dbo.spike_cancel values (1); waitfor delay '00:00:20'"),
        ("open write transaction, then slow select", "begin transaction; create table dbo.spike_cancel (i int); insert into dbo.spike_cancel values (2); select count(*) from sys.all_objects a cross join sys.all_objects b cross join sys.all_objects c cross join sys.all_objects d"),
    ] {
        println!("--- {label}");
        let mut c = connect().await;
        let spid = scalar(&mut c, "select @@spid").await;
        let spid: i16 = spid.trim_start_matches("I16(Some(").trim_end_matches("))").parse().unwrap();
        let _ = tokio::time::timeout(Duration::from_millis(300), async {
            let mut s = c.simple_query(sql).await.unwrap();
            while s.next().await.is_some() {}
        })
        .await;
        let t = Instant::now();
        drop(c);
        let mut gone_after = None;
        for _ in 0..200 {
            let n = scalar(&mut other, &format!("select count(*) from sys.dm_exec_sessions where session_id = {spid}")).await;
            if n.contains("Some(0)") { gone_after = Some(t.elapsed()); break; }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        println!("server session {spid} gone after drop: {gone_after:?}");
        let t = Instant::now();
        let mut c2 = connect().await;
        println!("reconnect {:?}; select 1 -> {}", t.elapsed(), scalar(&mut c2, "select 1").await);
        println!("table left behind by the lost write transaction: {}", scalar(&mut c2, "select count(*) from sys.tables where name = 'spike_cancel'").await);
    }
}

async fn write() {
    let mut c = connect().await;
    let q = "select coalesce(sum(database_transaction_log_record_count), 0) from sys.dm_tran_database_transactions where transaction_id = (select transaction_id from sys.dm_tran_current_transaction) and database_id <> 2";
    c.simple_query("drop table if exists dbo.spike").await.unwrap().into_results().await.unwrap();
    c.simple_query("begin transaction").await.unwrap().into_results().await.unwrap();
    c.simple_query("select * into #t from sys.objects").await.unwrap().into_results().await.unwrap();
    println!("after temp table: {}", scalar(&mut c, q).await);
    c.simple_query("create table dbo.spike (i int)").await.unwrap().into_results().await.unwrap();
    println!("after create table: {}", scalar(&mut c, q).await);
    c.simple_query("rollback transaction").await.unwrap().into_results().await.unwrap();
    println!("table exists after rollback: {}", scalar(&mut c, "select count(*) from sys.tables where name = 'spike'").await);
    // insert into a real table, and a read-only select
    c.simple_query("create table dbo.spike (i int)").await.unwrap().into_results().await.unwrap();
    c.simple_query("begin transaction").await.unwrap().into_results().await.unwrap();
    c.simple_query("select count(*) from dbo.spike").await.unwrap().into_results().await.unwrap();
    println!("after select: {}", scalar(&mut c, q).await);
    c.simple_query("insert into dbo.spike values (1)").await.unwrap().into_results().await.unwrap();
    println!("after insert: {}", scalar(&mut c, q).await);
    c.simple_query("rollback transaction").await.unwrap().into_results().await.unwrap();
    c.simple_query("drop table dbo.spike").await.unwrap().into_results().await.unwrap();
}

async fn values() {
    let mut c = connect().await;
    let cases: &[(&str, &str)] = &[
        ("int", "cast(-42 as int)"),
        ("bigint", "cast(9223372036854775807 as bigint)"),
        ("bit", "cast(1 as bit)"),
        ("decimal(10,2)", "cast(12345.60 as decimal(10,2))"),
        ("decimal(10,2) neg small", "cast(-0.05 as decimal(10,2))"),
        ("money", "cast(1234.5 as money)"),
        ("float", "cast(0.1 as float)"),
        ("float big", "cast(1.5e20 as float)"),
        ("real", "cast(0.1 as real)"),
        ("date", "cast('2026-10-21' as date)"),
        ("time", "cast('13:45:09.1234567' as time(7))"),
        ("time(3)", "cast('13:45:09.120' as time(3))"),
        ("datetime", "cast('2026-10-21 13:45:09.123' as datetime)"),
        ("datetime2", "cast('2026-10-21 13:45:09.1234567' as datetime2(7))"),
        ("datetime2(0)", "cast('2026-10-21 13:45:09' as datetime2(0))"),
        ("datetimeoffset", "cast('2026-10-21 13:45:09.1234567 +05:30' as datetimeoffset(7))"),
        ("uniqueidentifier", "cast('6f9619ff-8b86-d011-b42d-00c04fc964ff' as uniqueidentifier)"),
        ("nvarchar", "cast(N'héllo ''q'' ✓' as nvarchar(50))"),
        ("varbinary", "cast(0x00FF10 as varbinary(10))"),
        ("tinyint", "cast(7 as tinyint)"),
        ("smallint", "cast(-7 as smallint)"),
        ("smallmoney", "cast(12.5 as smallmoney)"),
        ("smalldatetime", "cast('2026-10-21 13:45' as smalldatetime)"),
        ("varchar", "cast('abc' as varchar(10))"),
        ("null int", "cast(null as int)"),
        ("numeric(38,10)", "cast(1234567890123456789.1234567891 as numeric(38,10))"),
        ("datetimeoffset neg", "cast('2026-10-21 03:15:09.5 -08:00' as datetimeoffset(1))"),
        ("datetime 1/300", "cast('2026-10-21 13:45:09.007' as datetime)"),
        ("xml", "cast('<a x=\"1\"><b/></a>' as xml)"),
    ];
    for style in ["126", "121"] {
        println!("=== style {style}");
        for (name, expr) in cases {
            let t = format!("coalesce(try_convert(nvarchar(max), x, {style}), convert(nvarchar(max), x))");
            let sql = format!("select x, {t} from (select {expr} as x) s");
            match c.simple_query(&sql).await {
                Err(e) => println!("{name:26} ERR {e}"),
                Ok(s) => match s.into_row().await {
                    Err(e) => println!("{name:26} ERR {e}"),
                    Ok(None) => println!("{name:26} no row"),
                    Ok(Some(row)) => {
                        let cells: Vec<_> = row.into_iter().collect();
                        println!("{name:26} tiberius={:?}  text={:?}", cells[0], cells[1]);
                    }
                },
            }
        }
    }
}

/// Does a second `cancel_query` (or a marker query) bring the connection back in sync?
async fn cancel2() {
    let mut c = connect().await;
    let stop = Arc::new(Notify::new());
    let s2 = stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        s2.notify_one();
    });
    tokio::select! {
        _ = stop.notified() => {}
        _ = async {
            let mut stream = c.simple_query("waitfor delay '00:00:20'").await.unwrap();
            while let Some(i) = stream.next().await { i.unwrap(); }
        } => println!("finished by itself"),
    }
    println!("cancel #1 -> {:?}", c.cancel_query().await);
    println!("cancel #2 -> {:?}", c.cancel_query().await);
    for n in 1..=3 {
        let r = c.simple_query(format!("select {n}00 as marker")).await;
        match r {
            Ok(st) => {
                let sets = st.into_results().await.unwrap();
                let vals: Vec<String> = sets.iter().flatten().map(|r| format!("{:?}", r.get::<i32, _>(0))).collect();
                println!("query marker {n}00 -> {} result sets, values {vals:?}", sets.len());
            }
            Err(e) => println!("query {n}: {e:?}"),
        }
    }
}

/// Which single expression turns every column type into text? Real columns, so the types are static.
async fn astext() {
    let mut c = connect().await;
    c.simple_query("drop table if exists #t").await.unwrap().into_results().await.unwrap();
    c.simple_query("create table #t (c_int int, c_bigint bigint, c_bit bit, c_dec decimal(10,2), c_money money, c_float float, c_real real, c_date date, c_time time(7), c_dt datetime, c_dt2 datetime2(7), c_dto datetimeoffset(7), c_guid uniqueidentifier, c_nv nvarchar(50), c_nvmax nvarchar(max), c_vb varbinary(10), c_vbmax varbinary(max), c_xml xml, c_sdt smalldatetime, c_tiny tinyint, c_char varchar(20), c_text varchar(max))").await.unwrap().into_results().await.unwrap();
    c.simple_query("insert into #t values (-42, 9223372036854775807, 1, 12345.60, 1234.5, 0.123456789012345678, 0.1, '2026-10-21', '13:45:09.1234567', '2026-10-21 13:45:09.123', '2026-10-21 13:45:09.1234567', '2026-10-21 13:45:09.1234567 +05:30', '6f9619ff-8b86-d011-b42d-00c04fc964ff', N'héllo', N'long', 0x00FF10, 0x00FF10, '<a x=\"1\"><b/></a>', '2026-10-21 13:45:00', 7, 'abc', 'def')").await.unwrap().into_results().await.unwrap();
    let cols = ["c_int","c_bigint","c_bit","c_dec","c_money","c_float","c_real","c_date","c_time","c_dt","c_dt2","c_dto","c_guid","c_nv","c_nvmax","c_vb","c_vbmax","c_xml","c_sdt","c_tiny","c_char","c_text"];
    let candidates = [
        ("convert(nvarchar(max), {x}, 126)", ""),
        ("try_convert(nvarchar(max), {x}, 126)", ""),
        ("convert(nvarchar(max), {x}, 121)", ""),
        ("convert(nvarchar(max), {x})", ""),
        ("concat({x}, N'')", ""),
        ("cast({x} as nvarchar(max))", ""),
        ("convert(nvarchar(max), {x}, 3)", ""),
        ("convert(nvarchar(max), {x}, 1)", ""),
        ("convert(nvarchar(max), {x}, 0)", ""),
        ("format({x}, 'G')", ""),
    ];
    for (cand, _) in candidates {
        println!("=== {cand}");
        for col in cols {
            let sql = format!("select {} from #t", cand.replace("{x}", col));
            let out = match c.simple_query(&sql).await {
                Err(e) => format!("ERR {}", e.to_string().chars().take(90).collect::<String>()),
                Ok(st) => match st.into_row().await {
                    Ok(Some(row)) => format!("{:?}", row.into_iter().next().unwrap()),
                    Ok(None) => "no row".into(),
                    Err(e) => format!("ERR {}", e.to_string().chars().take(90).collect::<String>()),
                },
            };
            println!("  {col:9} {out}");
        }
    }
    // The variant route: one `case` on the base type, for types sql_variant can hold.
    println!("=== sql_variant route");
    for col in cols {
        let sql = format!("select sql_variant_property(cast({col} as sql_variant), 'BaseType') from #t");
        let out = match c.simple_query(&sql).await {
            Err(e) => format!("ERR {}", e.to_string().chars().take(90).collect::<String>()),
            Ok(st) => match st.into_row().await {
                Ok(Some(row)) => format!("{:?}", row.into_iter().next().unwrap()),
                other => format!("{other:?}"),
            },
        };
        println!("  {col:9} {out}");
    }
}

async fn run(c: &mut Conn, sql: &str) -> Result<(), String> {
    c.simple_query(sql).await.map_err(|e| e.to_string())?.into_results().await.map_err(|e| e.to_string())?;
    Ok(())
}

async fn try_scalar(c: &mut Conn, sql: &str) -> String {
    match c.simple_query(sql).await {
        Err(e) => format!("ERR {e}"),
        Ok(st) => match st.into_row().await {
            Ok(Some(row)) => format!("{:?}", row.into_iter().next().unwrap()),
            Ok(None) => "no row".into(),
            Err(e) => format!("ERR {e}"),
        },
    }
}

/// Setup (as sa) for the two non-admin logins, then try write detection as each of them.
async fn logins() {
    let mut sa = connect().await;
    for sql in [
        "if db_id('shop') is null create database shop",
        "if suser_id('kuery_reader') is null create login kuery_reader with password = 'Kuery_reader_2026!', check_policy = off",
        "if suser_id('kuery_writer') is null create login kuery_writer with password = 'Kuery_writer_2026!', check_policy = off",
        "use shop; if user_id('kuery_reader') is null create user kuery_reader for login kuery_reader; alter role db_datareader add member kuery_reader",
        "use shop; if user_id('kuery_writer') is null create user kuery_writer for login kuery_writer; alter role db_datareader add member kuery_writer; alter role db_datawriter add member kuery_writer",
        "use shop; if object_id('dbo.spike_w') is null create table dbo.spike_w (i int)",
        "use shop; grant select, insert, delete on dbo.spike_w to public",
    ] {
        println!("setup: {:?}", run(&mut sa, sql).await);
    }
    let original = "select coalesce(sum(database_transaction_log_record_count), 0) from sys.dm_tran_database_transactions where transaction_id = (select transaction_id from sys.dm_tran_current_transaction) and database_id <> 2";
    let candidates = [
        ("original (dm_tran_database_transactions)", original),
        ("dm_tran_session_transactions join", "select count(*) from sys.dm_tran_session_transactions s join sys.dm_tran_database_transactions d on d.transaction_id = s.transaction_id where s.session_id = @@spid"),
        ("dm_tran_current_transaction only", "select transaction_id from sys.dm_tran_current_transaction"),
        ("dm_tran_active_transactions", "select transaction_state from sys.dm_tran_active_transactions where transaction_id = (select transaction_id from sys.dm_tran_current_transaction)"),
        ("xact_state / trancount", "select xact_state(), @@trancount"),
        ("dm_exec_sessions own row (writes)", "select writes, logical_reads from sys.dm_exec_sessions where session_id = @@spid"),
        ("dm_exec_requests own row (writes)", "select writes from sys.dm_exec_requests where session_id = @@spid"),
        ("dm_exec_connections num_writes", "select num_writes from sys.dm_exec_connections where session_id = @@spid"),
        ("dm_db_session_space_usage", "select user_objects_alloc_page_count from sys.dm_db_session_space_usage where session_id = @@spid"),
        ("dm_tran_locks own X locks", "select count(*) from sys.dm_tran_locks where request_session_id = @@spid and request_mode in ('X','IX','SIX','U') and resource_database_id <> 2"),
        ("has_perms_by_name view server state", "select has_perms_by_name(null, null, 'VIEW SERVER STATE'), has_perms_by_name(null, null, 'VIEW DATABASE STATE')"),
    ];
    for (user, pw) in [("kuery_reader", "Kuery_reader_2026!"), ("kuery_writer", "Kuery_writer_2026!")] {
        println!("##### login {user}");
        let mut cfg = config_as(user, pw);
        cfg.database("shop");
        let mut c = connect_with(cfg).await;
        for (step, sql) in [
            ("after select only", "select count(*) from dbo.spike_w"),
            ("after insert", "insert into dbo.spike_w values (1)"),
            ("after create table", "create table dbo.spike_ddl (i int)"),
        ] {
            let _ = run(&mut c, "if @@trancount > 0 rollback").await;
            let _ = run(&mut c, "begin transaction").await;
            println!("-- {step}: {:?}", run(&mut c, sql).await);
            for (name, q) in &candidates {
                println!("   {name:42} {}", try_scalar(&mut c, q).await);
            }
            let _ = run(&mut c, "rollback").await;
        }
    }
    let _ = run(&mut sa, "use shop; delete from dbo.spike_w").await;
}

/// Which `EncryptionLevel` (and `trust_cert`) each connection mode maps to, against the dev server.
async fn tlsmodes() {
    for (label, level, trust) in [
        ("disable                    ", EncryptionLevel::NotSupported, false),
        ("Off (login only encrypted) ", EncryptionLevel::Off, false),
        ("Off + trust                ", EncryptionLevel::Off, true),
        ("default: On, no trust      ", EncryptionLevel::On, false),
        ("default: On + trust        ", EncryptionLevel::On, true),
        ("require, no trust          ", EncryptionLevel::Required, false),
        ("require + trust (\"trust\")  ", EncryptionLevel::Required, true),
    ] {
        // Not `config()`: that one already trusts the certificate.
        let mut c = Config::new();
        c.host("localhost");
        c.port(14339);
        c.authentication(AuthMethod::sql_server("sa", "Kuery_dev_2026!"));
        c.encryption(level);
        if trust {
            c.trust_cert();
        }
        let tcp = TcpStream::connect(c.get_addr()).await.unwrap();
        match Client::connect(c, tcp.compat_write()).await {
            Err(e) => println!("{label} -> connect failed: {e}"),
            Ok(mut cl) => println!(
                "{label} -> ok, encrypt_option = {}",
                scalar(&mut cl, "select encrypt_option from sys.dm_exec_connections where session_id = @@spid").await
            ),
        }
    }
}

/// Write detection for a writer login with the server-state permission granted, and the
/// no-permission fallbacks (rows affected, `sys.objects` change).
async fn grants() {
    let original = "select coalesce(sum(database_transaction_log_record_count), 0) from sys.dm_tran_database_transactions where transaction_id = (select transaction_id from sys.dm_tran_current_transaction) and database_id <> 2";
    let mut sa = connect().await;
    println!("setup: {:?}", run(&mut sa, "use shop; alter role db_ddladmin add member kuery_writer").await);
    for grant in ["none", "VIEW SERVER PERFORMANCE STATE", "VIEW SERVER STATE"] {
        let _ = run(&mut sa, "use master; revoke view server state from kuery_writer; revoke view server performance state from kuery_writer").await;
        if grant != "none" {
            println!("grant {grant}: {:?}", run(&mut sa, &format!("use master; grant {grant} to kuery_writer")).await);
        }
        println!("##### kuery_writer (db_datareader, db_datawriter, db_ddladmin), grant = {grant}");
        let mut cfg = config_as("kuery_writer", "Kuery_writer_2026!");
        cfg.database("shop");
        let mut c = connect_with(cfg).await;
        let _ = run(&mut c, "begin transaction").await;
        println!("  after select:       {}", try_scalar(&mut c, original).await);
        let r = c.execute("insert into dbo.spike_w values (1), (2)", &[]).await;
        println!("  insert rows_affected (no perms needed): {:?}", r.map(|r| r.rows_affected().to_vec()));
        println!("  after insert:       {}", try_scalar(&mut c, original).await);
        let before = try_scalar(&mut c, "select max(modify_date) from sys.objects").await;
        println!("  create table: {:?}", run(&mut c, "create table dbo.spike_ddl (i int)").await);
        let after = try_scalar(&mut c, "select max(modify_date) from sys.objects").await;
        println!("  after create table: {}", try_scalar(&mut c, original).await);
        println!("  sys.objects max(modify_date) before/after (no perms needed): {before} / {after}");
        let _ = run(&mut c, "rollback").await;
    }
    let _ = run(&mut sa, "use master; revoke view server state from kuery_writer; revoke view server performance state from kuery_writer; use shop; delete from dbo.spike_w; drop table if exists dbo.spike_ddl; alter role db_ddladmin drop member kuery_writer").await;
}

fn parse() {
    use sqlparser::dialect::MsSqlDialect;
    use sqlparser::parser::Parser;
    for sql in ["waitfor delay '00:00:20'", "waitfor delay '00:00:20'; select 1", "select 1"] {
        println!("{sql:45} -> {:?}", Parser::parse_sql(&MsSqlDialect {}, sql));
    }
}

use tiberius::ColumnData;
use tiberius::time::{Date, DateTime2, Time};

/// Days since 0001-01-01 to (year, month, day), proleptic Gregorian.
fn civil(days_since_year_1: i64) -> (i64, u32, u32) {
    let z = days_since_year_1 - 719162 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

fn date_text(days_since_year_1: i64) -> String {
    let (y, m, d) = civil(days_since_year_1);
    format!("{y:04}-{m:02}-{d:02}")
}

fn clock(secs: u64, frac: Option<(u64, u8)>) -> String {
    let mut s = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    if let Some((f, scale)) = frac {
        if scale > 0 && f != 0 {
            s.push_str(&format!(".{f:0width$}", width = scale as usize));
        }
    }
    s
}

fn time_text(t: Time) -> String {
    let unit = 10u64.pow(t.scale() as u32);
    let inc = t.increments();
    clock(inc / unit, Some((inc % unit, t.scale())))
}

/// `e` format the way SQL Server's style 126 writes floats: sign and three exponent digits.
fn sci(v: f64, digits: usize) -> String {
    let s = format!("{v:.digits$e}");
    let (mant, exp) = s.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    format!("{mant}e{}{:03}", if exp < 0 { '-' } else { '+' }, exp.abs())
}

fn dt2_text(dt: DateTime2, offset_minutes: Option<i16>) -> String {
    let t = dt.time();
    let unit = 10u64.pow(t.scale() as u32);
    let mut days = dt.date().days() as i64;
    let mut inc = t.increments() as i64;
    let mut suffix = String::new();
    if let Some(off) = offset_minutes {
        // tiberius hands over UTC; style 126 writes the local time.
        inc += off as i64 * 60 * unit as i64;
        let day = 86400 * unit as i64;
        days += inc.div_euclid(day);
        inc = inc.rem_euclid(day);
        suffix = format!("{}{:02}:{:02}", if off < 0 { '-' } else { '+' }, off.abs() / 60, off.abs() % 60);
    }
    let unit = unit as i64;
    format!("{}T{}{suffix}", date_text(days), clock((inc / unit) as u64, Some(((inc % unit) as u64, t.scale()))))
}

/// The adapter's rendering rule per `ColumnData` variant (style 126 text).
fn render(d: &ColumnData<'_>) -> Option<String> {
    Some(match d {
        ColumnData::U8(v) => v.as_ref()?.to_string(),
        ColumnData::I16(v) => v.as_ref()?.to_string(),
        ColumnData::I32(v) => v.as_ref()?.to_string(),
        ColumnData::I64(v) => v.as_ref()?.to_string(),
        ColumnData::Bit(v) => (if *v.as_ref()? { "1" } else { "0" }).into(),
        ColumnData::Numeric(v) => {
            let n = v.as_ref()?;
            // `Display` writes `5.0` for scale 0; the server writes `5`.
            if n.scale() == 0 { n.value().to_string() } else { n.to_string() }
        }
        // money and smallmoney arrive as F64 (is_money is told apart by the column type; here: 4 decimals).
        ColumnData::F64(v) => v.map(|v| if MONEY.with(|m| m.get()) { format!("{v:.4}") } else { sci(v, 15) })?,
        ColumnData::F32(v) => sci(*v.as_ref()? as f64, 7),
        ColumnData::String(v) => v.as_ref()?.to_string(),
        ColumnData::Guid(v) => v.as_ref()?.to_string().to_uppercase(),
        ColumnData::Binary(v) => format!("0x{}", v.as_ref()?.iter().map(|b| format!("{b:02X}")).collect::<String>()),
        ColumnData::Xml(v) => v.as_ref()?.as_ref().to_string(),
        ColumnData::Date(v) => date_text((*v.as_ref()?).days() as i64),
        ColumnData::Time(v) => time_text(*v.as_ref()?),
        ColumnData::DateTime2(v) => dt2_text(*v.as_ref()?, None),
        ColumnData::DateTimeOffset(v) => {
            let v = v.as_ref()?;
            dt2_text(v.datetime2(), Some(v.offset()))
        }
        ColumnData::DateTime(v) => {
            let v = v.as_ref()?;
            let frag = v.seconds_fragments() as u64;
            let (mut secs, mut ms) = (frag / 300, ((frag % 300) * 10 + 1) / 3);
            if ms >= 1000 { secs += 1; ms -= 1000; }
            // 1900-01-01 is day 693595 since 0001-01-01
            format!("{}T{}", date_text(693595 + v.days() as i64), clock(secs, Some((ms, 3))))
        }
        ColumnData::SmallDateTime(v) => {
            let v = v.as_ref()?;
            let minutes = v.seconds_fragments() as u64;
            format!("{}T{}", date_text(693595 + v.days() as i64), clock(minutes * 60, None))
        }
    })
}

thread_local! { static MONEY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

/// Compare `render` with the server's `as_text` for every type and some edge values.
async fn render_check() {
    let as_text = "coalesce(try_convert(nvarchar(max), x, 126), convert(nvarchar(max), x))";
    let mut cases: Vec<(&str, String)> = Vec::new();
    let mut add = |ty: &'static str, vals: &[&str]| {
        for v in vals {
            cases.push((ty, format!("cast({v} as {ty})")));
        }
    };
    add("tinyint", &["0", "255", "null"]);
    add("smallint", &["-32768", "32767"]);
    add("int", &["-2147483648", "2147483647", "0", "null"]);
    add("bigint", &["-9223372036854775808", "9223372036854775807"]);
    add("bit", &["0", "1", "null"]);
    add("decimal(10,2)", &["0", "-0.05", "12345.60", "99999999.99", "null"]);
    add("decimal(18,0)", &["5", "-123456789012345678"]);
    add("decimal(38,10)", &["1234567890123456789.1234567891", "-0.0000000001", "9999999999999999999999999999.9999999999"]);
    add("money", &["0", "1234.5", "-0.0001", "1000000.5", "99999999999.9999", "922337203685477.5807", "-922337203685477.5808", "null"]);
    add("smallmoney", &["214748.3647", "-214748.3648", "12.5"]);
    add("float", &["0", "0.1", "-1.5e-300", "1.7976931348623157e308", "123456789.123456789", "1e15", "1e16", "2.2250738585072014e-308", "null"]);
    add("real", &["0", "0.1", "3.4e38", "-1.5e-10", "123456.789", "16777216", "null"]);
    add("date", &["'0001-01-01'", "'1900-01-01'", "'1970-01-01'", "'2000-02-29'", "'9999-12-31'", "null"]);
    for scale in [0, 1, 3, 7] {
        let ty: &'static str = ["time(0)", "time(1)", "time(3)", "time(7)"][[0, 1, 3, 7].iter().position(|s| *s == scale).unwrap()];
        add(ty, &["'00:00:00'", "'13:45:09'", "'23:59:59.9999999'", "null"]);
    }
    add("smalldatetime", &["'1900-01-01 00:00'", "'2026-10-21 13:45'", "'2079-06-06 23:59'", "null"]);
    add("datetime", &["'1753-01-01 00:00:00.000'", "'2026-10-21 13:45:09.003'", "'2026-10-21 13:45:09.007'", "'2026-10-21 13:45:09.010'", "'2026-10-21 13:45:09.013'", "'2026-10-21 13:45:09.997'", "'9999-12-31 23:59:59.997'", "'1900-01-01'", "null"]);
    add("datetime2(0)", &["'0001-01-01 00:00:00'", "'2026-10-21 13:45:09'", "'9999-12-31 23:59:59'"]);
    add("datetime2(3)", &["'2026-10-21 13:45:09.120'"]);
    add("datetime2(7)", &["'0001-01-01 00:00:00.0000000'", "'2026-10-21 13:45:09.1234567'", "'9999-12-31 23:59:59.9999999'", "null"]);
    add("datetimeoffset(0)", &["'2026-10-21 13:45:09 +00:00'", "'2026-10-21 13:45:09 +14:00'", "'2026-10-21 13:45:09 -12:00'"]);
    add("datetimeoffset(7)", &["'2026-10-21 13:45:09.1234567 +05:30'", "'2026-01-01 00:30:00.5 +05:30'", "'2026-12-31 23:30:00.5 -05:30'", "'0001-01-01 14:00:00 +14:00'", "null"]);
    add("uniqueidentifier", &["'6f9619ff-8b86-d011-b42d-00c04fc964ff'", "'00000000-0000-0000-0000-000000000000'", "null"]);
    add("nvarchar(50)", &["N''", "N'héllo ''q'' ✓ 😀'", "null"]);
    add("char(5)", &["'ab'"]);
    add("varchar(20)", &["'abc'"]);
    add("nvarchar(max)", &["N'long'"]);
    add("xml", &["'<a x=\"1\"><b/></a>'", "'<r/>'", "null"]);
    add("varbinary(10)", &["0x", "0x00FF10", "null"]);

    let mut c = connect().await;
    let (mut ok, mut bad) = (0, 0);
    for (ty, expr) in &cases {
        let is_binary = ty.starts_with("varbinary");
        let server = if is_binary { "convert(nvarchar(max), x, 1)" } else { as_text };
        let sql = format!("select x, {server} from (select {expr} as x) s");
        let row = c.simple_query(&sql).await.unwrap().into_row().await.unwrap().unwrap();
        let cells: Vec<_> = row.into_iter().collect();
        MONEY.with(|m| m.set(ty.contains("money")));
        let mine = render(&cells[0]);
        let theirs = match &cells[1] { ColumnData::String(s) => s.as_ref().map(|s| s.to_string()), _ => panic!() };
        let theirs = if is_binary && theirs.as_deref() == Some("") { theirs } else { theirs };
        if mine == theirs {
            ok += 1;
        } else {
            bad += 1;
            println!("MISMATCH {ty:18} {expr}\n    adapter={mine:?}\n    server ={theirs:?}\n    tiberius={:?}", cells[0]);
        }
    }
    println!("{ok} equal, {bad} different, of {}", ok + bad);
}
