//! SQLite driver.
//!
//! A database is a file. Each session keeps its connection on a thread of
//! its own (SQLite's API blocks) and takes work from a queue, one job at a
//! time; results come back as columnar batches over a bounded channel, like
//! every driver's, so the views do not know the difference.
//!
//! Queries go through two read-only layers, as with PostgreSQL:
//! 1. [`kuery_core::read_only::check_in`] rejects SQL that could write.
//! 2. The query runs with `PRAGMA query_only` on, so SQLite itself refuses
//!    any change. Read-only sessions keep it on for good, and open the file
//!    read-only too.
//!
//! Writes, on read-write sessions only, follow the PostgreSQL driver: row
//! changes in a transaction of their own ([`Session::apply`]), scripts in a
//! transaction that stays open ([`Session::run_in_transaction`]), scripts
//! kept as soon as they finish ([`Session::run_autocommit`]), and previews
//! that undo everything ([`Session::preview`]). Savepoints keep a failing
//! script from undoing more than itself.

pub mod catalog;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use kuery_core::edit::WriteStatement;
use kuery_core::read_only;
use kuery_core::result_set::{BatchBuilder, ColumnKind, ResultSet};
use kuery_core::script;
use kuery_core::session::{
    Access, Event, QueryError, RunOptions, StatementImpact, Summary, collect,
};
use kuery_core::sql::Dialect;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode, InterruptHandle, OpenFlags};

type Job = Box<dyn FnOnce(&mut Worker) + Send>;

/// An open database file. Cheap to clone; runs one job at a time.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

struct Inner {
    jobs: mpsc::Sender<Job>,
    interrupt: InterruptHandle,
    version: String,
    access: Access,
    path: PathBuf,
    /// A transaction opened by [`Session::run_in_transaction`] waits for a
    /// commit or a rollback.
    in_transaction: Arc<AtomicBool>,
}

/// The connection, on its thread.
pub(crate) struct Worker {
    connection: Connection,
    access: Access,
    in_transaction: Arc<AtomicBool>,
    /// Set by the progress handler when a statement ran out of time.
    timed_out: Arc<AtomicBool>,
}

impl Session {
    /// Opens the database at `path`, which must exist: a mistyped path
    /// never creates an empty database. Read-only sessions open the file
    /// read-only.
    pub fn open(
        path: PathBuf,
        access: Access,
    ) -> impl Future<Output = Result<Session, String>> + Send + use<> {
        let (sent, received) = oneshot::channel();
        std::thread::Builder::new()
            .name("kuery-sqlite".into())
            .spawn(move || match Worker::open(&path, access) {
                Ok((worker, version)) => {
                    let (jobs, queue) = mpsc::channel::<Job>();
                    let session = Session {
                        inner: Arc::new(Inner {
                            jobs,
                            interrupt: worker.connection.get_interrupt_handle(),
                            version,
                            access,
                            path,
                            in_transaction: worker.in_transaction.clone(),
                        }),
                    };
                    let _ = sent.send(Ok(session));
                    let mut worker = worker;
                    // Until every clone of the session is gone.
                    while let Ok(job) = queue.recv() {
                        job(&mut worker);
                    }
                }
                Err(error) => {
                    let _ = sent.send(Err(error));
                }
            })
            .ok();
        async move {
            received
                .await
                .unwrap_or_else(|_| Err("The SQLite thread stopped while opening".into()))
        }
    }

    pub fn server_version(&self) -> &str {
        &self.inner.version
    }

