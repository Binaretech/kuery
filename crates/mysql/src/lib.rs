//! MySQL and MariaDB driver.
//!
//! Runs on its own tokio runtime, like the PostgreSQL driver, and streams
//! results as columnar batches over a bounded channel. Values come as the
//! server's text (the text protocol), the same text `cast(… as char)` gives,
//! so row edits can compare what was shown.
//!
//! Queries go through two read-only layers:
//! 1. [`kuery_core::read_only::check_in`] rejects SQL that could write.
//! 2. The query runs inside `START TRANSACTION READ ONLY`. Read-only
//!    sessions also set `SESSION TRANSACTION READ ONLY`.
//!
//! Writes, on read-write sessions only, follow the PostgreSQL driver, with
//! one difference MySQL imposes: `CREATE`, `ALTER`, `DROP` and other
//! statements that commit by themselves cannot be undone, so a script with
//! one is refused where Kuery promises an undo (transactional write mode
//! and previews) and only runs in full write.

pub mod catalog;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kuery_core::analysis::{self, StatementKind};
use kuery_core::edit::WriteStatement;
use kuery_core::read_only;
use kuery_core::result_set::{BatchBuilder, ColumnKind, ResultSet};
use kuery_core::script;
use kuery_core::session::{
    Access, Event, QueryError, RunOptions, StatementImpact, Summary, collect,
};
use kuery_core::sql::Dialect;
use mysql_async::consts::ColumnType;
use mysql_async::prelude::Queryable as _;
use mysql_async::{Conn, Opts, OptsBuilder, SslOpts, Value};
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

/// Background runtime where the connections live.
pub struct Engine {
    runtime: Runtime,
}

impl Engine {
    pub fn new() -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("kuery-mysql")
            .enable_all()
            .build()?;
        Ok(Self { runtime })
    }

    /// Opens a session. `password` and everything else only live in
    /// memory.
    pub fn connect(
        &self,
        target: Target,
        access: Access,
    ) -> impl Future<Output = Result<Session, String>> + Send + use<> {
        let handle = self.runtime.handle().clone();
        let task = self.runtime.spawn(async move {
            let opts = target.opts();
            let mut conn = tokio::time::timeout(Duration::from_secs(15), Conn::new(opts.clone()))
                .await
                .map_err(|_| {
                    format!(
                        "{}:{} did not answer in 15 seconds",
                        target.host, target.port
                    )
                })?
                .map_err(|error| error.to_string())?;
            let version: String = conn
                .query_first("select version()")
                .await
                .map_err(|error| error.to_string())?
                .unwrap_or_default();
            if access == Access::ReadOnly {
                conn.query_drop("set session transaction read only")
                    .await
                    .map_err(|error| error.to_string())?;
            }
            let id = conn.id();
            Ok(Session {
                inner: Arc::new(Inner {
                    conn: Mutex::new(conn),
                    opts,
                    id,
                    handle,
                    mariadb: version.contains("MariaDB"),
                    version: version.split('-').next().unwrap_or(&version).to_string(),
                    access,
                    in_transaction: AtomicBool::new(false),
                    cancelled: AtomicBool::new(false),
                }),
            })
        });
        async move {
            task.await
                .unwrap_or_else(|error| Err(format!("the connection runtime failed: {error}")))
        }
    }
}

/// Where and how to connect.
#[derive(Clone)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    /// The database to start in; empty for none.
    pub database: String,
    /// TLS, checking the server's certificate against the system roots.
    pub tls: bool,
}

impl Target {
    fn opts(&self) -> Opts {
        let builder = OptsBuilder::default()
            .ip_or_hostname(self.host.clone())
            .tcp_port(self.port)
            .user(Some(self.user.clone()).filter(|u| !u.is_empty()))
            .pass(self.password.clone())
            .db_name(Some(self.database.clone()).filter(|d| !d.is_empty()))
            .prefer_socket(false)
            // Affected rows are the rows matched, not only those whose
            // values changed: a row edit that sets a value it already has
            // still counts as its one row.
            .client_found_rows(true)
            .ssl_opts(self.tls.then(SslOpts::default));
        Opts::from(builder)
    }
}

/// An open connection. Cheap to clone; runs one query at a time.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

struct Inner {
    conn: Mutex<Conn>,
    /// To open the side connection that cancels a query.
    opts: Opts,
    /// The connection's id on the server, for `KILL QUERY`.
    id: u32,
    handle: tokio::runtime::Handle,
    version: String,
    mariadb: bool,
    access: Access,
    in_transaction: AtomicBool,
    /// Set by [`Session::cancel`]: MySQL lets some killed queries end as if
    /// nothing happened (`SLEEP` returns 1), so the run checks this too.
    cancelled: AtomicBool,
}

