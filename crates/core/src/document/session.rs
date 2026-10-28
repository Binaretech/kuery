//! A document session, whichever the engine: what it may do, how a script
//! runs, what it reports while it runs, and the [`DocumentSession`] that
//! drives any driver's [`DocumentAdapter`] by the same rules.

use std::cell::Cell;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::analysis::StatementKind;
use crate::document::adapter::{DocumentAdapter, DocumentSink, Outcome};
use crate::document::capabilities::{Capabilities, Transactions};
use crate::document::catalog::{CollectionInfo, Container};
use crate::document::change::{BrowseSpec, Continuation, Count, DocChange, Expected};
use crate::document::language::{Language, Style};
use crate::document::value::{CollectionRef, Document, Value};
use crate::read_only::Rejection;
use crate::session::{Access, QueryError, RunOptions};
use crate::worker::{Handle, Streamed, Worker};

/// Opens an adapter, on the session's thread: at [`DocumentSession::open`]
/// and again for each [`DocumentSession::reopen`].
pub type DocumentOpener =
    Arc<dyn Fn(Access) -> Result<Box<dyn DocumentAdapter>, String> + Send + Sync>;

/// What a run may do. A session that may write still runs reads as reads:
/// the console's Read-only toggle is a `Read` run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Read,
    Write,
}

/// Whether work the language marks heavy may run (JavaScript on the server,
/// an index build). The app allows it on development connections only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heavy {
    Refuse,
    Allow,
}

#[derive(Debug)]
pub enum DocEvent {
    /// Documents a statement returned, in batches that grow.
    Documents(Vec<Document>),
    /// What a statement did, once it ended.
    Outcome(Outcome),
    Finished(DocSummary),
    Failed(QueryError),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocSummary {
    /// Documents returned, by every statement.
    pub documents: usize,
    /// Every statement's counts, added up.
    pub outcome: Outcome,
    /// Statements the engine completed (none for a browse).
    pub statements: usize,
    pub elapsed: Duration,
    /// Time until the first documents arrived.
    pub first_batch_after: Option<Duration>,
    /// Where a browse's next page starts; `None` after the last.
    pub next: Option<Continuation>,
}

/// Why document changes were not all applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyFailure {
    /// Changes kept before the failure: none where the engine has
    /// transactions, which undo them all, nor when the connection itself
    /// failed (what it kept then is unknown). A [`QueryError::RowMismatch`]
    /// says on its own that nothing was applied, so say what this kept
    /// instead of showing that error as it is.
    pub applied: usize,
    pub error: QueryError,
}

/// An open connection to a document database, whichever its driver. Cheap
/// to clone; runs one job at a time on a thread of its own, which ends (and
/// closes the connection) when the last clone is dropped.
///
/// Every script is read by the engine's [`Language`] before anything runs:
/// a read-only session, or a [`RunMode::Read`] run, refuses whatever is not
/// a read, and heavy work runs only when the caller allows it. A write run
/// of several statements goes in one transaction where the engine has
/// them, and otherwise stops at the first failure. Document changes are
/// checked one by one ([`Self::apply`]). No transaction is ever left open
/// between calls.
#[derive(Clone)]
pub struct DocumentSession {
    handle: Handle<dyn DocumentAdapter>,
    shared: Arc<Shared>,
}

/// What the adapter said about itself when it opened, with the access it
/// was opened for and the way to open it again.
struct Shared {
    language: &'static Language,
    server: String,
    capabilities: Capabilities,
    notice: Option<String>,
    access: Access,
    opener: DocumentOpener,
}

