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
use crate::document::capabilities::Capabilities;
use crate::document::catalog::{CollectionInfo, Container};
use crate::document::change::{BrowseSpec, Continuation, Count, DocChange};
use crate::document::language::Language;
use crate::document::value::{CollectionRef, Document};
use crate::plan::Plan;
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

    fn commit(&mut self) -> Result<(), QueryError> {
        unsupported("transactions")
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
    /// [`Capabilities::sampling`] says.
    fn sample(
        &mut self,
        collection: &CollectionRef,
        n: usize,
        sink: &mut dyn DocumentSink,
    ) -> Result<(), QueryError>;

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
    /// change expects; the counts say whether it found one.
    fn apply(
        &mut self,
        collection: &CollectionRef,
        change: &DocChange,
    ) -> Result<Outcome, QueryError>;

    /// How many documents a write would touch, without running it, when
    /// the engine can tell.
    fn estimate(&mut self, statement: &str) -> Result<Option<Count>, QueryError> {
        let _ = statement;
        Ok(None)
    }

    /// A statement's plan: estimated, or measured by running it when
    /// `execute`.
    fn explain(&mut self, statement: &str, execute: bool) -> Result<Plan, QueryError> {
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