impl Session {
    /// `8.4.2`, or MariaDB's `11.4.3`.
    pub fn server_version(&self) -> &str {
        &self.inner.version
    }

    pub fn is_mariadb(&self) -> bool {
        self.inner.mariadb
    }

    pub fn access(&self) -> Access {
        self.inner.access
    }

    fn stream<F, Fut>(&self, job: F) -> async_channel::Receiver<Event>
    where
        F: FnOnce(Arc<Inner>, async_channel::Sender<Event>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Summary, QueryError>> + Send + 'static,
    {
        let (tx, rx) = async_channel::bounded(4);
        let inner = self.inner.clone();
        self.inner.handle.spawn(async move {
            let event = match job(inner, tx.clone()).await {
                Ok(summary) => Event::Finished(summary),
                Err(error) => Event::Failed(error),
            };
            let _ = tx.send(event).await;
        });
        rx
    }

    pub(crate) fn call<T, F, Fut>(
        &self,
        job: F,
    ) -> impl Future<Output = Result<T, QueryError>> + Send + use<T, F, Fut>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Inner>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, QueryError>> + Send + 'static,
    {
        let task = self.inner.handle.spawn(job(self.inner.clone()));
        async move {
            task.await
                .unwrap_or_else(|error| Err(QueryError::Connection(error.to_string())))
        }
    }

    /// Runs `sql` read-only and returns the events over a channel. Dropping
    /// the receiver stops the query.
    pub fn run_read_only(
        &self,
        sql: String,
        options: RunOptions,
    ) -> async_channel::Receiver<Event> {
        if let Err(rejection) = read_only::check_in(Dialect::Mysql, &sql) {
            let (tx, rx) = async_channel::bounded(1);
            let _ = tx.try_send(Event::Failed(QueryError::Rejected(rejection)));
            return rx;
        }
        self.stream(move |inner, tx| async move { inner.read(&sql, &options, &tx, true).await })
    }

    pub fn fetch_all(
        &self,
        sql: String,
        options: RunOptions,
    ) -> impl Future<Output = Result<ResultSet, QueryError>> + use<> {
        collect(self.run_read_only(sql, options))
    }

    /// The driver's own fixed SQL (the catalog), still read-only.
    pub(crate) fn fetch_trusted(
        &self,
        sql: String,
    ) -> impl Future<Output = Result<ResultSet, QueryError>> + use<> {
        collect(self.stream(move |inner, tx| async move {
            inner.read(&sql, &RunOptions::default(), &tx, false).await
        }))
    }

    pub fn apply(
        &self,
        statements: Vec<WriteStatement>,
    ) -> impl Future<Output = Result<u64, QueryError>> + Send + use<> {
        self.call(move |inner| async move { inner.apply(&statements).await })
    }

    pub fn run_in_transaction(
        &self,
        sql: String,
        options: RunOptions,
    ) -> async_channel::Receiver<Event> {
        self.stream(move |inner, tx| async move {
            inner.write_in_transaction(&sql, &options, &tx).await
        })
    }

    pub fn run_autocommit(
        &self,
        sql: String,
        options: RunOptions,
    ) -> async_channel::Receiver<Event> {
        self.stream(
            move |inner, tx| async move { inner.write_autocommit(&sql, &options, &tx).await },
        )
    }

    pub fn preview(
        &self,
        sql: String,
        options: RunOptions,
    ) -> impl Future<Output = Result<Vec<StatementImpact>, QueryError>> + Send + use<> {
        self.call(move |inner| async move { inner.preview(&sql, &options).await })
    }

    pub fn in_transaction(&self) -> bool {
        self.inner.in_transaction.load(Ordering::SeqCst)
    }

    pub fn commit(&self) -> impl Future<Output = Result<(), QueryError>> + Send + use<> {
        self.call(|inner| async move { inner.end_transaction("commit").await })
    }

    pub fn rollback(&self) -> impl Future<Output = Result<(), QueryError>> + Send + use<> {
        self.call(|inner| async move { inner.end_transaction("rollback").await })
    }

    /// Stops the running statement: `KILL QUERY` from a side connection,
    /// again every quarter second while the run goes on (a kill that lands
    /// between two statements of the run stops nothing).
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        let inner = self.inner.clone();
        self.inner.handle.spawn(async move {
            let Ok(mut side) = Conn::new(inner.opts.clone()).await else {
                return;
            };
            for _ in 0..20 {
                let _ = side.query_drop(format!("kill query {}", inner.id)).await;
                tokio::time::sleep(Duration::from_millis(250)).await;
                // The run clears the flag when it ends.
                if !inner.cancelled.load(Ordering::SeqCst) {
                    break;
                }
            }
            let _ = side.disconnect().await;
        });
    }
}

