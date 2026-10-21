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

mod catalog;
mod value;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::TryStreamExt as _;
use kuery_core::adapter::{Adapter, Canceller, RowSink};
use kuery_core::catalog::{
    CatalogColumn, CatalogOverview, ForeignKey, FunctionInfo, TableDefinition, TableRef,
};
use kuery_core::driver::{ConnectOptions, DriverInfo, Family, Field, SqlDriver, Tls};
use kuery_core::result_set::ColumnKind;
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
}

impl Target {
    fn config(&self) -> Config {
        let mut config = Config::new();
        config.host(&self.host);
        config.port(self.port);
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
    },
    dialect: &MSSQL,
    open: |options, access| open(&Target::from(options), access),
};

impl From<&ConnectOptions> for Target {
    /// `HOST\INSTANCE` names an instance. Through the local end of a tunnel
    /// when there is one, which reaches one port: no instance lookup.
    fn from(options: &ConnectOptions) -> Self {
        let (host, instance) = match options.host.split_once('\\') {
            Some((host, instance)) => (host, Some(instance.trim().to_string())),
            None => (options.host.as_str(), None),
        };
        let tunnelled = options.tunnel.is_some();
        Target {
            host: match tunnelled {
                true => "127.0.0.1".into(),
                false => host.trim().to_string(),
            },
            port: options.tunnel.unwrap_or(options.port),
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
        }
    }
}

/// The log records the current transaction wrote outside tempdb: more than
/// zero after a batch means it wrote (temporary tables do not count).
const WRITES: &str = "(select coalesce(sum(database_transaction_log_record_count), 0) \
     from sys.dm_tran_database_transactions \
     where transaction_id = (select transaction_id from sys.dm_tran_current_transaction) \
       and database_id <> 2)";

/// Why the writes check cannot run, as the connection's notice.
const UNDETECTED: &str = "Read-only is enforced by Kuery's checks and automatic undo only: grant VIEW SERVER PERFORMANCE STATE to this login, or use a login with only db_datareader, for writes to be refused.";

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
    /// The session's read-only transaction is open.
    reading: bool,
    /// `@@trancount` after the last batch.
    transactions: i64,
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
            reading: false,
            transactions: 0,
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

/// SQL Server errors 300, 297 and 262: the login may not read what the
/// writes check reads.
fn lacks_permission(code: u32) -> bool {
    matches!(code, 300 | 297 | 262)
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
/// `@@trancount`, and the log records written when asked.
struct After {
    changed: u64,
    transactions: i64,
    writes: Option<i64>,
}

async fn after_batch(connection: &mut Connection, check_writes: bool) -> Result<After, QueryError> {
    let sql = match check_writes {
        true => format!("select @@rowcount, @@trancount, {WRITES}"),
        false => "select @@rowcount, @@trancount".into(),
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
        writes: check_writes.then(|| number(2)),
    })
}

/// How a batch ended.
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
        let connection = runtime()
            .block_on(connect(&self.target, self.lock_timeout))
            .map_err(QueryError::Connection)?;
        self.connection = Some(connection);
        Ok(())
    }

    /// Whether dropping the connection now would lose a write transaction:
    /// one open before `sql`, or one `sql` may open itself.
    fn would_lose_writes(&self, sql: &str) -> bool {
        (self.transactions > 0 && !self.reading) || opens_transaction(sql)
    }
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
        (!self.detects_writes).then(|| UNDETECTED.to_string())
    }

    fn execute(&mut self, sql: &str, rows: &mut dyn RowSink) -> Result<u64, QueryError> {
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
        let connection = self.connection()?;

        stop.start();
        let ended = runtime().block_on(async {
            let run = async {
                let streamed = stream(connection, sql, rows).await;
                // Also after an error: a batch may have written before it.
                let after = match &streamed {
                    Err(QueryError::Database { .. }) | Ok(_) => {
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

        self.transactions = after.transactions;
        if begins_reading {
            self.reading = true;
        }
        // Any rollback ends the read-only transaction.
        if after.transactions == 0 {
            self.reading = false;
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

    fn routine_source(&mut self, routine: &FunctionInfo) -> Result<String, QueryError> {
        catalog::function_definition(self, routine)
    }
}

#[cfg(test)]
mod tests;
