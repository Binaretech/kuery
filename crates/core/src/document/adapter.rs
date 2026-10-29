//! The contract between Kuery and a document database driver.
//!
//! A driver is a [`DocumentAdapter`]: one connection that runs one statement
//! of its language at a time, reads its catalog, browses and changes
//! documents, and says how to stop what runs. That is all it does.
//! Everything else (read-only by default, heavy work refused, scripts in one
//! transaction or stopped at the first failure, checked document changes,
//! batches, timeouts and cancelling) lives once in
//! [`crate::document::session::DocumentSession`], which drives any adapter
//! the same way.
//!
//! Adapters are blocking, like SQL ones: the session gives each one a
//! thread of its own, and a driver over an async client waits on its own
//! runtime there.

use crate::adapter::Canceller;
use crate::document::capabilities::{Capabilities, Sampling};
use crate::document::catalog::{CollectionInfo, Container};
use crate::document::change::{BrowseSpec, Continuation, Count, DocChange};
use crate::document::language::Language;
use crate::document::value::{CollectionRef, Document};
use crate::session::{QueryError, RunOptions};
use crate::worker::Driven;

/// Where a statement's documents go.
pub trait DocumentSink {
    /// Some documents, in the order the engine returned them. An error (the
    /// reader went away, say) means stop.
    fn documents(&mut self, batch: Vec<Document>) -> Result<(), QueryError>;

    /// Whether it holds all it takes of the statement that runs: the
    /// adapter stops reading and closes the cursor, and the statement ends
    /// well. Documents given after that are dropped.
    fn full(&self) -> bool {
        false
    }

    /// What the statement that runs changed before it failed, when the
    /// engine said so: an adapter tells it just before it returns the
    /// statement's error. Without it, what a failed write changed is
    /// unknown.
    fn changed(&mut self, outcome: Outcome) {
        let _ = outcome;
    }
}

/// Collects documents, for an adapter's own reads and for samples.
impl DocumentSink for Vec<Document> {
    fn documents(&mut self, batch: Vec<Document>) -> Result<(), QueryError> {
        self.extend(batch);
        Ok(())
    }
}

/// What a statement or a change did, as the engine counted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outcome {
    /// Documents a write's filter found.
    pub matched: u64,
    /// Documents it changed (fewer than matched when some already held the
    /// new values).
    pub modified: u64,
    pub inserted: u64,
    pub deleted: u64,
    /// Documents an upsert inserted.
    pub upserted: u64,
    /// Documents a read returned.
    pub returned: u64,
}

impl Outcome {
    /// Adds another statement's counts.
    pub fn add(&mut self, other: Outcome) {
        self.matched += other.matched;
        self.modified += other.modified;
        self.inserted += other.inserted;
        self.deleted += other.deleted;
        self.upserted += other.upserted;
        self.returned += other.returned;
    }
}

/// Why a write that keeps what it does as soon as it ends (a transaction's
/// commit, a change applied on its own) did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// It was not applied: nothing of it was kept.
    Undone(QueryError),
    /// The engine did not say whether it was applied, as when it ran out
    /// of time or its connection broke before the answer came.
    Unknown(QueryError),
}

impl WriteError {
    /// The error, whatever the write left.
    pub fn error(self) -> QueryError {
        match self {
            WriteError::Undone(error) | WriteError::Unknown(error) => error,
        }
    }
}

