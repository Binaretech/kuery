//! What every SQL driver's session speaks: whether it may write, how a
//! query runs, what it reports while it runs, and how it fails. Each driver
//! crate (Postgres, SQLite, MySQL) produces these, so the app's views work
//! with any of them.

use std::time::Duration;

use crate::read_only::Rejection;
use crate::result_set::{Batch, ColumnKind, ResultSet};

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