/// A MySQL error as a [`QueryError`].
pub(crate) fn query_error(error: mysql_async::Error) -> QueryError {
    match error {
        mysql_async::Error::Server(server) => {
            if server.code == 1317 {
                // ER_QUERY_INTERRUPTED: KILL QUERY.
                return QueryError::Cancelled;
            }
            QueryError::Database {
                code: server.state.clone(),
                message: if matches!(server.code, 3024 | 1969) {
                    "The statement ran out of time".into()
                } else {
                    server.message
                },
            }
        }
        other => QueryError::Connection(other.to_string()),
    }
}

trait My<T> {
    fn my(self) -> Result<T, QueryError>;
}

impl<T> My<T> for Result<T, mysql_async::Error> {
    fn my(self) -> Result<T, QueryError> {
        self.map_err(query_error)
    }
}

impl Inner {
    /// The session's statement and lock timeouts.
    async fn limit(&self, conn: &mut Conn, options: &RunOptions) -> Result<(), QueryError> {
        let millis = options.statement_timeout.as_millis();
        let lock = options.lock_timeout.as_secs().max(1);
        let timeout = if self.mariadb {
            format!("set session max_statement_time = {}", millis as f64 / 1000.)
        } else {
            format!("set session max_execution_time = {millis}")
        };
        conn.query_drop(format!(
            "{timeout}; set session innodb_lock_wait_timeout = {lock}"
        ))
        .await
        .my()
    }

    /// A run's outcome, as cancelled if a cancel came while it ran.
    fn checked(&self, outcome: Result<Summary, QueryError>) -> Result<Summary, QueryError> {
        if self.cancelled.swap(false, Ordering::SeqCst) && outcome.is_ok() {
            return Err(QueryError::Cancelled);
        }
        outcome
    }

    async fn read(
        &self,
        sql: &str,
        options: &RunOptions,
        tx: &async_channel::Sender<Event>,
        typed: bool,
    ) -> Result<Summary, QueryError> {
        let mut conn = self.conn.lock().await;
        let started = Instant::now();
        self.cancelled.store(false, Ordering::SeqCst);
        self.limit(&mut conn, options).await?;
        if self.in_transaction.load(Ordering::SeqCst) {
            // Inside the open write transaction: see its changes, undo
            // anything the query does.
            conn.query_drop("savepoint kuery_read").await.my()?;
            let outcome = run_script(&mut conn, sql, options, tx, started, typed).await;
            let _ = conn.query_drop("rollback to savepoint kuery_read").await;
            let _ = conn.query_drop("release savepoint kuery_read").await;
            return self.checked(outcome);
        }
        conn.query_drop("start transaction read only").await.my()?;
        let outcome = run_script(&mut conn, sql, options, tx, started, typed).await;
        let _ = conn.query_drop("rollback").await;
        self.checked(outcome)
    }

    async fn apply(&self, statements: &[WriteStatement]) -> Result<u64, QueryError> {
        if self.access != Access::ReadWrite {
            return Err(QueryError::WritesDisabled);
        }
        if statements.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().await;
        if self.in_transaction.load(Ordering::SeqCst) {
            return Err(QueryError::TransactionOpen);
        }
        self.limit(
            &mut conn,
            &RunOptions {
                statement_timeout: Duration::from_secs(15),
                ..RunOptions::default()
            },
        )
        .await?;
        conn.query_drop("start transaction read write").await.my()?;
        let mut total = 0;
        for statement in statements {
            let outcome = conn.query_drop(statement.sql()).await.my();
            let actual = conn.affected_rows();
            let failure = match outcome {
                Err(error) => Some(error),
                Ok(()) if actual != statement.expected_rows() => Some(QueryError::RowMismatch {
                    sql: statement.sql().to_string(),
                    expected: statement.expected_rows(),
                    actual,
                }),
                Ok(()) => None,
            };
            if let Some(error) = failure {
                let _ = conn.query_drop("rollback").await;
                return Err(error);
            }
            total += actual;
        }
        conn.query_drop("commit").await.my()?;
        Ok(total)
    }

