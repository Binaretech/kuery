//! SQL Server driver: an [`Adapter`] over one TDS connection.
//!
//! The rules (read-only in two layers, write mode, previews, row edits,
//! `GO` batches, cancelling) are the session's ([`kuery_core::session`]);
//! this crate connects, runs one batch at a time and streams its result sets
//! (values in the text `as_text` gives, see [`value`]), and stops a running
//! batch by dropping the connection and opening a new one in its place: TDS
//! attention leaves the client out of step with the server. It waits on a
//! tokio runtime of its own.
//!
//! SQL Server cannot make a transaction read-only. While the session's
//! read-only transaction is open, every batch is followed by a look at the
//! transaction log records it wrote: any write is rolled back and refused.
//! That look needs `VIEW SERVER PERFORMANCE STATE`; without it the rollback
//! at the end still undoes everything, and [`Adapter::notice`] says so.

mod activity;
mod catalog;
mod value;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::TryStreamExt as _;
use kuery_core::activity::{Activity, Signal};
use kuery_core::adapter::{Adapter, Canceller, RowSink};
use kuery_core::catalog::{
    CatalogColumn, CatalogOverview, ForeignKey, FunctionInfo, TableDefinition, TableRef,
};
use kuery_core::driver::{ConnectOptions, DriverInfo, Family, Field, SqlDriver, Tls};
use kuery_core::result_set::ColumnKind;
use kuery_core::script;
use kuery_core::session::{Access, QueryError, RunOptions};
use kuery_core::sql::{Dialect, MSSQL};
use tiberius::{AuthMethod, Client, ColumnType, Config, EncryptionLevel, QueryItem, SqlBrowser};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio::sync::Notify;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt as _};

type Connection = Client<Compat<TcpStream>>;

/// The runtime every SQL Server connection waits on.
fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("kuery-mssql")
            .enable_all()
            .build()
            .expect("start the SQL Server runtime")
    })
}

/// How the connection uses TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// Encrypted when the server offers it, checking its certificate.
    Default,
    /// Never encrypted, not even the login.
    Disable,
    /// Always encrypted, checking the server's certificate.
    Require,
    /// Always encrypted, without checking the certificate (SQL Server's own
    /// self-signed one, say).
    TrustServerCertificate,
}

/// Where and how to connect.
#[derive(Clone)]
pub struct Target {
    pub host: String,
    pub port: u16,
    /// A named instance, whose port the SQL Browser service tells.
    pub instance: Option<String>,
    pub user: String,
    pub password: Option<String>,
    /// The database to start in; empty for the login's default.
    pub database: String,
    pub tls: TlsMode,
    /// The name the certificate must carry: the server as named, even
    /// when `host` is the local end of a tunnel.
    pub server_name: String,
}

impl Target {
    fn config(&self) -> Config {
        let mut config = Config::new();
        config.host(&self.host);
        config.port(self.port);
        config.hostname_in_certificate(&self.server_name);
        if let Some(instance) = &self.instance {
            config.instance_name(instance);
        }
        config.authentication(AuthMethod::sql_server(
            &self.user,
            self.password.as_deref().unwrap_or_default(),
        ));
        if !self.database.is_empty() {
            config.database(&self.database);
        }
        config.application_name("Kuery");
        config.encryption(match self.tls {
            TlsMode::Default => EncryptionLevel::On,
            TlsMode::Disable => EncryptionLevel::NotSupported,
            TlsMode::Require | TlsMode::TrustServerCertificate => EncryptionLevel::Required,
        });
        if self.tls == TlsMode::TrustServerCertificate {
            config.trust_cert();
        }
        config
    }

    /// `host:port`, or `host\instance`, for messages.
    fn address(&self) -> String {
        match &self.instance {
            Some(instance) => format!("{}\\{instance}", self.host),
            None => format!("{}:{}", self.host, self.port),
        }
    }
}

