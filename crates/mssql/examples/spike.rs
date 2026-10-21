//! Throwaway spike: cancel, write detection, value text and TLS against the dev server.
//! Run: `cargo run -p kuery-mssql --example spike -- [cancel|socket|write|values|parse]`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

type Conn = Client<Compat<TcpStream>>;

fn config() -> Config {
    let mut c = Config::new();
    c.host("localhost");
    c.port(14339);
    c.authentication(AuthMethod::sql_server("sa", "Kuery_dev_2026!"));
    c.trust_cert();
    c.encryption(EncryptionLevel::Required);
    c
}

async fn connect() -> Conn {
    let c = config();
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
        "tls" => {
            let mut c = connect().await;
            println!("encrypted: {}", scalar(&mut c, "select encrypt_option from sys.dm_exec_connections where session_id = @@spid").await);
            println!("version: {}", scalar(&mut c, "select @@version").await);
        }
        "parse" => parse(),
        _ => eprintln!("usage: spike cancel|socket|write|values|parse"),
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

fn parse() {
    use sqlparser::dialect::MsSqlDialect;
    use sqlparser::parser::Parser;
    for sql in ["waitfor delay '00:00:20'", "waitfor delay '00:00:20'; select 1", "select 1"] {
        println!("{sql:45} -> {:?}", Parser::parse_sql(&MsSqlDialect {}, sql));
    }
}