impl DocumentSession {
    /// Opens a session: `opener` runs on the session's own thread.
    pub fn open(
        opener: DocumentOpener,
        access: Access,
    ) -> impl Future<Output = Result<DocumentSession, String>> + Send + 'static {
        let reopen = opener.clone();
        let opened = Handle::open(
            access,
            move || opener(access),
            move |adapter: &dyn DocumentAdapter| Shared {
                language: adapter.language(),
                server: adapter.server(),
                capabilities: adapter.capabilities(),
                notice: adapter.notice(),
                access,
                opener: reopen,
            },
        );
        async move {
            let (handle, shared) = opened.await?;
            Ok(DocumentSession {
                handle,
                shared: Arc::new(shared),
            })
        }
    }

    /// Another session to the same database, with `access`. Drivers share
    /// their client between the sessions of one target.
    pub fn reopen(
        &self,
        access: Access,
    ) -> impl Future<Output = Result<DocumentSession, String>> + Send + 'static {
        DocumentSession::open(self.shared.opener.clone(), access)
    }

    pub fn language(&self) -> &'static Language {
        self.shared.language
    }

    /// `MongoDB 8.0.4 · replica set rs0`, for the status bar.
    pub fn server_label(&self) -> &str {
        &self.shared.server
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.shared.capabilities
    }

    /// What the driver said the user should know about this connection
    /// (see [`DocumentAdapter::notice`]).
    pub fn notice(&self) -> Option<&str> {
        self.shared.notice.as_deref()
    }

    pub fn access(&self) -> Access {
        self.shared.access
    }

    /// Runs a script and returns its events. Dropping the receiver stops it.
    pub fn run(
        &self,
        text: String,
        mode: RunMode,
        heavy: Heavy,
        options: RunOptions,
    ) -> async_channel::Receiver<DocEvent> {
        let statements = match check(
            self.language(),
            self.capabilities(),
            self.access(),
            &text,
            mode,
            heavy,
        ) {
            Ok(statements) => statements,
            Err(error) => return failed(error),
        };
        let transactions = self.shared.capabilities.transactions;
        self.handle
            .stream(move |worker, tx| worker.run(&statements, mode, transactions, &options, tx))
    }

    /// Streams one page of a collection; the summary says where the next
    /// one starts. Its filter and projection are checked like a script's.
    pub fn browse(
        &self,
        collection: CollectionRef,
        spec: BrowseSpec,
        heavy: Heavy,
        options: RunOptions,
    ) -> async_channel::Receiver<DocEvent> {
        let bars = [
            ("The filter", &spec.filter),
            ("The projection", &spec.projection),
        ];
        for (name, text) in bars {
            if let Some(text) = text
                && let Err(error) = check_bar(self.language(), name, text, heavy)
            {
                return failed(error);
            }
        }
        self.handle
            .stream(move |worker, tx| worker.browse(&collection, &spec, &options, tx))
    }

    /// What lies under `parent` in the tree (`[]` for the top level).
    pub fn children(
        &self,
        parent: Vec<String>,
    ) -> impl Future<Output = Result<Vec<Container>, QueryError>> + Send + use<> {
        self.handle
            .call(move |worker| worker.catalog(|adapter| adapter.children(&parent)))
    }

    pub fn collection(
        &self,
        collection: CollectionRef,
    ) -> impl Future<Output = Result<CollectionInfo, QueryError>> + Send + use<> {
        self.handle
            .call(move |worker| worker.catalog(|adapter| adapter.collection(&collection)))
    }

    /// About `n` documents of the collection, to infer its schema.
    pub fn sample(
        &self,
        collection: CollectionRef,
        n: usize,
    ) -> impl Future<Output = Result<Vec<Document>, QueryError>> + Send + use<> {
        self.handle.call(move |worker| {
            worker.catalog(|adapter| {
                let mut documents = Vec::new();
                adapter.sample(&collection, n, &mut documents)?;
                Ok(documents)
            })
        })
    }

    /// How many documents match `filter`, counting no further than `cap`
    /// when it costs time. The filter is checked like the filter bar's.
    pub fn count(
        &self,
        collection: CollectionRef,
        filter: Option<String>,
        cap: u64,
        heavy: Heavy,
    ) -> impl Future<Output = Result<Count, QueryError>> + Send + use<> {
        let checked = match &filter {
            Some(text) => check_bar(self.language(), "The filter", text, heavy),
            None => Ok(()),
        };
        let counted = checked.map(|()| {
            self.handle.call(move |worker| {
                worker.catalog(|adapter| adapter.count(&collection, filter.as_deref(), cap))
            })
        });
        async move { counted?.await }
    }

    /// Applies document changes, each to a document that still holds what
    /// it expects. Where the engine has transactions, all of them or none;
    /// elsewhere they stop at the first failure, and the failure says how
    /// many were kept.
    pub fn apply(
        &self,
        collection: CollectionRef,
        changes: Vec<DocChange>,
    ) -> impl Future<Output = Result<Outcome, ApplyFailure>> + Send + use<> {
        let transactions = self.shared.capabilities.transactions;
        let render = self.shared.language.render;
        let applied = self
            .handle
            .call(move |worker| Ok(worker.apply(&collection, &changes, transactions, render)));
        async move {
            applied
                .await
                .unwrap_or_else(|error| Err(ApplyFailure { applied: 0, error }))
        }
    }

    /// Stops the running job, if any: now, and again every quarter second
    /// while it goes on.
    pub fn cancel(&self) {
        self.handle.cancel();
    }
}

