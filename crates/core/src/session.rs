//! A SQL session, whichever the database: what it may do, how a query runs,
//! what it reports while it runs, how it fails, and the [`Session`] that
//! drives any driver's [`Adapter`] by the same rules.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use futures::channel::oneshot;

use crate::activity::{Activity, Signal};
use crate::adapter::{Adapter, Canceller, Discard, RowSink, Rows};
use crate::analysis::{self, StatementKind};
use crate::catalog::{CatalogOverview, FunctionInfo, TableDefinition, TableRef};
use crate::completion;
use crate::diagram::Diagram;
use crate::edit::WriteStatement;
use crate::read_only::{self, Rejection};
use crate::result_set::{Batch, BatchBuilder, ColumnKind, ResultSet};
use crate::script;
use crate::sql::Dialect;

/// What a connection allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Queries only. The norm in production.
    ReadOnly,
    /// Queries and row changes through the session's `apply`.
    ReadWrite,
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Maximum execution time on the server (`statement_timeout`).
    pub statement_timeout: Duration,
    /// Rows in the first batch: small so something shows up right away.
    pub first_batch_rows: usize,
    /// Maximum batch size; batches grow up to this.
    pub max_batch_rows: usize,
    /// How long a write waits for a lock before giving up (`lock_timeout`),
    /// so a busy table does not leave the script hanging.
    pub lock_timeout: Duration,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            statement_timeout: Duration::from_secs(30),
            first_batch_rows: 500,
            max_batch_rows: 50_000,
            lock_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Columns(Vec<String>),
    /// The kinds of the columns just announced, when the server reported
    /// them (single-statement reads). Comes right after their `Columns`.
    Types(Vec<ColumnKind>),
    Batch(Batch),
    Finished(Summary),
    Failed(QueryError),
}

/// What one statement of a previewed script did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementImpact {
    /// Rows changed, or rows returned when `returns_rows`.
    pub rows: u64,
    pub returns_rows: bool,
}

#[derive(Debug, Clone)]
pub struct Summary {
    /// Rows returned (by the last statement that returns rows).
    pub rows: usize,
    /// Rows changed by the statements that do not return rows.
    pub affected: u64,
    /// Statements the server completed.
    pub statements: usize,
    pub elapsed: Duration,
    /// Time until the first batch arrived.
    pub first_batch_after: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("{0}")]
    Rejected(Rejection),
    #[error("{message}")]
    Database { code: String, message: String },
    #[error("Query cancelled")]
    Cancelled,
    #[error("Connection lost: {0}")]
    Connection(String),
    #[error("This connection is read-only: changes cannot be applied")]
    WritesDisabled,
    #[error("A transaction is open: commit it or roll it back first")]
    TransactionOpen,
    #[error("{0}")]
    Explain(String),
    /// What this database cannot do (here, or at all).
    #[error("{0}")]
    Unsupported(String),
    #[error(
        "Expected to change {expected} row but {actual} changed: the row no longer exists or someone modified it. No changes were applied."
    )]
    RowMismatch {
        sql: String,
        expected: u64,
        actual: u64,
    },
}

/// Gathers a run's events into one result: the last result's columns and
/// rows, or its error. A channel closed before the end means cancelled.
pub async fn collect(rx: async_channel::Receiver<Event>) -> Result<ResultSet, QueryError> {
    let mut set = ResultSet::default();
    while let Ok(event) = rx.recv().await {
        match event {
            Event::Columns(names) => set = ResultSet::new(names),
            Event::Types(kinds) => set.set_kinds(kinds),
            Event::Batch(batch) => set.push(batch),
            Event::Finished(_) => return Ok(set),
            Event::Failed(error) => return Err(error),
        }
    }
    Err(QueryError::Cancelled)
}

// The session -----------------------------------------------------------

/// Opens an adapter, on the session's thread: at [`Session::open`] and
/// again for each [`Session::reopen`].
pub type Opener = Arc<dyn Fn(Access) -> Result<Box<dyn Adapter>, String> + Send + Sync>;