/// SQL Server, for Kuery's driver list.
pub static DRIVER: SqlDriver = SqlDriver {
    info: DriverInfo {
        id: "mssql",
        name: "SQL Server",
        aliases: &["sqlserver", "mssql", "tsql", "microsoft"],
        family: Family::Sql,
        default_port: 1433,
        database: Field {
            label: "Database",
            placeholder: "shop",
            required: false,
            numeric: false,
            help: Some("The database queries start in; empty for the login's default database."),
        },
        user: Field {
            label: "User",
            placeholder: "sa",
            required: true,
            numeric: false,
            help: None,
        },
        tls: Tls::Modes(&["disable", "require", "trust server certificate"]),
        production_note: "Production connections open read-only. SQL Server cannot refuse writes by itself: Kuery blocks them and undoes any that slip through; for a guarantee, use a login with only db_datareader.",
        file: false,
        host_help: Some(
            "A named instance is HOST\\INSTANCE; it is found through the SQL Browser service (UDP 1434). Through an SSH tunnel, use the instance's fixed port instead.",
        ),
    },
    dialect: &MSSQL,
    open: |options, access| open(&Target::from(options), access),
};

impl From<&ConnectOptions> for Target {
    /// `HOST\INSTANCE` names an instance. Through the local end of a tunnel
    /// when there is one, which reaches one port: no instance lookup.
    fn from(options: &ConnectOptions) -> Self {
        let endpoint = options.endpoint();
        let (server_name, instance) = match endpoint.server_name.split_once('\\') {
            Some((host, instance)) => (host.trim().to_string(), Some(instance.trim().to_string())),
            None => (endpoint.server_name.clone(), None),
        };
        let tunnelled = options.tunnel.is_some();
        Target {
            host: match tunnelled {
                true => endpoint.host,
                false => server_name.clone(),
            },
            port: endpoint.port,
            instance: instance.filter(|i| !i.is_empty() && !tunnelled),
            user: options.user.trim().to_string(),
            password: options.password.clone().filter(|p| !p.is_empty()),
            database: options.database.trim().to_string(),
            tls: match options.tls.as_deref() {
                Some("disable") => TlsMode::Disable,
                Some("require") => TlsMode::Require,
                Some("trust server certificate") => TlsMode::TrustServerCertificate,
                _ => TlsMode::Default,
            },
            server_name,
        }
    }
}

/// Log records the transaction wrote outside tempdb, less the begin record per database: more than zero means it wrote.
/// Query Store can log just that begin record during a read; every write logs at least one more.
const WRITES: &str = "(select coalesce(sum(database_transaction_log_record_count - 1), 0) \
     from sys.dm_tran_database_transactions \
     where transaction_id = (select transaction_id from sys.dm_tran_current_transaction) \
       and database_id <> 2 and database_transaction_log_record_count > 0)";