    pub fn access(&self) -> Access {
        self.inner.access
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Runs `job` on the connection's thread.
    pub(crate) fn call<T, F>(
        &self,
        job: F,
    ) -> impl Future<Output = Result<T, QueryError>> + Send + use<T, F>
    where
        T: Send + 'static,
        F: FnOnce(&mut Worker) -> Result<T, QueryError> + Send + 'static,
    {
        let (sent, received) = oneshot::channel();
        let queued = self.inner.jobs.send(Box::new(move |worker: &mut Worker| {
            let _ = sent.send(job(worker));
        }));
        async move {
            queued.map_err(|_| QueryError::Connection("the database is closed".into()))?;
            received
                .await
                .unwrap_or_else(|_| Err(QueryError::Connection("the database is closed".into())))
        }
    }

    /// Runs `job` on the connection's thread, streaming its events.
    fn stream(
        &self,
        job: impl FnOnce(&mut Worker, &async_channel::Sender<Event>) -> Result<Summary, QueryError>
        + Send
        + 'static,
    ) -> async_channel::Receiver<Event> {
        let (tx, rx) = async_channel::bounded(4);
        let failed = tx.clone();
        let queued = self.inner.jobs.send(Box::new(move |worker: &mut Worker| {
            let event = match job(worker, &tx) {
                Ok(summary) => Event::Finished(summary),
                Err(error) => Event::Failed(error),
            };
            let _ = tx.send_blocking(event);
        }));
        if queued.is_err() {
            let _ = failed.try_send(Event::Failed(QueryError::Connection(
                "the database is closed".into(),
            )));
        }
        rx
    }

    /// Runs `sql` read-only and returns the events over a channel. Dropping
    /// the receiver stops the query.
    pub fn run_read_only(
        &self,
        sql: String,
        options: RunOptions,
    ) -> async_channel::Receiver<Event> {
        if let Err(rejection) = read_only::check_in(Dialect::Sqlite, &sql) {
            let (tx, rx) = async_channel::bounded(1);
            let _ = tx.try_send(Event::Failed(QueryError::Rejected(rejection)));
            return rx;
        }
        self.stream(move |worker, tx| worker.read(&sql, &options, tx))
    }

    pub fn fetch_all(
        &self,
        sql: String,
        options: RunOptions,
    ) -> impl Future<Output = Result<ResultSet, QueryError>> + use<> {
        collect(self.run_read_only(sql, options))
    }

    /// Applies row changes in a transaction of their own; each must change
    /// exactly the rows it expects, or nothing is kept.
    pub fn apply(
        &self,
        statements: Vec<WriteStatement>,
    ) -> impl Future<Output = Result<u64, QueryError>> + Send + use<> {
        self.call(move |worker| worker.apply(&statements))
    }

    /// Runs a script inside the session's open transaction (opening one),
    /// under a savepoint: a failing script undoes only itself.
    pub fn run_in_transaction(
        &self,
        sql: String,
        options: RunOptions,
    ) -> async_channel::Receiver<Event> {
        self.stream(move |worker, tx| worker.write_in_transaction(&sql, &options, tx))
    }

    /// Runs a script and keeps its changes when it finishes: all of it, or
    /// none of it if a statement fails. A single statement runs as is (so
    /// `VACUUM` works).
    pub fn run_autocommit(
        &self,
        sql: String,
        options: RunOptions,
    ) -> async_channel::Receiver<Event> {
        self.stream(move |worker, tx| worker.write_autocommit(&sql, &options, tx))
    }

    /// Runs a script, counts what each statement changes, and undoes it.
    pub fn preview(
        &self,
        sql: String,
        options: RunOptions,
    ) -> impl Future<Output = Result<Vec<StatementImpact>, QueryError>> + Send + use<> {
        self.call(move |worker| worker.preview(&sql, &options))
    }

    pub fn in_transaction(&self) -> bool {
        self.inner.in_transaction.load(Ordering::SeqCst)
    }

    pub fn commit(&self) -> impl Future<Output = Result<(), QueryError>> + Send + use<> {
        self.call(|worker| worker.end_transaction("commit"))
    }

    pub fn rollback(&self) -> impl Future<Output = Result<(), QueryError>> + Send + use<> {
        self.call(|worker| worker.end_transaction("rollback"))
    }

    /// Stops the running statement.
    pub fn cancel(&self) {
        self.inner.interrupt.interrupt();
    }
}

impl Worker {
    fn open(path: &Path, access: Access) -> Result<(Worker, String), String> {
        if !path.is_file() {
            return Err(format!("There is no database file at {}", path.display()));
        }
        let flags = match access {
            Access::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
            Access::ReadWrite => OpenFlags::SQLITE_OPEN_READ_WRITE,
        } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection = Connection::open_with_flags(path, flags)
            .map_err(|error| format!("Could not open {}: {error}", path.display()))?;
        let setup = match access {
            // Even temporary tables are refused.
            Access::ReadOnly => "pragma query_only = on",
            // Foreign keys as declared: cascades and checks apply.
            Access::ReadWrite => "pragma foreign_keys = on",
        };
        connection
            .execute_batch(setup)
            .map_err(|error| format!("Could not open {}: {error}", path.display()))?;
        let version: String = connection
            .query_row("select sqlite_version()", [], |row| row.get(0))
            .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
        // Is it a database at all? Reading the schema says.
        connection
            .query_row("select count(*) from sqlite_master", [], |_| Ok(()))
            .map_err(|error| format!("{} is not a database: {error}", path.display()))?;
        let timed_out = Arc::new(AtomicBool::new(false));
        Ok((
            Worker {
                connection,
                access,
                in_transaction: Arc::new(AtomicBool::new(false)),
                timed_out,
            },
            version,
        ))
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Statements stop after `options.statement_timeout`, and wait up to
    /// `options.lock_timeout` for another process's lock.
    fn limit(&self, options: &RunOptions) {
        let started = Instant::now();
        let (limit, timed_out) = (options.statement_timeout, self.timed_out.clone());
        timed_out.store(false, Ordering::SeqCst);
        let _ = self.connection.progress_handler(
            10_000,
            Some(move || {
                let over = started.elapsed() > limit;
                if over {
                    timed_out.store(true, Ordering::SeqCst);
                }
                over
            }),
        );
        let _ = self.connection.busy_timeout(options.lock_timeout);
    }

    fn unlimit(&self) {
        let _ = self.connection.progress_handler(0, None::<fn() -> bool>);
    }

    fn error(&self, error: rusqlite::Error) -> QueryError {
        if let rusqlite::Error::SqliteFailure(failure, message) = &error {
            if failure.code == ErrorCode::OperationInterrupted {
                if self.timed_out.swap(false, Ordering::SeqCst) {
                    return QueryError::Database {
                        code: "timeout".into(),
                        message: "The statement ran out of time".into(),
                    };
                }
                return QueryError::Cancelled;
            }
            return QueryError::Database {
                code: format!("{}", failure.extended_code),
                message: message.clone().unwrap_or_else(|| failure.to_string()),
            };
        }
        QueryError::Database {
            code: String::new(),
            message: error.to_string(),
        }
    }

    fn batch(&self, sql: &str) -> Result<(), QueryError> {
        self.connection
            .execute_batch(sql)
            .map_err(|error| self.error(error))
    }

    fn read(
        &mut self,
        sql: &str,
        options: &RunOptions,
        tx: &async_channel::Sender<Event>,
    ) -> Result<Summary, QueryError> {
        let started = Instant::now();
        self.batch("pragma query_only = on")?;
        self.limit(options);
        let outcome = self.run_script(sql, options, tx, started);
        self.unlimit();
        if self.access == Access::ReadWrite {
            self.batch("pragma query_only = off")?;
        }
        outcome
    }

    fn write_in_transaction(
        &mut self,
        sql: &str,
        options: &RunOptions,
        tx: &async_channel::Sender<Event>,
    ) -> Result<Summary, QueryError> {
        if self.access != Access::ReadWrite {
            return Err(QueryError::WritesDisabled);
        }
        let started = Instant::now();
        if !self.in_transaction.load(Ordering::SeqCst) {
            self.batch("begin")?;
            self.in_transaction.store(true, Ordering::SeqCst);
        }
        self.batch("savepoint kuery_run")?;
        self.limit(options);
        let outcome = self.run_script(sql, options, tx, started);
        self.unlimit();
        let close = if outcome.is_ok() {
            "release kuery_run"
        } else {
            "rollback to kuery_run; release kuery_run"
        };
        if let Err(error) = self.batch(close) {
            self.in_transaction
                .store(!self.connection.is_autocommit(), Ordering::SeqCst);
            return Err(outcome.err().unwrap_or(error));
        }
        outcome
    }

    fn write_autocommit(
        &mut self,
        sql: &str,
        options: &RunOptions,
        tx: &async_channel::Sender<Event>,
    ) -> Result<Summary, QueryError> {
        if self.access != Access::ReadWrite {
            return Err(QueryError::WritesDisabled);
        }
        if self.in_transaction.load(Ordering::SeqCst) {
            return Err(QueryError::TransactionOpen);
        }
        let started = Instant::now();
        let single = statements(sql).len() <= 1;
        if !single {
            self.batch("savepoint kuery_script")?;
        }
        self.limit(options);
        let outcome = self.run_script(sql, options, tx, started);
        self.unlimit();
        if !single {
            let close = if outcome.is_ok() {
                "release kuery_script"
            } else {
                "rollback to kuery_script; release kuery_script"
            };
            self.batch(close)?;
        }
        outcome
    }

    fn preview(
        &mut self,
        sql: &str,
        options: &RunOptions,
    ) -> Result<Vec<StatementImpact>, QueryError> {
        if self.access != Access::ReadWrite {
            return Err(QueryError::WritesDisabled);
        }
        self.batch("savepoint kuery_preview")?;
        self.limit(options);
        let mut impacts = Vec::new();
        let mut outcome = Ok(());
        for statement in statements(sql) {
            match self.count(&statement) {
                Ok(impact) => impacts.push(impact),
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
        }
        self.unlimit();
        // Undo whatever happened, even halfway.
        let undone = self.batch("rollback to kuery_preview; release kuery_preview");
        outcome?;
        undone?;
        Ok(impacts)
    }

    fn count(&self, sql: &str) -> Result<StatementImpact, QueryError> {
        let mut statement = self
            .connection
            .prepare(sql)
            .map_err(|error| self.error(error))?;
        if statement.column_count() > 0 {
            let mut rows = statement.raw_query();
            let mut count = 0;
            while rows.next().map_err(|error| self.error(error))?.is_some() {
                count += 1;
            }
            Ok(StatementImpact {
                rows: count,
                returns_rows: true,
            })
        } else {
            let changed = statement.raw_execute().map_err(|error| self.error(error))?;
            Ok(StatementImpact {
                rows: changed as u64,
                returns_rows: false,
            })
        }
    }

    fn end_transaction(&mut self, command: &str) -> Result<(), QueryError> {
        if !self.in_transaction.load(Ordering::SeqCst) {
            return Ok(());
        }
        let result = self.batch(command);
        self.in_transaction
            .store(!self.connection.is_autocommit(), Ordering::SeqCst);
        result
    }

    fn apply(&mut self, statements: &[WriteStatement]) -> Result<u64, QueryError> {
        if self.access != Access::ReadWrite {
            return Err(QueryError::WritesDisabled);
        }
        if statements.is_empty() {
            return Ok(0);
        }
        if self.in_transaction.load(Ordering::SeqCst) {
            return Err(QueryError::TransactionOpen);
        }
        let _ = self.connection.busy_timeout(Duration::from_secs(5));
        self.batch("begin immediate")?;
        let outcome = (|| {
            let mut total = 0;
            for statement in statements {
                let actual = self
                    .connection
                    .execute(statement.sql(), [])
                    .map_err(|error| self.error(error))? as u64;
                if actual != statement.expected_rows() {
                    return Err(QueryError::RowMismatch {
                        sql: statement.sql().to_string(),
                        expected: statement.expected_rows(),
                        actual,
                    });
                }
                total += actual;
            }
            Ok(total)
        })();
        match outcome {
            Ok(total) => {
                self.batch("commit")?;
                Ok(total)
            }
            Err(error) => {
                let _ = self.batch("rollback");
                Err(error)
            }
        }
    }

    /// Runs each statement of `sql` in turn, streaming the rows of those
    /// that return any.
    fn run_script(
        &self,
        sql: &str,
        options: &RunOptions,
        tx: &async_channel::Sender<Event>,
        started: Instant,
    ) -> Result<Summary, QueryError> {
        let mut summary = Summary {
            rows: 0,
            affected: 0,
            statements: 0,
            elapsed: Duration::ZERO,
            first_batch_after: None,
        };
        for statement in statements(sql) {
            let mut prepared = self
                .connection
                .prepare(&statement)
                .map_err(|error| self.error(error))?;
            let count = prepared.column_count();
            if count == 0 {
                let changed = prepared.raw_execute().map_err(|error| self.error(error))?;
                summary.affected += changed as u64;
            } else {
                let names: Vec<String> = prepared
                    .column_names()
                    .into_iter()
                    .map(str::to_string)
                    .collect();
                let kinds: Vec<ColumnKind> = prepared
                    .columns()
                    .iter()
                    .map(|column| kind_of(column.decl_type()))
                    .collect();
                send(tx, Event::Columns(names))?;
                send(tx, Event::Types(kinds))?;
                let mut batch_rows = options.first_batch_rows.max(1);
                let mut builder = BatchBuilder::new(count, batch_rows);
                let mut rows = prepared.raw_query();
                summary.rows = 0;
                while let Some(row) = rows.next().map_err(|error| self.error(error))? {
                    let values: Vec<Option<String>> = (0..count)
                        .map(|index| row.get_ref(index).ok().and_then(text))
                        .collect();
                    builder.push_row(|index| values[index].as_deref());
                    summary.rows += 1;
                    if builder.rows() >= batch_rows {
                        summary
                            .first_batch_after
                            .get_or_insert_with(|| started.elapsed());
                        send(tx, Event::Batch(builder.finish()))?;
                        batch_rows = (batch_rows * 4).min(options.max_batch_rows);
                    }
                }
                if builder.rows() > 0 {
                    summary
                        .first_batch_after
                        .get_or_insert_with(|| started.elapsed());
                    send(tx, Event::Batch(builder.finish()))?;
                }
            }
            summary.statements += 1;
        }
        summary.elapsed = started.elapsed();
        Ok(summary)
    }
}

/// The statements of a script, in order; the whole text when it cannot be
/// split.
fn statements(sql: &str) -> Vec<String> {
    match script::statements_in(Dialect::Sqlite, sql) {
        Some(ranges) => ranges
            .into_iter()
            .map(|range| sql[range].to_string())
            .collect(),
        None => vec![sql.to_string()],
    }
}

fn send(tx: &async_channel::Sender<Event>, event: Event) -> Result<(), QueryError> {
    tx.send_blocking(event).map_err(|_| QueryError::Cancelled)
}

/// A value as text, the way SQLite's own `cast(… as text)` writes it (so a
/// row's shown values can be compared back), and blobs as `\x` hex.
fn text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(n) => Some(n.to_string()),
        ValueRef::Real(f) => Some(real(f)),
        ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Some(format!(
            "\\x{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )),
    }
}

/// `1.0`, `0.25`, `1.0e+20`: SQLite's `%!.15g`.
fn real(value: f64) -> String {
    if !value.is_finite() {
        return if value.is_nan() {
            String::new()
        } else if value > 0. {
            "Inf".into()
        } else {
            "-Inf".into()
        };
    }
    let formatted = format!("{:.15e}", value);
    let (mantissa, exponent) = formatted.split_once('e').unwrap_or((&formatted, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if (-5..15).contains(&exponent) {
        let digits = (14 - exponent).max(0) as usize;
        let fixed = format!("{value:.digits$}");
        let trimmed = if fixed.contains('.') {
            fixed
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_string()
        } else {
            fixed
        };
        if trimmed.contains('.') {
            trimmed
        } else {
            format!("{trimmed}.0")
        }
    } else {
        let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
        let mantissa = if mantissa.contains('.') {
            mantissa.to_string()
        } else {
            format!("{mantissa}.0")
        };
        format!(
            "{mantissa}e{}{:02}",
            if exponent < 0 { '-' } else { '+' },
            exponent.abs()
        )
    }
}

/// What a column holds, from its declared type and SQLite's affinity rules.
fn kind_of(declared: Option<&str>) -> ColumnKind {
    let Some(declared) = declared else {
        return ColumnKind::Unknown;
    };
    let upper = declared.to_uppercase();
    if upper.starts_with("BOOL") {
        ColumnKind::Boolean
    } else if upper == "DATE" {
        ColumnKind::Date
    } else if upper.starts_with("DATETIME") || upper.starts_with("TIMESTAMP") {
        ColumnKind::Timestamp
    } else if upper.contains("INT") {
        ColumnKind::Integer
    } else if ["CHAR", "CLOB", "TEXT"].iter().any(|t| upper.contains(t)) {
        ColumnKind::Text
    } else if upper.is_empty() || upper.contains("BLOB") {
        ColumnKind::Unknown
    } else {
        // REAL, FLOAT, DOUBLE, NUMERIC, DECIMAL…
        ColumnKind::Number
    }
}

#[cfg(test)]
mod tests;