type Job = Box<dyn FnOnce(&mut Worker) + Send>;

/// An open connection to a SQL database, whichever its driver. Cheap to
/// clone; runs one job at a time on a thread of its own, which ends (and
/// closes the connection) when the last clone is dropped.
///
/// Every query is read-only in two layers: the SQL is analysed first
/// ([`read_only::check_in`]), then it runs in the dialect's read-only
/// transaction. Writes need a read-write session and come in four kinds:
/// row changes ([`Self::apply`]), scripts in a transaction that stays open
/// until [`Self::commit`] or [`Self::rollback`] ([`Self::run_in_transaction`],
/// each script under a savepoint so a failure undoes only itself), scripts
/// kept as soon as they end ([`Self::run_autocommit`], all of a script or
/// none of it), and previews that undo everything ([`Self::preview`]).
/// Where the dialect commits schema changes by itself, they are refused
/// wherever an undo is promised.
#[derive(Clone)]
pub struct Session {
    shared: Arc<Shared>,
}

struct Shared {
    jobs: mpsc::Sender<Job>,
    dialect: &'static Dialect,
    server: String,
    access: Access,
    has_activity: bool,
    canceller: Canceller,
    state: Arc<State>,
    opener: Opener,
}

/// What the session's thread and its handles both see.
#[derive(Default)]
struct State {
    /// A transaction opened by `run_in_transaction` waits for a commit or a
    /// rollback.
    in_transaction: AtomicBool,
    /// A statement runs: cancelling keeps at it until this goes false.
    running: AtomicBool,
    /// A cancel came: some databases let a stopped statement end as if
    /// nothing happened, so the run checks this too.
    cancelled: AtomicBool,
}

/// The adapter, on the session's thread, and the rules it is driven by.
struct Worker {
    adapter: Box<dyn Adapter>,
    access: Access,
    state: Arc<State>,
    canceller: Canceller,
}