/// 1 when the login can only read the current database, anything else (or an error) when it may write.
/// Only used when the writes check cannot run, to decide whether to show the notice. Fails open: any doubt shows it.
const READS_ONLY: &str = "with token as ( \
         select principal_id from sys.user_token \
         union select database_principal_id() union select 0) \
     select case when user_name() <> 'dbo' \
       and is_member('db_owner') = 0 \
       and is_srvrolemember('sysadmin') = 0 \
       and not exists (select 1 from sys.user_token t \
                       where t.type = 'ROLE' and t.name not in ('public', 'db_datareader')) \
       and not exists (select 1 from sys.database_role_members m \
                       join sys.database_principals r on r.principal_id = m.role_principal_id \
                       where m.member_principal_id in (select principal_id from token) \
                         and r.name not in ('public', 'db_datareader')) \
       and not exists (select 1 from sys.schemas s \
                       where s.principal_id in (select principal_id from token) \
                         and not (s.schema_id between 16384 and 16393 \
                                  and not exists (select 1 from sys.objects o \
                                                  where o.schema_id = s.schema_id))) \
       and not exists (select 1 from sys.objects o \
                       where o.principal_id in (select principal_id from token)) \
       and not exists (select 1 from sys.database_permissions p \
                       where p.grantee_principal_id in (select principal_id from token) \
                         and p.state in ('G', 'W') \
                         and (p.permission_name in ('INSERT', 'UPDATE', 'DELETE', 'ALTER', 'CONTROL', \
                                                    'TAKE OWNERSHIP', 'EXECUTE', 'IMPERSONATE') \
                              or p.permission_name like 'CREATE %' \
                              or p.permission_name like 'ALTER ANY %')) \
       and not exists (select 1 from fn_my_permissions(null, 'SERVER') \
                       where permission_name in ('CONTROL SERVER', 'ALTER ANY DATABASE', \
                                                 'IMPERSONATE ANY LOGIN', 'ALTER ANY LOGIN', \
                                                 'ALTER ANY SERVER ROLE')) \
       and not exists (select 1 from sys.login_token t \
                       where t.type = 'SERVER ROLE' and t.name <> 'public') \
       and not exists (select 1 from sys.server_role_members m \
                       join sys.server_principals r on r.principal_id = m.role_principal_id \
                       where m.member_principal_id = suser_id() and r.name <> 'public') \
       and is_srvrolemember('securityadmin') = 0 \
       and is_srvrolemember('serveradmin') = 0 \
       and is_srvrolemember('setupadmin') = 0 \
       and is_srvrolemember('processadmin') = 0 \
       and is_srvrolemember('diskadmin') = 0 \
       and is_srvrolemember('dbcreator') = 0 \
       and is_srvrolemember('bulkadmin') = 0 \
       and not exists (select 1 from sys.server_permissions p \
                       where (p.grantee_principal_id = suser_id() \
                              or p.grantee_principal_id in (select principal_id from sys.login_token)) \
                         and p.state in ('G', 'W') \
                         and ((p.class = 101 and p.permission_name in ('IMPERSONATE', 'CONTROL')) \
                              or p.permission_name in ('CONTROL SERVER', 'ALTER ANY DATABASE', \
                                                       'IMPERSONATE ANY LOGIN', 'ALTER ANY LOGIN', \
                                                       'ALTER ANY SERVER ROLE'))) \
     then 1 else 0 end";

/// Why the writes check cannot run, as the connection's notice.
const UNDETECTED: &str = "Read-only is enforced by Kuery's checks and automatic undo only: grant VIEW SERVER PERFORMANCE STATE to this login, or use a login with only db_datareader, for writes to be refused.";

/// The isolation level of read transactions on a database that allows
/// snapshots, and the one writes (and everything else) run at.
const SNAPSHOT: &str = "set transaction isolation level snapshot";
const READ_COMMITTED: &str = "set transaction isolation level read committed";

/// What a dropped connection takes with it inside a write transaction.
const LOST: &str = "the open transaction and its uncommitted changes were lost";

/// One open connection.
pub struct Mssql {
    /// `None` after a connection was lost and could not be opened again.
    connection: Option<Connection>,
    target: Target,
    server: String,
    /// Whether the writes check can run (see [`UNDETECTED`]).
    detects_writes: bool,
    /// Whether the login may write in the database it opened in (see
    /// [`READS_ONLY`]); without the writes check, only such a login gets the
    /// notice.
    may_write: bool,
    reading: bool,
    /// The database allows snapshot isolation: reads use it, so they do not
    /// wait for other sessions' uncommitted rows.
    snapshot: bool,
    /// The database (`db_id()`) `snapshot` is about; a batch that moves to
    /// another (`use`) has it read again.
    database: i64,
    /// The open read transaction runs at snapshot isolation, to be set back
    /// when it ends.
    snapshot_read: bool,
    /// `@@trancount` after the last batch.
    transactions: i64,
    /// A `set showplan_xml on` (or the like) is in force: batches return
    /// plans instead of running, the after-batch check included.
    plans_only: bool,
    /// Replayed on every new connection.
    lock_timeout: Option<u128>,
    statement_timeout: Option<Duration>,
    stop: Arc<Stop>,
}

