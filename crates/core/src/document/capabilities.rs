//! What a document connection can do, detected when it opens and never
//! assumed from the engine's name: the same MongoDB driver reaches replica
//! sets with transactions and standalone servers without them, users the
//! server keeps from writing and users only Kuery does.

use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct Capabilities {
    /// What each level of the tree is called, from the top
    /// (`["Database", "Collection"]`).
    pub levels: &'static [&'static str],
    pub transactions: Transactions,
    /// Who keeps a read-only connection from writing.
    pub read_only: Enforcement,
    /// How a write's impact can be shown before it runs.
    pub preview: Preview,
    /// The plans it can show, if any.
    pub explain: Option<ExplainKind>,
    /// Whether the server's running operations can be watched.
    pub activity: bool,
    /// Whether one of them can be stopped.
    pub kill: bool,
    pub sampling: Sampling,
    /// Whether documents carry a version an edit can check (`_rev`, CAS).
    pub version_token: bool,
}

/// Whether several writes can be kept or undone together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transactions {
    /// Each write stands on its own.
    None,
    /// Transactions over several documents, which the server aborts after
    /// `lifetime`.
    Multi { lifetime: Duration },
}

/// Who refuses writes on a read-only connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    /// The server: the user cannot write, as detected at connect time.
    Server,
    /// Only Kuery, and why the server does not (`this server has no
    /// authentication`).
    ClientOnly(&'static str),
}

/// How a write's impact is shown before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preview {
    /// Run in a transaction and undone: exact.
    Undone,
    /// Counted from the write's filter: an estimate.
    Estimated,
    /// Not at all: the write counts as dangerous.
    None,
}

/// The plans an engine shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainKind {
    /// Only what the planner expects.
    Estimated,
    /// Also measured by running the statement.
    Measured,
}

/// How a schema sample is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sampling {
    /// Documents picked at random across the collection.
    Random,
    /// The first documents the engine returns, which the schema says.
    FirstN,
}