impl Session {
    /// Opens a session: `opener` runs on the session's own thread.
    pub fn open(
        opener: Opener,
        access: Access,
    ) -> impl Future<Output = Result<Session, String>> + Send + 'static {
        let (sent, received) = oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("kuery-session".into())
            .spawn(move || {
                let adapter = match opener(access) {
                    Ok(adapter) => adapter,
                    Err(error) => {
                        let _ = sent.send(Err(error));
                        return;
                    }
                };
                let (jobs, queue) = mpsc::channel::<Job>();
                let state = Arc::new(State::default());
                let canceller = adapter.canceller();
                let session = Session {
                    shared: Arc::new(Shared {
                        jobs,
                        dialect: adapter.dialect(),
                        server: adapter.server(),
                        access,
                        has_activity: adapter.has_activity(),
                        canceller: canceller.clone(),
                        state: state.clone(),
                        opener: opener.clone(),
                    }),
                };
                if sent.send(Ok(session)).is_err() {
                    return;
                }
                let mut worker = Worker {
                    adapter,
                    access,
                    state,
                    canceller,
                };
                while let Ok(job) = queue.recv() {
                    job(&mut worker);
                }
            })
            .is_ok();
        async move {
            if !spawned {
                return Err("Could not start the connection's thread".into());
            }
            received
                .await
                .unwrap_or_else(|_| Err("The connection's thread stopped while opening".into()))
        }
    }

    /// Another session to the same database, with `access`.
    pub fn reopen(
        &self,
        access: Access,
    ) -> impl Future<Output = Result<Session, String>> + Send + 'static {
        Session::open(self.shared.opener.clone(), access)
    }

    pub fn dialect(&self) -> &'static Dialect {
        self.shared.dialect
    }

    /// `PostgreSQL 16.4`, for the status bar.
    pub fn server_label(&self) -> &str {
        &self.shared.server
    }

    pub fn access(&self) -> Access {
        self.shared.access
    }

    /// Whether the Plan tab can draw this database's plans.
    pub fn has_plans(&self) -> bool {
        self.shared.dialect.explain.is_some()
    }

    /// Whether the server's sessions can be watched (Server Activity).
    pub fn has_activity(&self) -> bool {
        self.shared.has_activity
    }

    pub fn in_transaction(&self) -> bool {
        self.shared.state.in_transaction.load(Ordering::SeqCst)
    }

    /// Runs `job` on the session's thread.
    fn call<T, F>(
        &self,
        job: F,
    ) -> impl Future<Output = Result<T, QueryError>> + Send + 'static + use<T, F>
    where
        T: Send + 'static,
        F: FnOnce(&mut Worker) -> Result<T, QueryError> + Send + 'static,
    {
        let (sent, received) = oneshot::channel();
        let queued = self.shared.jobs.send(Box::new(move |worker: &mut Worker| {
            let _ = sent.send(job(worker));
        }));
        async move {
            queued.map_err(|_| QueryError::Connection("the connection is closed".into()))?;
            received
                .await
                .unwrap_or_else(|_| Err(QueryError::Connection("the connection is closed".into())))
        }
    }

    /// Runs `job` on the session's thread, streaming its events.
    fn stream<F>(&self, job: F) -> async_channel::Receiver<Event>
    where
        F: FnOnce(&mut Worker, &async_channel::Sender<Event>) -> Result<Summary, QueryError>
            + Send
            + 'static,
    {
        let (tx, rx) = async_channel::bounded(4);
        let failed = tx.clone();
        let queued = self.shared.jobs.send(Box::new(move |worker: &mut Worker| {
            let event = match job(worker, &tx) {
                Ok(summary) => Event::Finished(summary),
                Err(error) => Event::Failed(error),
            };
            let _ = tx.send_blocking(event);
        }));
        if queued.is_err() {
            let _ = failed.try_send(Event::Failed(QueryError::Connection(
                "the connection is closed".into(),
            )));
        }
        rx
    }

    /// Runs one read-only statement and returns its events. Dropping the
    /// receiver stops it.
    pub fn run_read_only(
        &self,
        sql: String,
        options: RunOptions,
    ) -> async_channel::Receiver<Event> {
        if let Err(rejection) = read_only::check_in(self.dialect(), &sql) {
            let (tx, rx) = async_channel::bounded(1);
            let _ = tx.try_send(Event::Failed(QueryError::Rejected(rejection)));
            return rx;
        }
        self.stream(move |worker, tx| worker.read(&sql, &options, tx, true))
    }

    pub fn fetch_all(
        &self,
        sql: String,
        options: RunOptions,
    ) -> impl Future<Output = Result<ResultSet, QueryError>> + use<> {
        collect(self.run_read_only(sql, options))
    }

    /// Applies row changes in a transaction of their own: each must change
    /// exactly the rows it expects, or nothing is kept
    /// ([`QueryError::RowMismatch`]).
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

    /// Runs a script and keeps its changes when it ends: all of it, or none
    /// of it if a statement fails. A single statement runs as is, so one
    /// that refuses to run in a transaction (`VACUUM`) works.
    pub fn run_autocommit(
        &self,
        sql: String,
        options: RunOptions,
    ) -> async_channel::Receiver<Event> {
        self.stream(move |worker, tx| worker.write_autocommit(&sql, &options, tx))
    }

    /// Runs a script, counts what each statement changes or returns, and
    /// undoes it all.
    pub fn preview(
        &self,
        sql: String,
        options: RunOptions,
    ) -> impl Future<Output = Result<Vec<StatementImpact>, QueryError>> + Send + use<> {
        self.call(move |worker| worker.preview(&sql, &options))
    }

    /// The plan of `statement` (see [`Dialect::explain`]): estimated, or
    /// with `analyze` measured by running it, which only reads may do here
    /// (for the others, [`Self::explain_and_undo`]).
    pub fn explain(
        &self,
        statement: String,
        analyze: bool,
        options: RunOptions,
    ) -> impl Future<Output = Result<String, QueryError>> + Send + use<> {
        self.call(move |worker| worker.explain(&statement, analyze, &options))
    }

    /// The measured plan of a statement that may write: run, then undone
    /// like a preview. Needs a read-write session.
    pub fn explain_and_undo(
        &self,
        statement: String,
        options: RunOptions,
    ) -> impl Future<Output = Result<String, QueryError>> + Send + use<> {
        self.call(move |worker| worker.explain_and_undo(&statement, &options))
    }

    pub fn commit(&self) -> impl Future<Output = Result<(), QueryError>> + Send + use<> {
        self.call(|worker| worker.end_transaction("commit"))
    }

    pub fn rollback(&self) -> impl Future<Output = Result<(), QueryError>> + Send + use<> {
        self.call(|worker| worker.end_transaction("rollback"))
    }

    /// Stops the running statement, if any: now, and again every quarter
    /// second while it goes on.
    pub fn cancel(&self) {
        let state = self.shared.state.clone();
        if !state.running.load(Ordering::SeqCst) {
            return;
        }
        state.cancelled.store(true, Ordering::SeqCst);
        let canceller = self.shared.canceller.clone();
        let _ = std::thread::Builder::new()
            .name("kuery-cancel".into())
            .spawn(move || {
                for _ in 0..40 {
                    if !state.running.load(Ordering::SeqCst) {
                        break;
                    }
                    canceller();
                    std::thread::sleep(Duration::from_millis(250));
                }
            });
    }

    pub fn overview(
        &self,
    ) -> impl Future<Output = Result<CatalogOverview, QueryError>> + Send + use<> {
        self.call(|worker| worker.catalog(|adapter| adapter.overview()))
    }

    pub fn definition(
        &self,
        table: TableRef,
    ) -> impl Future<Output = Result<TableDefinition, QueryError>> + Send + use<> {
        self.call(move |worker| worker.catalog(|adapter| adapter.definition(&table)))
    }

    pub fn routine_source(
        &self,
        routine: FunctionInfo,
    ) -> impl Future<Output = Result<String, QueryError>> + Send + use<> {
        self.call(move |worker| worker.catalog(|adapter| adapter.routine_source(&routine)))
    }

    /// What completion knows: schemas, tables and their columns.
    pub fn completion_catalog(
        &self,
    ) -> impl Future<Output = Result<completion::Catalog, QueryError>> + Send + use<> {
        self.call(|worker| {
            worker.catalog(|adapter| {
                let search_path = adapter.search_path()?;
                let overview = adapter.overview()?;
                let columns = adapter.columns(None)?;
                Ok(completion::Catalog::compose(
                    search_path,
                    &overview,
                    columns,
                    adapter.dialect(),
                ))
            })
        })
    }

    /// A schema's tables with their columns and the foreign keys between
    /// them, to draw.
    pub fn diagram(
        &self,
        schema: String,
    ) -> impl Future<Output = Result<Diagram, QueryError>> + Send + use<> {
        self.call(move |worker| {
            worker.catalog(|adapter| {
                let overview = adapter.overview()?;
                let columns = adapter.columns(Some(&schema))?;
                let keys = adapter.foreign_keys(&schema)?;
                Ok(Diagram::compose(&schema, &overview.tables, columns, keys))
            })
        })
    }

    pub fn activity(&self) -> impl Future<Output = Result<Activity, QueryError>> + Send + use<> {
        self.call(|worker| worker.catalog(|adapter| adapter.activity()))
    }

    pub fn signal(
        &self,
        pid: i32,
        signal: Signal,
    ) -> impl Future<Output = Result<bool, QueryError>> + Send + use<> {
        self.call(move |worker| {
            worker.adapter.limit(&RunOptions::default())?;
            worker.adapter.signal(pid, signal)
        })
    }
}