/// A cancel request for the batch that runs now.
#[derive(Default)]
struct Stop {
    running: AtomicBool,
    requested: AtomicBool,
    notify: Notify,
}

impl Stop {
    fn start(&self) {
        self.requested.store(false, Ordering::SeqCst);
        self.running.store(true, Ordering::SeqCst);
    }

    fn end(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Stops the running batch; nothing when none runs.
    fn request(&self) {
        if self.running.load(Ordering::SeqCst) {
            self.requested.store(true, Ordering::SeqCst);
            self.notify.notify_waiters();
        }
    }

    async fn requested(&self) {
        loop {
            // Made before the check, so a request in between still wakes it.
            let notified = self.notify.notified();
            if self.requested.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

/// Connects. Read-only connections need nothing more: the session's
/// transactions and the writes check do the work.
pub fn open(target: &Target, _access: Access) -> Result<Box<dyn Adapter>, String> {
    runtime().block_on(async {
        let mut connection = connect(target, None).await?;
        let version = first_row(
            &mut connection,
            "select cast(serverproperty('ProductVersion') as nvarchar(128)), \
                    cast(serverproperty('EngineEdition') as int)",
        )
        .await
        .map_err(|error| error.to_string())?;
        let detects_writes = probe_verdict(
            first_row(&mut connection, &format!("select {WRITES}"))
                .await
                .map(|_| ()),
        )?;
        // Any error or doubt: it may write.
        let may_write = detects_writes
            || first_row(&mut connection, READS_ONLY)
                .await
                .map_or(true, |row| {
                    row.first().cloned().flatten().as_deref() != Some("1")
                });
        let (database, snapshot) = database_state(&mut connection).await?;
        Ok(Box::new(Mssql {
            connection: Some(connection),
            target: target.clone(),
            server: server_label(
                version
                    .first()
                    .cloned()
                    .flatten()
                    .unwrap_or_default()
                    .as_str(),
                version
                    .get(1)
                    .cloned()
                    .flatten()
                    .unwrap_or_default()
                    .as_str(),
            ),
            detects_writes,
            may_write,
            reading: false,
            snapshot,
            database,
            snapshot_read: false,
            transactions: 0,
            plans_only: false,
            lock_timeout: None,
            statement_timeout: None,
            stop: Arc::default(),
        }) as Box<dyn Adapter>)
    })
}

/// A new connection, with the session settings Kuery relies on.
async fn connect(target: &Target, lock_timeout: Option<u128>) -> Result<Connection, String> {
    let config = target.config();
    let opening = async {
        let tcp = match target.instance {
            Some(_) => TcpStream::connect_named(&config).await?,
            None => TcpStream::connect(config.get_addr()).await?,
        };
        tcp.set_nodelay(true)?;
        Client::connect(config, tcp.compat_write()).await
    };
    let mut connection = tokio::time::timeout(Duration::from_secs(15), opening)
        .await
        .map_err(|_| format!("{} did not answer in 15 seconds", target.address()))?
        .map_err(|error| connect_error(error, target.tls))?;
    let mut settings = "set xact_abort off; set nocount off".to_string();
    if let Some(millis) = lock_timeout {
        settings.push_str(&format!("; set lock_timeout {millis}"));
    }
    connection
        .simple_query(settings)
        .await
        .map_err(|error| error.to_string())?
        .into_results()
        .await
        .map_err(|error| error.to_string())?;
    Ok(connection)
}

/// The current database's `db_id()`, and whether it allows snapshot
/// isolation (`allow_snapshot_isolation on`; the database's default level is
/// not touched).
async fn database_state(connection: &mut Connection) -> Result<(i64, bool), String> {
    let row = first_row(
        connection,
        "select db_id(), snapshot_isolation_state from sys.databases where database_id = db_id()",
    )
    .await
    .map_err(|error| error.to_string())?;
    let id = row
        .first()
        .cloned()
        .flatten()
        .and_then(|id| id.parse().ok())
        .unwrap_or(0);
    Ok((id, row.get(1).cloned().flatten().as_deref() == Some("1")))
}

/// Whether the writes check can run, from the probe's result: a login
/// without the permission only loses the check; any other failure is an error
/// (the connection must not open with read-only silently weaker).
fn probe_verdict(probe: Result<(), tiberius::error::Error>) -> Result<bool, String> {
    match probe {
        Ok(()) => Ok(true),
        Err(tiberius::error::Error::Server(server)) if lacks_permission(server.code()) => Ok(false),
        Err(error) => Err(format!("Could not check for writes: {error}")),
    }
}

/// SQL Server errors 300, 297, 262 and 229: the login may not read what
/// the writes check reads.
fn lacks_permission(code: u32) -> bool {
    matches!(code, 300 | 297 | 262 | 229)
}

fn connect_error(error: tiberius::error::Error, tls: TlsMode) -> String {
    match error {
        tiberius::error::Error::Tls(_) if matches!(tls, TlsMode::Default | TlsMode::Require) => {
            "The server's certificate is not trusted (SQL Server's own self-signed certificate never is): \
             choose TLS \"trust server certificate\" to connect anyway."
                .to_string()
        }
        tiberius::error::Error::Server(server) => server.message().to_string(),
        other => other.to_string(),
    }
}

/// `SQL Server 2022 (16.0.4305.5)`.
fn server_label(version: &str, edition: &str) -> String {
    let name = match edition {
        "5" => return format!("Azure SQL Database ({version})"),
        "8" => return format!("Azure SQL Managed Instance ({version})"),
        _ => match version.split('.').next().unwrap_or_default() {
            "11" => "2012",
            "12" => "2014",
            "13" => "2016",
            "14" => "2017",
            "15" => "2019",
            "16" => "2022",
            "17" => "2025",
            _ => return format!("SQL Server {version}"),
        },
    };
    format!("SQL Server {name} ({version})")
}

/// The first row of a statement's first result, as text.
async fn first_row(
    connection: &mut Connection,
    sql: &str,
) -> Result<Vec<Option<String>>, tiberius::error::Error> {
    let row = connection.simple_query(sql).await?.into_row().await?;
    Ok(row
        .map(|row| {
            row.cells()
                .map(|(column, data)| value::text(data, column.column_type()))
                .collect()
        })
        .unwrap_or_default())
}

/// A SQL Server error as a [`QueryError`].
fn query_error(error: tiberius::error::Error) -> QueryError {
    match error {
        tiberius::error::Error::Server(server) => QueryError::Database {
            code: server.code().to_string(),
            message: match server.code() {
                1222 => "Waited too long for a lock".into(),
                _ => server.message().to_string(),
            },
        },
        other => QueryError::Connection(other.to_string()),
    }
}

/// Streams every result set of `sql` to `rows`; whether there was any.
async fn stream(
    connection: &mut Connection,
    sql: &str,
    rows: &mut dyn RowSink,
) -> Result<bool, QueryError> {
    let mut results = connection.simple_query(sql).await.map_err(query_error)?;
    let mut types: Vec<ColumnType> = Vec::new();
    let mut any = false;
    while let Some(item) = results.try_next().await.map_err(query_error)? {
        match item {
            QueryItem::Metadata(metadata) => {
                any = true;
                let columns = metadata.columns();
                types = columns.iter().map(|c| c.column_type()).collect();
                let kinds: Vec<ColumnKind> = types.iter().map(|t| value::kind(*t)).collect();
                let names = columns.iter().map(|c| c.name().to_string()).collect();
                rows.columns(names, Some(kinds))?;
            }
            QueryItem::Row(row) => {
                let values: Vec<Option<String>> = row
                    .cells()
                    .zip(&types)
                    .map(|((_, data), column)| value::text(data, *column))
                    .collect();
                let borrowed: Vec<Option<&str>> = values.iter().map(Option::as_deref).collect();
                // On an error the rest is read and dropped before the next
                // statement.
                rows.row(&borrowed)?;
            }
        }
    }
    Ok(any)
}

/// What a batch left behind: rows changed by its last statement,
/// `@@trancount`, the database it ended in, and the log records written
/// when asked.
struct After {
    changed: u64,
    transactions: i64,
    database: i64,
    writes: Option<i64>,
}

async fn after_batch(connection: &mut Connection, check_writes: bool) -> Result<After, QueryError> {
    let sql = match check_writes {
        true => format!("select @@rowcount, @@trancount, db_id(), {WRITES}"),
        false => "select @@rowcount, @@trancount, db_id()".into(),
    };
    let row = first_row(connection, &sql).await.map_err(query_error)?;
    let number = |index: usize| -> i64 {
        row.get(index)
            .cloned()
            .flatten()
            .and_then(|text| text.parse().ok())
            .unwrap_or(0)
    };
    Ok(After {
        changed: number(0).max(0) as u64,
        transactions: number(1),
        database: number(2),
        writes: check_writes.then(|| number(3)),
    })
}

enum Ended {
    Ran(Result<bool, QueryError>, Result<After, QueryError>),
    Cancelled,
    TimedOut,
}

impl Mssql {
    /// The connection, opening a new one if the last was lost.
    fn connection(&mut self) -> Result<&mut Connection, QueryError> {
        if self.connection.is_none() {
            self.reconnect()?;
        }
        Ok(self.connection.as_mut().expect("just connected"))
    }

    /// Drops the connection (and with it the server session, its
    /// transaction and anything running) and opens a new one.
    fn reconnect(&mut self) -> Result<(), QueryError> {
        self.connection = None;
        self.reading = false;
        self.transactions = 0;
        self.plans_only = false;
        self.snapshot_read = false;
        let (connection, (database, snapshot)) = runtime()
            .block_on(async {
                let mut connection = connect(&self.target, self.lock_timeout).await?;
                let state = database_state(&mut connection).await?;
                Ok::<_, String>((connection, state))
            })
            .map_err(QueryError::Connection)?;
        self.connection = Some(connection);
        (self.database, self.snapshot) = (database, snapshot);
        Ok(())
    }

    /// Whether dropping the connection now would lose a write transaction:
    /// one open before `sql`, or one `sql` may open itself.
    fn would_lose_writes(&self, sql: &str) -> bool {
        (self.transactions > 0 && !self.reading) || opens_transaction(sql)
    }

    /// Runs one batch, watching for cancel, timeout and writes.
    fn run_batch(&mut self, sql: &str, rows: &mut dyn RowSink) -> Result<u64, QueryError> {
        let begins_reading = MSSQL
            .transactions
            .begin_read_only
            .iter()
            .any(|begin| sql.trim().eq_ignore_ascii_case(begin));
        let check_writes = self.detects_writes && self.reading && !begins_reading;
        let loses_writes = !begins_reading && self.would_lose_writes(sql);
        // Write transactions never time out: that would drop them.
        let timeout = self.statement_timeout.filter(|_| !loses_writes);
        let stop = self.stop.clone();
        let was_reading = self.reading;
        // Under a plan mode the after-batch check would only return its
        // plan; nothing ran, so nothing changed.
        let plans_only = plan_mode(sql).unwrap_or(self.plans_only);
        let unchanged = After {
            changed: 0,
            transactions: self.transactions,
            database: self.database,
            writes: None,
        };
        // The read transaction starts at snapshot isolation when the
        // database allows it; the level lasts until the transaction ends.
        let sql = match begins_reading && self.snapshot {
            true => format!("{SNAPSHOT}; {sql}"),
            false => sql.to_string(),
        };
        let sql = sql.as_str();
        let connection = self.connection()?;

        stop.start();
        let ended = runtime().block_on(async {
            let run = async {
                let streamed = stream(connection, sql, rows).await;
                // Also after an error: a batch may have written before it.
                let after = match &streamed {
                    Err(QueryError::Database { .. }) | Ok(_) if plans_only => Ok(unchanged),
                    Err(QueryError::Database { .. }) | Ok(_) => {
                        after_batch(connection, check_writes).await
                    }
                    Err(QueryError::Cancelled) if loses_writes => {
                        after_batch(connection, check_writes).await
                    }
                    Err(_) => Err(QueryError::Cancelled),
                };
                Ended::Ran(streamed, after)
            };
            let timed_out = async {
                match timeout {
                    Some(limit) => tokio::time::sleep(limit).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                ended = run => ended,
                () = stop.requested() => Ended::Cancelled,
                () = timed_out => Ended::TimedOut,
            }
        });
        stop.end();

        let (streamed, after) = match ended {
            // The reader went away: stop the statement so the next query does not have to read the rest.
            Ended::Ran(Err(QueryError::Cancelled), _) if !loses_writes => {
                self.reconnect().map_err(|error| {
                    QueryError::Connection(format!("Stopped, and could not reconnect: {error}"))
                })?;
                return Err(QueryError::Cancelled);
            }
            Ended::Ran(streamed, after) => (streamed, after),
            stopped => {
                self.reconnect().map_err(|error| {
                    QueryError::Connection(format!("Stopped, and could not reconnect: {error}"))
                })?;
                return Err(match stopped {
                    // Never timed out (see above): cancelled.
                    _ if loses_writes => QueryError::Connection(format!("Cancelled; {LOST}")),
                    Ended::TimedOut => QueryError::Database {
                        code: "timeout".into(),
                        message: "The statement ran out of time".into(),
                    },
                    _ => QueryError::Cancelled,
                });
            }
        };
        let after = match after {
            Ok(after) => after,
            Err(error) => {
                let lost = matches!(error, QueryError::Connection(_))
                    || matches!(streamed, Err(QueryError::Connection(_)));
                if lost {
                    // Opened again for the next batch.
                    self.connection = None;
                    self.reading = false;
                    self.transactions = 0;
                }
                return Err(streamed.err().unwrap_or(error));
            }
        };

        self.plans_only = plans_only;
        self.transactions = after.transactions;
        if begins_reading {
            self.reading = true;
            self.snapshot_read = self.snapshot;
        }
        // Any rollback ends the read-only transaction.
        if after.transactions == 0 {
            self.reading = false;
        }
        if after.database != self.database {
            let connection = self.connection()?;
            (self.database, self.snapshot) = runtime()
                .block_on(database_state(connection))
                .map_err(QueryError::Connection)?;
        }
        if was_reading && !self.reading && self.snapshot_read {
            self.snapshot_read = false;
            let connection = self.connection()?;
            runtime()
                .block_on(async {
                    connection
                        .simple_query(READ_COMMITTED)
                        .await?
                        .into_results()
                        .await
                })
                .map_err(query_error)?;
        }
        if after.writes.is_some_and(|writes| writes > 0) {
            let undo = format!(
                "rollback transaction; {}",
                MSSQL.transactions.begin_read_only.join("; ")
            );
            let connection = self.connection()?;
            runtime()
                .block_on(async { connection.simple_query(undo).await?.into_results().await })
                .map_err(query_error)?;
            self.transactions = 1;
            return Err(QueryError::WritesDisabled);
        }
        // Rows changed, as `@@rowcount` tells for the last statement; none for
        // a batch that returns rows.
        streamed.map(|returned| if returned { 0 } else { after.changed })
    }
}

/// The plan mode a batch turns on (`Some(true)`) or off: `set showplan_xml`,
/// `showplan_all`, `showplan_text` or `noexec`, under which statements are
/// compiled but not run.
fn plan_mode(batch: &str) -> Option<bool> {
    let batch = batch.trim().trim_end_matches(';').to_lowercase();
    let words: Vec<&str> = batch.split_whitespace().collect();
    match words.as_slice() {
        [
            "set",
            "showplan_xml" | "showplan_all" | "showplan_text" | "noexec",
            state,
        ] => match *state {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Whether a batch is a `set … off`, which ends a mode the batches before it
/// turned on (`set showplan_xml off`).
fn turns_off(batch: &str) -> bool {
    let batch = batch.trim().to_lowercase();
    batch.starts_with("set ") && batch.ends_with(" off")
}

/// Whether a batch may begin a transaction of its own.
fn opens_transaction(sql: &str) -> bool {
    let sql = sql.to_lowercase();
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|pair| pair[0] == "begin" && pair[1].starts_with("tran"))
}

impl Adapter for Mssql {
    fn dialect(&self) -> &'static Dialect {
        &MSSQL
    }

    fn server(&self) -> String {
        self.server.clone()
    }

    fn notice(&self) -> Option<String> {
        (!self.detects_writes && self.may_write).then(|| UNDETECTED.to_string())
    }

    /// One call may hold several `GO`-separated batches (the plan text
    /// does); they run in order on this connection, and the last one's
    /// outcome is the call's.
    fn execute(&mut self, sql: &str, rows: &mut dyn RowSink) -> Result<u64, QueryError> {
        let ranges = script::statements_in(&MSSQL, sql).unwrap_or_default();
        let texts: Vec<&str> = match ranges.len() {
            0 => return self.run_batch(sql, rows),
            _ => ranges.iter().map(|range| &sql[range.clone()]).collect(),
        };
        let mut outcome = Ok(0);
        for (index, text) in texts.iter().enumerate() {
            outcome = self.run_batch(text, rows);
            if outcome.is_err() {
                // A failed batch must not leave a plan mode on that a later `set ... off` batch was to end.
                let reconnected = matches!(
                    outcome,
                    Err(QueryError::Cancelled | QueryError::Connection(_))
                );
                if let Some(last) = texts[index + 1..].last().filter(|t| turns_off(t))
                    && !reconnected
                {
                    let _ = self.run_batch(last, &mut kuery_core::adapter::Discard);
                }
                break;
            }
        }
        outcome
    }

    fn limit(&mut self, options: &RunOptions) -> Result<(), QueryError> {
        let millis = options.lock_timeout.as_millis();
        self.lock_timeout = Some(millis);
        self.statement_timeout = Some(options.statement_timeout).filter(|t| !t.is_zero());
        self.execute(
            &format!("set lock_timeout {millis}"),
            &mut kuery_core::adapter::Discard,
        )
        .map(|_| ())
    }

    fn canceller(&self) -> Canceller {
        let stop = self.stop.clone();
        Arc::new(move || stop.request())
    }

    fn overview(&mut self) -> Result<CatalogOverview, QueryError> {
        catalog::overview(self)
    }

    fn definition(&mut self, table: &TableRef) -> Result<TableDefinition, QueryError> {
        catalog::definition(self, table)
    }

    fn columns(&mut self, schema: Option<&str>) -> Result<Vec<CatalogColumn>, QueryError> {
        catalog::catalog_columns(self, schema)
    }

    fn foreign_keys(&mut self, schema: &str) -> Result<Vec<ForeignKey>, QueryError> {
        catalog::catalog_foreign_keys(self, schema)
    }

    fn search_path(&mut self) -> Result<Vec<String>, QueryError> {
        catalog::search_path(self)
    }

    fn has_activity(&self) -> bool {
        true
    }

    fn activity(&mut self) -> Result<Activity, QueryError> {
        activity::activity(self)
    }

    fn signal(&mut self, pid: i32, signal: Signal) -> Result<bool, QueryError> {
        activity::signal(self, pid, signal)
    }

    fn routine_source(&mut self, routine: &FunctionInfo) -> Result<String, QueryError> {
        catalog::function_definition(self, routine)
    }
}

#[cfg(test)]
mod tests;