    async fn write_in_transaction(
        &self,
        sql: &str,
        options: &RunOptions,
        tx: &async_channel::Sender<Event>,
    ) -> Result<Summary, QueryError> {
        if self.access != Access::ReadWrite {
            return Err(QueryError::WritesDisabled);
        }
        refuse_implicit_commits(sql, "inside a transaction; run it in Full write")?;
        let mut conn = self.conn.lock().await;
        let started = Instant::now();
        self.limit(&mut conn, options).await?;
        if !self.in_transaction.load(Ordering::SeqCst) {
            conn.query_drop("start transaction read write").await.my()?;
            self.in_transaction.store(true, Ordering::SeqCst);
        }
        conn.query_drop("savepoint kuery_run").await.my()?;
        self.cancelled.store(false, Ordering::SeqCst);
        let outcome = run_script(&mut conn, sql, options, tx, started, false).await;
        // Cancelled: undone, like any failure.
        let outcome = self.checked(outcome);
        let close = if outcome.is_ok() {
            "release savepoint kuery_run"
        } else {
            "rollback to savepoint kuery_run"
        };
        if let Err(error) = conn.query_drop(close).await.my() {
            self.in_transaction.store(false, Ordering::SeqCst);
            return Err(outcome.err().unwrap_or(error));
        }
        if outcome.is_err() {
            let _ = conn.query_drop("release savepoint kuery_run").await;
        }
        outcome
    }

    async fn write_autocommit(
        &self,
        sql: &str,
        options: &RunOptions,
        tx: &async_channel::Sender<Event>,
    ) -> Result<Summary, QueryError> {
        if self.access != Access::ReadWrite {
            return Err(QueryError::WritesDisabled);
        }
        let mut conn = self.conn.lock().await;
        if self.in_transaction.load(Ordering::SeqCst) {
            return Err(QueryError::TransactionOpen);
        }
        let started = Instant::now();
        self.limit(&mut conn, options).await?;
        // A script of several statements is kept whole or not at all, as far
        // as MySQL can undo it (statements that commit by themselves cannot
        // be undone).
        let single = statements(sql).len() <= 1;
        if !single {
            conn.query_drop("start transaction read write").await.my()?;
        }
        self.cancelled.store(false, Ordering::SeqCst);
        let outcome = run_script(&mut conn, sql, options, tx, started, false).await;
        if single {
            self.cancelled.store(false, Ordering::SeqCst);
            return outcome;
        }
        // Cancelled: rolled back, like any failure.
        let outcome = self.checked(outcome);
        let close = if outcome.is_ok() {
            "commit"
        } else {
            "rollback"
        };
        conn.query_drop(close).await.my()?;
        outcome
    }