/// The name Kuery gives the savepoint of each kind of run.
const READ: &str = "kuery_read";
const RUN: &str = "kuery_run";
const PREVIEW: &str = "kuery_preview";

impl Worker {
    fn dialect(&self) -> &'static Dialect {
        self.adapter.dialect()
    }

    fn exec(&mut self, sql: &str) -> Result<(), QueryError> {
        self.adapter.execute(sql, &mut Discard).map(|_| ())
    }

    fn exec_all(&mut self, statements: &[&str]) -> Result<(), QueryError> {
        statements.iter().try_for_each(|sql| self.exec(sql))
    }

    fn writable(&self) -> Result<(), QueryError> {
        match self.access {
            Access::ReadWrite => Ok(()),
            Access::ReadOnly => Err(QueryError::WritesDisabled),
        }
    }

    /// A statement is running, for [`Session::cancel`].
    fn begin_run(&mut self) {
        self.state.cancelled.store(false, Ordering::SeqCst);
        self.state.running.store(true, Ordering::SeqCst);
    }

    /// The run's outcome, as cancelled if a cancel came while it ran.
    fn end_run<T>(&mut self, outcome: Result<T, QueryError>) -> Result<T, QueryError> {
        self.state.running.store(false, Ordering::SeqCst);
        if self.state.cancelled.swap(false, Ordering::SeqCst) && outcome.is_ok() {
            return Err(QueryError::Cancelled);
        }
        outcome
    }

    /// Statements the dialect commits by itself, which nothing could undo.
    fn refuse_implicit_commits(&self, sql: &str, place: &str) -> Result<(), QueryError> {
        let dialect = self.dialect();
        if !dialect.implicit_commits {
            return Ok(());
        }
        let Ok(analysis) = analysis::analyze_in(dialect, sql) else {
            return Ok(());
        };
        const COMMITTING: [&str; 7] = [
            "TRUNCATE", "RENAME", "LOCK", "UNLOCK", "ANALYZE", "OPTIMIZE", "REPAIR",
        ];
        let commits = |summary: &str| {
            let first = summary.split_whitespace().next().unwrap_or_default();
            COMMITTING.contains(&first.to_uppercase().as_str())
        };
        match analysis
            .statements
            .iter()
            .find(|s| s.kind == StatementKind::Schema || commits(&s.summary))
        {
            Some(statement) => Err(QueryError::Unsupported(format!(
                "{} commits {} by itself, so it cannot run {place}",
                dialect.name, statement.summary
            ))),
            None => Ok(()),
        }
    }

    /// Runs `job` in a read-only transaction (or, inside the open write
    /// transaction, under a savepoint undone afterwards).
    fn read_with<T>(
        &mut self,
        options: &RunOptions,
        job: impl FnOnce(&mut Self) -> Result<T, QueryError>,
    ) -> Result<T, QueryError> {
        self.adapter.limit(options)?;
        let transactions = &self.dialect().transactions;
        if self.state.in_transaction.load(Ordering::SeqCst) {
            self.exec(&self.dialect().savepoint(READ))?;
            let outcome = job(self);
            let _ = self.exec(&self.dialect().rollback_to(READ));
            let _ = self.exec(&self.dialect().release(READ));
            return outcome;
        }
        self.exec_all(transactions.begin_read_only)?;
        let outcome = job(self);
        // There is never anything to keep.
        let _ = self.exec_all(self.dialect().transactions.end_read_only);
        outcome
    }

    fn catalog<T>(
        &mut self,
        job: impl FnOnce(&mut dyn Adapter) -> Result<T, QueryError>,
    ) -> Result<T, QueryError> {
        self.read_with(&RunOptions::default(), |worker| {
            job(worker.adapter.as_mut())
        })
    }

    fn read(
        &mut self,
        sql: &str,
        options: &RunOptions,
        tx: &async_channel::Sender<Event>,
        typed: bool,
    ) -> Result<Summary, QueryError> {
        let started = Instant::now();
        self.begin_run();
        let outcome = self.read_with(options, |worker| {
            worker.run_script(sql, options, tx, typed, started)
        });
        self.end_run(outcome)
    }

    fn write_in_transaction(
        &mut self,
        sql: &str,
        options: &RunOptions,
        tx: &async_channel::Sender<Event>,
    ) -> Result<Summary, QueryError> {
        self.writable()?;
        self.refuse_implicit_commits(sql, "inside a transaction; run it in Full write")?;
        let started = Instant::now();
        self.adapter.limit(options)?;
        let dialect = self.dialect();
        if !self.state.in_transaction.load(Ordering::SeqCst) {
            self.exec(dialect.transactions.begin)?;
            self.state.in_transaction.store(true, Ordering::SeqCst);
        }
        if let Err(error) = self.exec(&dialect.savepoint(RUN)) {
            self.lose_transaction();
            return Err(error);
        }
        self.begin_run();
        let outcome = self.run_script(sql, options, tx, false, started);
        // Cancelled: undone, like any failure.
        let outcome = self.end_run(outcome);
        let closed = if outcome.is_ok() {
            self.exec(&dialect.release(RUN))
        } else {
            self.exec(&dialect.rollback_to(RUN))
                .and_then(|()| self.exec(&dialect.release(RUN)))
        };
        if let Err(error) = closed {
            // The savepoint is gone only if the transaction is (the script
            // ended it itself, or the connection dropped).
            self.lose_transaction();
            return Err(outcome.err().unwrap_or(error));
        }
        outcome
    }

    /// The transaction is no longer what Kuery opened: roll back whatever is
    /// left and forget it.
    fn lose_transaction(&mut self) {
        let _ = self.exec("rollback");
        self.state.in_transaction.store(false, Ordering::SeqCst);
    }

    fn write_autocommit(
        &mut self,
        sql: &str,
        options: &RunOptions,
        tx: &async_channel::Sender<Event>,
    ) -> Result<Summary, QueryError> {
        self.writable()?;
        if self.state.in_transaction.load(Ordering::SeqCst) {
            return Err(QueryError::TransactionOpen);
        }
        let started = Instant::now();
        self.adapter.limit(options)?;
        let single = statements(self.dialect(), sql).len() <= 1;
        if single {
            self.begin_run();
            let outcome = self.run_script(sql, options, tx, false, started);
            // Kept or not, it ran: a late cancel does not undo it.
            self.state.running.store(false, Ordering::SeqCst);
            self.state.cancelled.store(false, Ordering::SeqCst);
            return outcome;
        }
        self.exec(self.dialect().transactions.begin)?;
        self.begin_run();
        let outcome = self.run_script(sql, options, tx, false, started);
        let outcome = self.end_run(outcome);
        let close = if outcome.is_ok() {
            "commit"
        } else {
            "rollback"
        };
        self.exec(close)?;
        outcome
    }

    /// Runs `job` and undoes whatever it did: under a savepoint inside the
    /// open transaction, or in a transaction of its own rolled back.
    fn undone<T>(
        &mut self,
        options: &RunOptions,
        job: impl FnOnce(&mut Self) -> Result<T, QueryError>,
    ) -> Result<T, QueryError> {
        self.adapter.limit(options)?;
        let dialect = self.dialect();
        let in_transaction = self.state.in_transaction.load(Ordering::SeqCst);
        if in_transaction {
            self.exec(&dialect.savepoint(PREVIEW))?;
        } else {
            self.exec(dialect.transactions.begin)?;
        }
        let outcome = job(self);
        // Even halfway.
        let undone = if in_transaction {
            self.exec(&dialect.rollback_to(PREVIEW))
                .and_then(|()| self.exec(&dialect.release(PREVIEW)))
        } else {
            self.exec("rollback")
        };
        if undone.is_err() && in_transaction {
            self.lose_transaction();
        }
        let value = outcome?;
        undone?;
        Ok(value)
    }

    fn preview(
        &mut self,
        sql: &str,
        options: &RunOptions,
    ) -> Result<Vec<StatementImpact>, QueryError> {
        self.writable()?;
        self.refuse_implicit_commits(sql, "in a preview, which must undo everything")?;
        let statements = statements(self.dialect(), sql);
        self.undone(options, |worker| {
            statements
                .iter()
                .map(|statement| {
                    let mut counter = Counter::default();
                    let changed = worker.adapter.execute(statement, &mut counter)?;
                    Ok(match counter.columns {
                        true => StatementImpact {
                            rows: counter.rows,
                            returns_rows: true,
                        },
                        false => StatementImpact {
                            rows: changed,
                            returns_rows: false,
                        },
                    })
                })
                .collect()
        })
    }

    fn explain(
        &mut self,
        statement: &str,
        analyze: bool,
        options: &RunOptions,
    ) -> Result<String, QueryError> {
        let dialect = self.dialect();
        let explain = dialect.explain.ok_or_else(|| {
            QueryError::Unsupported(format!("Plans are not drawn for {} yet", dialect.name))
        })?;
        let sql = explain(statement, analyze).map_err(QueryError::Explain)?;
        if analyze {
            read_only::check_in(dialect, statement).map_err(QueryError::Rejected)?;
        }
        self.begin_run();
        let outcome = self.read_with(options, |worker| worker.first_value(&sql));
        self.end_run(outcome)
    }

    fn explain_and_undo(
        &mut self,
        statement: &str,
        options: &RunOptions,
    ) -> Result<String, QueryError> {
        self.writable()?;
        let dialect = self.dialect();
        let explain = dialect.explain.ok_or_else(|| {
            QueryError::Unsupported(format!("Plans are not drawn for {} yet", dialect.name))
        })?;
        let sql = explain(statement, true).map_err(QueryError::Explain)?;
        self.refuse_implicit_commits(statement, "while measuring its plan")?;
        self.begin_run();
        let outcome = self.undone(options, |worker| worker.first_value(&sql));
        self.end_run(outcome)
    }

    fn first_value(&mut self, sql: &str) -> Result<String, QueryError> {
        let mut rows = Rows::default();
        self.adapter.execute(sql, &mut rows)?;
        rows.get(0, 0)
            .map(str::to_string)
            .ok_or_else(|| QueryError::Explain("The database returned no plan".into()))
    }

    fn end_transaction(&mut self, command: &str) -> Result<(), QueryError> {
        if !self.state.in_transaction.load(Ordering::SeqCst) {
            return Ok(());
        }
        let result = self.exec(command);
        // Whatever happened, the transaction is over: a failed COMMIT ends
        // it too, and so does a lost connection.
        self.state.in_transaction.store(false, Ordering::SeqCst);
        result
    }

    fn apply(&mut self, statements: &[WriteStatement]) -> Result<u64, QueryError> {
        self.writable()?;
        if statements.is_empty() {
            return Ok(0);
        }
        if self.state.in_transaction.load(Ordering::SeqCst) {
            return Err(QueryError::TransactionOpen);
        }
        self.adapter.limit(&RunOptions {
            statement_timeout: Duration::from_secs(15),
            ..RunOptions::default()
        })?;
        self.exec(self.dialect().transactions.begin)?;
        let mut total = 0;
        for statement in statements {
            let failure = match self.adapter.execute(statement.sql(), &mut Discard) {
                Err(error) => Some(error),
                Ok(actual) if actual != statement.expected_rows() => {
                    Some(QueryError::RowMismatch {
                        sql: statement.sql().to_string(),
                        expected: statement.expected_rows(),
                        actual,
                    })
                }
                Ok(actual) => {
                    total += actual;
                    None
                }
            };
            if let Some(error) = failure {
                let _ = self.exec("rollback");
                return Err(error);
            }
        }
        self.exec("commit")?;
        Ok(total)
    }

    /// Runs each statement of `sql` in turn, streaming the rows of those
    /// that return any.
    fn run_script(
        &mut self,
        sql: &str,
        options: &RunOptions,
        tx: &async_channel::Sender<Event>,
        typed: bool,
        started: Instant,
    ) -> Result<Summary, QueryError> {
        let mut stream = Stream {
            tx,
            typed,
            options,
            started,
            builder: None,
            batch_rows: options.first_batch_rows.max(1),
            rows: 0,
            first_batch_after: None,
        };
        let mut summary = Summary {
            rows: 0,
            affected: 0,
            statements: 0,
            elapsed: Duration::ZERO,
            first_batch_after: None,
        };
        for statement in statements(self.dialect(), sql) {
            let outcome = self
                .adapter
                .execute(&statement, &mut stream)
                .and_then(|changed| stream.flush().map(|()| changed));
            match outcome {
                Ok(changed) => summary.affected += changed,
                Err(error) => {
                    if matches!(error, QueryError::Cancelled) {
                        // The reader went away: stop the statement on the
                        // server too.
                        (self.canceller)();
                    }
                    return Err(error);
                }
            }
            summary.statements += 1;
            if stream.builder.is_some() {
                summary.rows = stream.rows;
            }
        }
        summary.first_batch_after = stream.first_batch_after;
        summary.elapsed = started.elapsed();
        Ok(summary)
    }
}