/// One open connection to a document database.
pub trait DocumentAdapter: Send + 'static {
    /// The language its statements are in.
    fn language(&self) -> &'static Language;

    /// What it is connected to, such as `MongoDB 8.0.4 · replica set rs0`.
    fn server(&self) -> String;

    /// What this connection can do, as detected when it opened.
    fn capabilities(&self) -> Capabilities;

    /// Something the user should know about this connection, such as a
    /// protection it cannot give; asked once, at open.
    fn notice(&self) -> Option<String> {
        None
    }

    /// The container statements work in when the connection opens, such as
    /// `["test"]` for MongoDB without a database; empty when statements
    /// work in none. A console enters it again before each run that has no
    /// other container to start in.
    fn home(&self) -> Vec<String> {
        Vec::new()
    }

    /// How long what runs next may take, and how big its batches are.
    fn limit(&mut self, options: &RunOptions) -> Result<(), QueryError>;

    /// How to stop what runs now (see [`Canceller`]).
    fn canceller(&self) -> Canceller;

    /// Runs one statement of its language exactly as given (reading it
    /// again itself). Documents it returns go to `sink`. Errors are the
    /// engine's own, [`QueryError::Cancelled`] when stopped, or a lost
    /// connection.
    fn execute(
        &mut self,
        statement: &str,
        sink: &mut dyn DocumentSink,
    ) -> Result<Outcome, QueryError>;

    /// Starts a transaction over what runs next, for engines that have them
    /// (see [`Capabilities::transactions`]).
    fn begin(&mut self) -> Result<(), QueryError> {
        unsupported("transactions")
    }

    /// Ends the transaction, applying it; it is gone either way.
    fn commit(&mut self) -> Result<(), WriteError> {
        unsupported("transactions").map_err(WriteError::Undone)
    }

    fn abort(&mut self) -> Result<(), QueryError> {
        unsupported("transactions")
    }

    /// What lies under `parent` in the tree: `[]` for the top level,
    /// `["shop"]` for a database's collections.
    fn children(&mut self, parent: &[String]) -> Result<Vec<Container>, QueryError>;

    /// Everything about one collection: its kind, indexes and validator.
    fn collection(&mut self, collection: &CollectionRef) -> Result<CollectionInfo, QueryError>;

    /// About `n` documents of the collection, picked as
    /// [`Capabilities::sampling`] says where the collection allows it.
    /// Returns how they were picked: a view, say, may only give its first.
    fn sample(
        &mut self,
        collection: &CollectionRef,
        n: usize,
        sink: &mut dyn DocumentSink,
    ) -> Result<Sampling, QueryError>;

    /// One page of the collection; returns where the next one starts, or
    /// `None` when this was the last.
    fn browse(
        &mut self,
        collection: &CollectionRef,
        spec: &BrowseSpec,
        sink: &mut dyn DocumentSink,
    ) -> Result<Option<Continuation>, QueryError>;

    /// How many documents match `filter` (all without one), counting no
    /// further than `cap` when it costs time.
    fn count(
        &mut self,
        collection: &CollectionRef,
        filter: Option<&str>,
        cap: u64,
    ) -> Result<Count, QueryError>;

    /// Applies one change, only to a document that still holds what the
    /// change expects; the counts say whether it found one. A failure says
    /// whether the change may have been applied all the same.
    fn apply(
        &mut self,
        collection: &CollectionRef,
        change: &DocChange,
    ) -> Result<Outcome, WriteError>;

    /// How many documents a write would touch, without running it and
    /// without changing anything, when the engine can tell; the first of
    /// them go to `sample`, which says when it holds enough. `None` when
    /// the engine cannot count what the statement touches.
    fn estimate(
        &mut self,
        statement: &str,
        sample: &mut dyn DocumentSink,
    ) -> Result<Option<Count>, QueryError> {
        let _ = (statement, sample);
        Ok(None)
    }

    /// A statement's plan, as the engine's own text that
    /// [`crate::plan::parse`] reads (JSON): what the planner chose, or,
    /// when `execute`, also measured by running it. The session asks to
    /// execute only reads; an adapter refuses to execute anything else all
    /// the same.
    fn explain(&mut self, statement: &str, execute: bool) -> Result<String, QueryError> {
        let _ = (statement, execute);
        unsupported("plans")
    }
}

impl Driven for dyn DocumentAdapter {
    fn canceller(&self) -> Canceller {
        DocumentAdapter::canceller(self)
    }
}

/// What an adapter answers for what its engine does not have.
fn unsupported<T>(what: &str) -> Result<T, QueryError> {
    Err(QueryError::Unsupported(format!(
        "This database has no {what}"
    )))
}