/// A run that fails before it reaches the session's thread.
fn failed(error: QueryError) -> async_channel::Receiver<DocEvent> {
    let (tx, rx) = async_channel::bounded(1);
    let _ = tx.try_send(DocEvent::Failed(error));
    rx
}

/// Reads a script with the engine's language and refuses what the run may
/// not do, before anything reaches the adapter. Returns each statement's
/// text.
fn check(
    language: &Language,
    capabilities: &Capabilities,
    access: Access,
    text: &str,
    mode: RunMode,
    heavy: Heavy,
) -> Result<Vec<String>, QueryError> {
    let reads_only = match (mode, access) {
        (RunMode::Write, Access::ReadOnly) => return Err(QueryError::WritesDisabled),
        (RunMode::Write, Access::ReadWrite) => false,
        (RunMode::Read, _) => true,
    };
    let statements = (language.analyze)(text)
        .map_err(|error| QueryError::Rejected(Rejection::Parse(error.located(text))))?;
    if statements.is_empty() {
        return Err(QueryError::Rejected(Rejection::Empty));
    }
    for statement in &statements {
        let info = &statement.info;
        if reads_only && info.kind != StatementKind::Read {
            return Err(QueryError::Rejected(Rejection::Statement(
                info.summary.clone(),
            )));
        }
        if info.kind == StatementKind::Transaction {
            return Err(QueryError::Unsupported(format!(
                "Kuery runs each script's transaction itself, so {} cannot run here",
                info.summary
            )));
        }
        if info.heavy && heavy == Heavy::Refuse {
            return Err(QueryError::Rejected(Rejection::Heavy(info.summary.clone())));
        }
    }
    let atomic = matches!(capabilities.transactions, Transactions::Multi { .. });
    if !reads_only && atomic && statements.len() > 1 {
        let schema = statements
            .iter()
            .find(|statement| statement.info.kind == StatementKind::Schema);
        if let Some(schema) = schema {
            return Err(QueryError::Unsupported(format!(
                "{} cannot run inside a transaction: run it on its own",
                schema.info.summary
            )));
        }
    }
    statements
        .iter()
        .map(|statement| {
            text.get(statement.span.clone())
                .map(str::to_string)
                .ok_or_else(|| {
                    QueryError::Rejected(Rejection::Parse(format!(
                        "{} placed a statement outside the script",
                        language.name
                    )))
                })
        })
        .collect()
}

/// Checks a filter or projection bar's text.
fn check_bar(language: &Language, name: &str, text: &str, heavy: Heavy) -> Result<(), QueryError> {
    let checked = (language.check_filter)(text)
        .map_err(|error| QueryError::Rejected(Rejection::Parse(error.located(text))))?;
    if checked.heavy && heavy == Heavy::Refuse {
        return Err(QueryError::Rejected(Rejection::Heavy(name.into())));
    }
    Ok(())
}

/// The rules a document adapter is driven by, on the session's thread.
impl Worker<dyn DocumentAdapter> {
    fn catalog<T>(
        &mut self,
        job: impl FnOnce(&mut dyn DocumentAdapter) -> Result<T, QueryError>,
    ) -> Result<T, QueryError> {
        self.adapter.limit(&RunOptions::default())?;
        job(self.adapter.as_mut())
    }