/// The statements of a script, in order; the whole text when it cannot be
/// split.
fn statements(dialect: &Dialect, sql: &str) -> Vec<String> {
    match script::statements_in(dialect, sql) {
        Some(ranges) if !ranges.is_empty() => ranges
            .into_iter()
            .map(|range| sql[range].to_string())
            .collect(),
        _ => vec![sql.to_string()],
    }
}

/// Streams a script's rows as batches that grow, as events.
struct Stream<'a> {
    tx: &'a async_channel::Sender<Event>,
    typed: bool,
    options: &'a RunOptions,
    started: Instant,
    /// The current result's, once it announced its columns.
    builder: Option<BatchBuilder>,
    batch_rows: usize,
    /// Rows of the current result.
    rows: usize,
    first_batch_after: Option<Duration>,
}

impl Stream<'_> {
    fn send(&self, event: Event) -> Result<(), QueryError> {
        self.tx
            .send_blocking(event)
            .map_err(|_| QueryError::Cancelled)
    }

    /// Sends what is left of the current result.
    fn flush(&mut self) -> Result<(), QueryError> {
        let Some(builder) = self.builder.as_mut().filter(|b| b.rows() > 0) else {
            return Ok(());
        };
        let batch = builder.finish();
        self.first_batch_after
            .get_or_insert_with(|| self.started.elapsed());
        self.send(Event::Batch(batch))
    }
}