    async fn preview(
        &self,
        sql: &str,
        options: &RunOptions,
    ) -> Result<Vec<StatementImpact>, QueryError> {
        if self.access != Access::ReadWrite {
            return Err(QueryError::WritesDisabled);
        }
        refuse_implicit_commits(sql, "in a preview, which must undo everything")?;
        let mut conn = self.conn.lock().await;
        self.limit(&mut conn, options).await?;
        let in_transaction = self.in_transaction.load(Ordering::SeqCst);
        let (open, undo) = if in_transaction {
            (
                "savepoint kuery_preview",
                "rollback to savepoint kuery_preview",
            )
        } else {
            ("start transaction read write", "rollback")
        };
        conn.query_drop(open).await.my()?;
        let mut impacts = Vec::new();
        let mut outcome = Ok(());
        for statement in statements(sql) {
            match count(&mut conn, &statement).await {
                Ok(impact) => impacts.push(impact),
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
        }
        let undone = conn.query_drop(undo).await.my();
        if in_transaction {
            let _ = conn.query_drop("release savepoint kuery_preview").await;
        }
        outcome?;
        undone?;
        Ok(impacts)
    }

    async fn end_transaction(&self, command: &str) -> Result<(), QueryError> {
        let mut conn = self.conn.lock().await;
        if !self.in_transaction.load(Ordering::SeqCst) {
            return Ok(());
        }
        let result = conn.query_drop(command).await.my();
        self.in_transaction.store(false, Ordering::SeqCst);
        result
    }
}

/// Refuses a script with a statement MySQL commits by itself (`CREATE`,
/// `ALTER`, `DROP`, `TRUNCATE`…), which nothing could undo `where`.
fn refuse_implicit_commits(sql: &str, place: &str) -> Result<(), QueryError> {
    let Ok(analysis) = analysis::analyze_in(Dialect::Mysql, sql) else {
        return Ok(());
    };
    const COMMITTING: [&str; 7] = [
        "TRUNCATE", "RENAME", "LOCK", "UNLOCK", "ANALYZE", "OPTIMIZE", "REPAIR",
    ];
    let commits = |summary: &str| {
        let first = summary
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_uppercase();
        COMMITTING.contains(&first.as_str())
    };
    if let Some(statement) = analysis
        .statements
        .iter()
        .find(|s| s.kind == StatementKind::Schema || commits(&s.summary))
    {
        return Err(QueryError::Unsupported(format!(
            "MySQL commits {} by itself, so it cannot run {place}",
            statement.summary
        )));
    }
    Ok(())
}

async fn count(conn: &mut Conn, sql: &str) -> Result<StatementImpact, QueryError> {
    let mut result = conn.query_iter(sql).await.my()?;
    let returns_rows = result.columns().is_some_and(|columns| !columns.is_empty());
    if !returns_rows {
        let rows = result.affected_rows();
        result.drop_result().await.my()?;
        return Ok(StatementImpact {
            rows,
            returns_rows: false,
        });
    }
    let mut rows = 0;
    while result.next().await.my()?.is_some() {
        rows += 1;
    }
    result.drop_result().await.my()?;
    Ok(StatementImpact {
        rows,
        returns_rows: true,
    })
}

/// The statements of a script, in order; the whole text when it cannot be
/// split.
fn statements(sql: &str) -> Vec<String> {
    match script::statements_in(Dialect::Mysql, sql) {
        Some(ranges) => ranges
            .into_iter()
            .map(|range| sql[range].to_string())
            .collect(),
        None => vec![sql.to_string()],
    }
}

async fn send(tx: &async_channel::Sender<Event>, event: Event) -> Result<(), QueryError> {
    tx.send(event).await.map_err(|_| QueryError::Cancelled)
}

/// Runs each statement in turn, streaming the rows of those that return
/// any.
async fn run_script(
    conn: &mut Conn,
    sql: &str,
    options: &RunOptions,
    tx: &async_channel::Sender<Event>,
    started: Instant,
    typed: bool,
) -> Result<Summary, QueryError> {
    let mut summary = Summary {
        rows: 0,
        affected: 0,
        statements: 0,
        elapsed: Duration::ZERO,
        first_batch_after: None,
    };
    for statement in statements(sql) {
        let mut result = conn.query_iter(statement.as_str()).await.my()?;
        let columns = result.columns().filter(|columns| !columns.is_empty());
        match columns {
            None => {
                summary.affected += result.affected_rows();
                result.drop_result().await.my()?;
            }
            Some(columns) => {
                let names: Vec<String> = columns
                    .iter()
                    .map(|column| column.name_str().into_owned())
                    .collect();
                let count = names.len();
                send(tx, Event::Columns(names)).await?;
                if typed {
                    let kinds = columns
                        .iter()
                        .map(|column| kind_of(column.column_type()))
                        .collect();
                    send(tx, Event::Types(kinds)).await?;
                }
                let mut batch_rows = options.first_batch_rows.max(1);
                let mut builder = BatchBuilder::new(count, batch_rows);
                summary.rows = 0;
                while let Some(row) = result.next().await.my()? {
                    let values: Vec<Option<String>> = (0..count)
                        .map(|index| match row.as_ref(index) {
                            None | Some(Value::NULL) => None,
                            Some(Value::Bytes(bytes)) => {
                                Some(String::from_utf8_lossy(bytes).into_owned())
                            }
                            Some(other) => Some(other.as_sql(true).trim_matches('\'').to_string()),
                        })
                        .collect();
                    builder.push_row(|index| values[index].as_deref());
                    summary.rows += 1;
                    if builder.rows() >= batch_rows {
                        summary
                            .first_batch_after
                            .get_or_insert_with(|| started.elapsed());
                        send(tx, Event::Batch(builder.finish())).await?;
                        batch_rows = (batch_rows * 4).min(options.max_batch_rows);
                    }
                }
                if builder.rows() > 0 {
                    summary
                        .first_batch_after
                        .get_or_insert_with(|| started.elapsed());
                    send(tx, Event::Batch(builder.finish())).await?;
                }
                result.drop_result().await.my()?;
            }
        }
        summary.statements += 1;
    }
    summary.elapsed = started.elapsed();
    Ok(summary)
}

fn kind_of(column_type: ColumnType) -> ColumnKind {
    use ColumnType::*;
    match column_type {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG
        | MYSQL_TYPE_INT24 | MYSQL_TYPE_YEAR => ColumnKind::Integer,
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL | MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
            ColumnKind::Number
        }
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => ColumnKind::Date,
        MYSQL_TYPE_DATETIME
        | MYSQL_TYPE_DATETIME2
        | MYSQL_TYPE_TIMESTAMP
        | MYSQL_TYPE_TIMESTAMP2 => ColumnKind::Timestamp,
        MYSQL_TYPE_NULL => ColumnKind::Unknown,
        _ => ColumnKind::Text,
    }
}

#[cfg(test)]
mod tests;