    fn run(
        &mut self,
        statements: &[String],
        mode: RunMode,
        transactions: Transactions,
        options: &RunOptions,
        tx: &async_channel::Sender<DocEvent>,
    ) -> Result<DocSummary, QueryError> {
        let started = Instant::now();
        if mode == RunMode::Write {
            self.writable()?;
        }
        self.adapter.limit(options)?;
        let atomic = mode == RunMode::Write
            && statements.len() > 1
            && matches!(transactions, Transactions::Multi { .. });
        if atomic {
            self.adapter.begin()?;
        }
        self.begin_run();
        let outcome = self.run_statements(statements, options, tx, started);
        match (mode, atomic) {
            (RunMode::Read, _) => self.end_run(outcome),
            (RunMode::Write, true) => {
                // Cancelled: undone, like any failure.
                let outcome = self.end_run(outcome);
                let ended = match &outcome {
                    Ok(_) => self.adapter.commit(),
                    Err(_) => self.adapter.abort(),
                };
                if let Err(error) = ended {
                    // Whatever the failure, nothing is left open.
                    let _ = self.adapter.abort();
                    return Err(outcome.err().unwrap_or(error));
                }
                outcome
            }
            (RunMode::Write, false) => {
                // Kept as each statement ended: a late cancel undoes nothing.
                self.state.running.store(false, Ordering::SeqCst);
                self.state.cancelled.store(false, Ordering::SeqCst);
                outcome
            }
        }
    }

    /// Runs each statement in turn, streaming their documents and what
    /// each did, and stops at the first failure.
    fn run_statements(
        &mut self,
        statements: &[String],
        options: &RunOptions,
        tx: &async_channel::Sender<DocEvent>,
        started: Instant,
    ) -> Result<DocSummary, QueryError> {
        let mut stream = Stream::new(tx, options, started);
        let mut summary = DocSummary {
            documents: 0,
            outcome: Outcome::default(),
            statements: 0,
            elapsed: Duration::ZERO,
            first_batch_after: None,
            next: None,
        };
        for statement in statements {
            // A cancel stops the script before its next statement, even when
            // the one it reached ended well: what ran keeps its outcome.
            if self.state.cancelled.load(Ordering::SeqCst) {
                return Err(QueryError::Cancelled);
            }
            let outcome = self
                .adapter
                .execute(statement, &mut stream)
                .inspect_err(|error| stream.stop_on_the_server(error, &self.canceller))?;
            // The statement ended: a reader gone now has nothing to stop.
            stream.flush()?;
            stream.send(DocEvent::Outcome(outcome))?;
            summary.outcome.add(outcome);
            summary.statements += 1;
        }
        summary.documents = stream.documents;
        summary.first_batch_after = stream.first_batch_after;
        summary.elapsed = started.elapsed();
        Ok(summary)
    }

    fn browse(
        &mut self,
        collection: &CollectionRef,
        spec: &BrowseSpec,
        options: &RunOptions,
        tx: &async_channel::Sender<DocEvent>,
    ) -> Result<DocSummary, QueryError> {
        let started = Instant::now();
        self.adapter.limit(options)?;
        self.begin_run();
        let mut stream = Stream::new(tx, options, started);
        let outcome = self
            .adapter
            .browse(collection, spec, &mut stream)
            .inspect_err(|error| stream.stop_on_the_server(error, &self.canceller))
            .and_then(|next| stream.flush().map(|()| next));
        let next = self.end_run(outcome)?;
        Ok(DocSummary {
            documents: stream.documents,
            outcome: Outcome {
                returned: stream.documents as u64,
                ..Outcome::default()
            },
            statements: 0,
            elapsed: started.elapsed(),
            first_batch_after: stream.first_batch_after,
            next,
        })
    }