impl RowSink for Stream<'_> {
    fn wants_kinds(&self) -> bool {
        self.typed
    }

    fn columns(
        &mut self,
        names: Vec<String>,
        kinds: Option<Vec<ColumnKind>>,
    ) -> Result<(), QueryError> {
        self.flush()?;
        let count = names.len();
        self.builder = Some(BatchBuilder::new(count, self.batch_rows));
        self.rows = 0;
        self.send(Event::Columns(names))?;
        if let Some(kinds) = kinds.filter(|kinds| self.typed && kinds.len() == count) {
            self.send(Event::Types(kinds))?;
        }
        Ok(())
    }

    fn row(&mut self, values: &[Option<&str>]) -> Result<(), QueryError> {
        let Some(builder) = self.builder.as_mut() else {
            return Ok(());
        };
        builder.push_row(|index| values.get(index).copied().flatten());
        self.rows += 1;
        if builder.rows() >= self.batch_rows {
            let batch = builder.finish();
            self.first_batch_after
                .get_or_insert_with(|| self.started.elapsed());
            self.batch_rows = (self.batch_rows * 4).min(self.options.max_batch_rows);
            self.send(Event::Batch(batch))?;
        }
        Ok(())
    }
}

/// Counts a statement's rows, for previews.
#[derive(Default)]
struct Counter {
    columns: bool,
    rows: u64,
}

impl RowSink for Counter {
    fn wants_kinds(&self) -> bool {
        false
    }

    fn columns(&mut self, _: Vec<String>, _: Option<Vec<ColumnKind>>) -> Result<(), QueryError> {
        self.columns = true;
        Ok(())
    }

    fn row(&mut self, _: &[Option<&str>]) -> Result<(), QueryError> {
        self.rows += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
