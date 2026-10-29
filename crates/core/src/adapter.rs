//! The contract between Kuery and a SQL driver.
//!
//! A driver is an [`Adapter`]: one connection, which runs one statement at
//! a time, says how to stop it, and reads its catalog. That is all it does.
//! Everything else (read-only queries in two layers, write mode with its
//! transaction and savepoints, full writes, previews that undo, row edits
//! with their checks, batches, timeouts and cancelling, the catalog's
//! derived views) lives once in [`crate::session::Session`], which drives
//! any adapter the same way.
//!
//! Adding a database means writing an adapter (with the [`Dialect`] it
//! speaks) and passing the conformance tests in [`crate::conformance`].
//!
//! Adapters are blocking: the session gives each one a thread of its own.
//! A driver over an async client waits on its own runtime there.

use std::sync::Arc;

use crate::activity::{Activity, Signal};
use crate::catalog::{
    CatalogColumn, CatalogOverview, ForeignKey, FunctionInfo, TableDefinition, TableRef,
};
use crate::result_set::ColumnKind;
use crate::session::{QueryError, RunOptions};
use crate::sql::Dialect;

/// Stops the statement running on a connection, from any thread. Called
/// again every quarter second until the statement ends, so a cancel that
/// lands between two steps still stops the next; harmless when nothing
/// runs. It returns at once (anything slow goes on another thread): the
/// next run waits for it to begin.
pub type Canceller = Arc<dyn Fn() + Send + Sync>;

/// Where a statement's rows go.
pub trait RowSink {
    /// Whether the columns' kinds are wanted (worth an extra round trip
    /// where the server only tells them on request).
    fn wants_kinds(&self) -> bool;

    /// A result starts: its columns, and their kinds when known.
    fn columns(
        &mut self,
        names: Vec<String>,
        kinds: Option<Vec<ColumnKind>>,
    ) -> Result<(), QueryError>;

    /// One row, as text (`None` for NULL): the text the database itself
    /// gives a value, so what is shown can be compared back. An error (the
    /// reader went away, say) means stop.
    fn row(&mut self, values: &[Option<&str>]) -> Result<(), QueryError>;
}

/// One open connection to a SQL database.
pub trait Adapter: Send + 'static {
    /// The SQL it speaks.
    fn dialect(&self) -> &'static Dialect;

    /// What it is connected to, such as `PostgreSQL 16.4`.
    fn server(&self) -> String;

    /// Runs one statement exactly as given. Rows it returns go to `rows`;
    /// returns how many rows it changed (0 for one that returns rows).
    /// Errors are the database's own, [`QueryError::Cancelled`] when
    /// stopped, or a lost connection.
    fn execute(&mut self, sql: &str, rows: &mut dyn RowSink) -> Result<u64, QueryError>;

    /// How long what runs next may take, and wait for a lock.
    fn limit(&mut self, options: &RunOptions) -> Result<(), QueryError>;

    /// How to stop what runs now (see [`Canceller`]).
    fn canceller(&self) -> Canceller;

    /// Schemas with their tables, views, routines, triggers and enums.
    fn overview(&mut self) -> Result<CatalogOverview, QueryError>;

    /// Everything about one table: columns, keys, indexes, constraints,
    /// triggers.
    fn definition(&mut self, table: &TableRef) -> Result<TableDefinition, QueryError>;

    /// The columns of every table and view in `schema`, or in every schema.
    fn columns(&mut self, schema: Option<&str>) -> Result<Vec<CatalogColumn>, QueryError>;

    /// The foreign keys of the tables in `schema`.
    fn foreign_keys(&mut self, schema: &str) -> Result<Vec<ForeignKey>, QueryError>;

    /// Schemas whose tables are written unqualified, in order.
    fn search_path(&mut self) -> Result<Vec<String>, QueryError>;

    /// A function's or procedure's source.
    fn routine_source(&mut self, routine: &FunctionInfo) -> Result<String, QueryError> {
        Err(QueryError::Unsupported(format!(
            "{} has no source to show for {}",
            self.dialect().name,
            routine.name
        )))
    }

    /// Whether [`Self::activity`] and [`Self::signal`] work.
    fn has_activity(&self) -> bool {
        false
    }

    /// Something the user should know about this connection, such as a
    /// protection it cannot give; asked once, at open.
    fn notice(&self) -> Option<String> {
        None
    }

    /// The server's sessions, what they run and who blocks whom.
    fn activity(&mut self) -> Result<Activity, QueryError> {
        Err(QueryError::Unsupported(format!(
            "Kuery cannot watch {} sessions yet",
            self.dialect().name
        )))
    }

    /// Cancels another session's query, or ends it; `false` when there was
    /// no such session.
    fn signal(&mut self, pid: i32, signal: Signal) -> Result<bool, QueryError> {
        let _ = (pid, signal);
        Err(QueryError::Unsupported(format!(
            "Kuery cannot signal {} sessions",
            self.dialect().name
        )))
    }
}

/// Collects rows as text, for an adapter's own catalog queries.
#[derive(Default)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl Rows {
    /// The text of `column` in `row`, empty for NULL.
    pub fn text(&self, row: usize, column: usize) -> String {
        self.get(row, column).unwrap_or_default().to_string()
    }

    pub fn get(&self, row: usize, column: usize) -> Option<&str> {
        self.rows.get(row)?.get(column)?.as_deref()
    }
}

impl RowSink for Rows {
    fn wants_kinds(&self) -> bool {
        false
    }

    fn columns(
        &mut self,
        names: Vec<String>,
        _: Option<Vec<ColumnKind>>,
    ) -> Result<(), QueryError> {
        self.columns = names;
        self.rows.clear();
        Ok(())
    }

    fn row(&mut self, values: &[Option<&str>]) -> Result<(), QueryError> {
        self.rows
            .push(values.iter().map(|v| v.map(str::to_string)).collect());
        Ok(())
    }
}

/// Runs `sql` on `adapter` and returns its rows.
pub fn fetch<A: Adapter + ?Sized>(adapter: &mut A, sql: &str) -> Result<Rows, QueryError> {
    let mut rows = Rows::default();
    adapter.execute(sql, &mut rows)?;
    Ok(rows)
}

/// Ignores rows (for statements run for their effect).
pub struct Discard;

impl RowSink for Discard {
    fn wants_kinds(&self) -> bool {
        false
    }

    fn columns(&mut self, _: Vec<String>, _: Option<Vec<ColumnKind>>) -> Result<(), QueryError> {
        Ok(())
    }

    fn row(&mut self, _: &[Option<&str>]) -> Result<(), QueryError> {
        Ok(())
    }
}