    fn apply(
        &mut self,
        collection: &CollectionRef,
        changes: &[DocChange],
        transactions: Transactions,
        render: fn(&Value, Style) -> String,
    ) -> Result<Outcome, ApplyFailure> {
        let none_kept = |error| ApplyFailure { applied: 0, error };
        self.writable().map_err(none_kept)?;
        if changes.is_empty() {
            return Ok(Outcome::default());
        }
        self.adapter
            .limit(&RunOptions {
                statement_timeout: Duration::from_secs(15),
                ..RunOptions::default()
            })
            .map_err(none_kept)?;
        let atomic = matches!(transactions, Transactions::Multi { .. });
        if atomic {
            self.adapter.begin().map_err(none_kept)?;
        }
        let mut total = Outcome::default();
        for (kept, change) in changes.iter().enumerate() {
            match self.apply_one(collection, change, render) {
                Ok(outcome) => total.add(outcome),
                Err(error) if atomic => {
                    let _ = self.adapter.abort();
                    return Err(none_kept(error));
                }
                Err(error) => {
                    return Err(ApplyFailure {
                        applied: kept,
                        error,
                    });
                }
            }
        }
        if atomic && let Err(error) = self.adapter.commit() {
            let _ = self.adapter.abort();
            return Err(none_kept(error));
        }
        Ok(total)
    }

    /// Applies one change and checks it did what its kind expects.
    fn apply_one(
        &mut self,
        collection: &CollectionRef,
        change: &DocChange,
        render: fn(&Value, Style) -> String,
    ) -> Result<Outcome, QueryError> {
        let outcome = self.adapter.apply(collection, change)?;
        let (expected, actual) = match change.expected() {
            Expected::Matched(expected) => (expected, outcome.matched),
            Expected::Inserted(expected) => (expected, outcome.inserted),
        };
        if actual != expected {
            return Err(QueryError::RowMismatch {
                sql: change.describe(render),
                expected,
                actual,
            });
        }
        Ok(outcome)
    }
}

impl Streamed for DocEvent {
    type Summary = DocSummary;

    fn finished(summary: DocSummary) -> Self {
        DocEvent::Finished(summary)
    }

    fn failed(error: QueryError) -> Self {
        DocEvent::Failed(error)
    }
}

/// Streams a run's documents as batches that grow, as events.
struct Stream<'a> {
    tx: &'a async_channel::Sender<DocEvent>,
    options: &'a RunOptions,
    started: Instant,
    /// Documents not sent yet.
    pending: Vec<Document>,
    batch: usize,
    /// Documents sent, by every statement.
    documents: usize,
    first_batch_after: Option<Duration>,
    /// Whether a send failed because the receiver was dropped.
    reader_gone: Cell<bool>,
}

impl<'a> Stream<'a> {
    fn new(
        tx: &'a async_channel::Sender<DocEvent>,
        options: &'a RunOptions,
        started: Instant,
    ) -> Self {
        Stream {
            tx,
            options,
            started,
            pending: Vec::new(),
            batch: options.first_batch_rows.max(1),
            documents: 0,
            first_batch_after: None,
            reader_gone: Cell::new(false),
        }
    }

    fn send(&self, event: DocEvent) -> Result<(), QueryError> {
        self.tx.send_blocking(event).map_err(|_| {
            self.reader_gone.set(true);
            QueryError::Cancelled
        })
    }

    fn ship(&mut self, batch: Vec<Document>) -> Result<(), QueryError> {
        self.documents += batch.len();
        self.first_batch_after
            .get_or_insert_with(|| self.started.elapsed());
        self.send(DocEvent::Documents(batch))
    }

    /// Sends what is left.
    fn flush(&mut self) -> Result<(), QueryError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(&mut self.pending);
        self.ship(batch)
    }

    /// The reader went away and the statement stopped for it: stop it on
    /// the server too. Not when the engine reported the cancel itself, nor
    /// after another error ended it, so a cancel never reaches what runs
    /// next.
    fn stop_on_the_server(&self, error: &QueryError, canceller: &crate::adapter::Canceller) {
        if self.reader_gone.get() && matches!(error, QueryError::Cancelled) {
            canceller();
        }
    }
}

impl DocumentSink for Stream<'_> {
    fn documents(&mut self, batch: Vec<Document>) -> Result<(), QueryError> {
        self.pending.extend(batch);
        while self.pending.len() >= self.batch {
            let rest = self.pending.split_off(self.batch);
            let full = std::mem::replace(&mut self.pending, rest);
            self.batch = (self.batch * 4).min(self.options.max_batch_rows.max(1));
            self.ship(full)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
