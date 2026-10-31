//! A document session, whichever the engine: what it may do, how a script
//! runs, what it reports while it runs, and the [`DocumentSession`] that
//! drives any driver's [`DocumentAdapter`] by the same rules.

use std::cell::Cell;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::activity::{Activity, BackendId, Signal};
use crate::analysis::StatementKind;
use crate::document::adapter::{DocumentAdapter, DocumentSink, Outcome, WriteError};
use crate::document::capabilities::{Capabilities, ExplainKind, Preview, Transactions};
use crate::document::catalog::{CollectionInfo, Container};
use crate::document::change::{BrowseSpec, Continuation, Count, DocChange, Expected};
use crate::document::language::{Language, OpInfo, Statement, Style};
use crate::document::schema::{self, SampleSink, Sampled};
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
    /// What a write run that failed left in the database, just before its
    /// [`DocEvent::Failed`].
    Left(Left),
    Finished(DocSummary),
    Failed(QueryError),
}

/// What a write run that failed left in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Left {
    /// Its transaction was undone: nothing of it was kept.
    Undone,
    /// What each statement did stayed as it ended: the statements that
    /// ended (their outcomes came before) and, of the one the run stopped
    /// in, `partial`. That is what it changed before it failed: nothing for
    /// a read, or when the run stopped between statements or before the
    /// first; `None` when unknown, as for a write stopped, out of time or
    /// cut off on its way.
    Kept { partial: Option<Outcome> },
    /// Its transaction's commit did not say whether it applied.
    Unknown,
}

impl Left {
    /// Nothing ran, so nothing was kept.
    const NOTHING: Left = Left::Kept {
        partial: Some(Outcome {
            matched: 0,
            modified: 0,
            inserted: 0,
            deleted: 0,
            upserted: 0,
            returned: 0,
        }),
    };
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
    /// transactions, which undo them all. A [`QueryError::RowMismatch`]
    /// says on its own that nothing was applied, so say what this kept
    /// instead of showing that error as it is.
    pub applied: usize,
    /// Changes after those that may have been applied all the same, since
    /// the engine's answer never came: the one it stopped at, or every one
    /// when a transaction's commit did not say (or the session itself
    /// failed). Applying them again could apply them twice.
    pub unknown: usize,
    pub error: QueryError,
}

/// What one statement of a script would do, as a preview found out
/// without keeping anything.
#[derive(Debug, Clone, PartialEq)]
pub struct Impact {
    /// What the statement does, as the language says it (`deleteMany
    /// shop.orders`).
    pub summary: String,
    pub estimate: Estimate,
    /// The first documents it would touch, or that it returned.
    pub sample: Vec<Document>,
    /// Why the preview is less than it could be, such as a statement that
    /// could not run in the transaction that was undone.
    pub note: Option<String>,
}

/// How much a preview knows about what a statement would change.
#[derive(Debug, Clone, PartialEq)]
pub enum Estimate {
    /// It only reads: it changes nothing.
    Reads,
    /// It ran in a transaction that was then undone: what it did.
    Exact(Outcome),
    /// Counted from what it would change, without running it.
    Estimated(Count),
    /// Not known, which makes the statement dangerous.
    Unknown,
}

/// Documents a preview keeps of each statement.
const PREVIEW_SAMPLE: usize = 20;

/// An open connection to a document database, whichever its driver. Cheap
/// to clone; runs one job at a time on a thread of its own, which ends (and
/// closes the connection) when the last clone is dropped.
///
/// Every script is read by the engine's [`Language`] before anything runs,
/// on the session's thread: a read-only session, or a [`RunMode::Read`]
/// run, refuses whatever is not a read, and heavy work runs only when the
/// caller allows it. A write run goes in one transaction where the engine
/// has them ([`Self::all_or_nothing`]), and otherwise stops at the first
/// failure. Document changes are checked one by one ([`Self::apply`]). No
/// transaction is ever left open between calls.
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
    home: Vec<String>,
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
                home: adapter.home(),
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

    /// Where statements work when the connection opens (see
    /// [`DocumentAdapter::home`]).
    pub fn home(&self) -> &[String] {
        &self.shared.home
    }

    pub fn access(&self) -> Access {
        self.shared.access
    }

    /// Runs a script where the last run left off (see [`Self::run_in`]).
    pub fn run(
        &self,
        text: String,
        mode: RunMode,
        heavy: Heavy,
        options: RunOptions,
    ) -> async_channel::Receiver<DocEvent> {
        self.run_in(&[], text, mode, heavy, options)
    }

    /// Runs a script that starts in the container `start` (`["shop"]`;
    /// empty for where the last run left off), entered in the same job, so
    /// nothing else runs between. Returns its events; dropping the receiver
    /// stops it. A write run that fails says what it left
    /// ([`DocEvent::Left`]).
    pub fn run_in(
        &self,
        start: &[String],
        text: String,
        mode: RunMode,
        heavy: Heavy,
        options: RunOptions,
    ) -> async_channel::Receiver<DocEvent> {
        let enter = (self.language().enter)(start);
        let (language, capabilities, access) =
            (self.language(), self.capabilities().clone(), self.access());
        self.handle.stream(move |worker, tx| {
            let statements = check(language, &capabilities, access, &text, mode, heavy)
                .and_then(|statements| parts(language, &text, statements));
            let statements = match statements {
                Ok(statements) => statements,
                Err(error) => {
                    if mode == RunMode::Write {
                        let _ = tx.send_blocking(DocEvent::Left(Left::NOTHING));
                    }
                    return Err(error);
                }
            };
            let transactions = capabilities.transactions;
            worker.run(
                enter.as_deref(),
                &statements,
                mode,
                transactions,
                &options,
                tx,
            )
        })
    }

    /// Reads a script as [`Self::run`] would before running it, and refuses
    /// what that run may not do; nothing reaches the database. The
    /// statements, as the language reads them.
    pub fn check(
        &self,
        text: &str,
        mode: RunMode,
        heavy: Heavy,
    ) -> Result<Vec<Statement>, QueryError> {
        check(
            self.language(),
            self.capabilities(),
            self.access(),
            text,
            mode,
            heavy,
        )
    }

    /// Refuses what a run of `statements`, as the language read them, may
    /// not do (see [`Self::check`]), without reading the script again.
    pub fn check_statements(
        &self,
        statements: &[Statement],
        mode: RunMode,
        heavy: Heavy,
    ) -> Result<(), QueryError> {
        check_statements(self.capabilities(), self.access(), statements, mode, heavy)
    }

    /// Whether a write run of `statements` goes in one transaction, all of
    /// it or none: where the engine has transactions, for a script of
    /// several statements or a statement that changes documents. A lone
    /// schema change, or what the language does not know, runs on its own.
    pub fn all_or_nothing(&self, statements: &[Statement]) -> bool {
        all_or_nothing(
            self.capabilities().transactions,
            statements.iter().map(|statement| &statement.info),
        )
    }

    /// Previews a script where the last run left off (see
    /// [`Self::preview_in`]).
    pub fn preview(
        &self,
        text: String,
        heavy: Heavy,
        options: RunOptions,
    ) -> impl Future<Output = Result<Vec<Impact>, QueryError>> + Send + use<> {
        self.preview_in(&[], text, heavy, options)
    }

    /// What each statement of a write script that starts in `start` would
    /// do, without keeping any of it, as the engine's [`Preview`] allows:
    /// run in a transaction that is always undone, even halfway; counted
    /// from what each statement would change; or unknown. A statement that
    /// cannot run in that transaction (a schema change, or one the engine
    /// refuses there) and the ones after it are counted instead. Checked
    /// like a write run, on the session's thread, and refused on a
    /// read-only session.
    pub fn preview_in(
        &self,
        start: &[String],
        text: String,
        heavy: Heavy,
        options: RunOptions,
    ) -> impl Future<Output = Result<Vec<Impact>, QueryError>> + Send + use<> {
        let enter = (self.language().enter)(start);
        let (language, capabilities, access) =
            (self.language(), self.capabilities().clone(), self.access());
        self.handle.call(move |worker| {
            let statements = check(
                language,
                &capabilities,
                access,
                &text,
                RunMode::Write,
                heavy,
            )
            .and_then(|statements| parts(language, &text, statements))?;
            worker.preview(
                enter.as_deref(),
                &statements,
                capabilities.preview,
                &options,
            )
        })
    }

    /// The plan of one statement, starting in the container `start` (empty
    /// for where the last run left off), as the engine's text that
    /// [`crate::plan::parse`] reads. With `execute`, the engine measures it
    /// by running it, which only a read may do; a statement that writes is
    /// only ever planned, never run, and only on a session that may write.
    /// Heavy work is refused as in a run when it would execute. Checked on
    /// the session's thread, before the adapter sees it.
    pub fn explain_in(
        &self,
        start: &[String],
        text: String,
        execute: bool,
        heavy: Heavy,
        options: RunOptions,
    ) -> impl Future<Output = Result<String, QueryError>> + Send + use<> {
        let enter = (self.language().enter)(start);
        let (language, capabilities, access) =
            (self.language(), self.capabilities().clone(), self.access());
        self.handle.call(move |worker| {
            let execute = check_explain(language, &capabilities, access, &text, execute, heavy)?;
            worker.explain(enter.as_deref(), &text, execute, &options)
        })
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

    /// About `n` documents of the collection.
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

    /// The schema of about `n` documents of the collection, or as many as
    /// fit in [`schema::SAMPLE_BYTES`], sampled and inferred on the
    /// session's thread.
    pub fn schema(
        &self,
        collection: CollectionRef,
        n: usize,
    ) -> impl Future<Output = Result<Sampled, QueryError>> + Send + use<> {
        self.handle.call(move |worker| {
            worker.catalog(|adapter| {
                let mut sink = SampleSink::default();
                let sampling = adapter.sample(&collection, n, &mut sink)?;
                Ok(Sampled {
                    schema: schema::infer(&sink.documents),
                    sampling,
                    capped: sink.capped(),
                })
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
    /// elsewhere they stop at the first failure. The failure says how many
    /// were kept, and how many more may have been when the engine's answer
    /// never came.
    pub fn apply(
        &self,
        collection: CollectionRef,
        changes: Vec<DocChange>,
    ) -> impl Future<Output = Result<Outcome, ApplyFailure>> + Send + use<> {
        let transactions = self.shared.capabilities.transactions;
        let render = self.shared.language.render;
        let count = changes.len();
        let applied = self
            .handle
            .call(move |worker| Ok(worker.apply(&collection, &changes, transactions, render)));
        async move {
            // The session failed on its way: what it applied is unknown.
            applied.await.unwrap_or_else(|error| {
                Err(ApplyFailure {
                    applied: 0,
                    unknown: count,
                    error,
                })
            })
        }
    }

    /// The server's running operations (see
    /// [`DocumentAdapter::activity`]).
    pub fn activity(&self) -> impl Future<Output = Result<Activity, QueryError>> + Send + use<> {
        self.handle
            .call(|worker| worker.catalog(|adapter| adapter.activity()))
    }

    /// Stops operation `id`, or ends its session; `false` when it was
    /// already gone. It changes no data, so a read-only session may send it.
    pub fn signal(
        &self,
        id: BackendId,
        signal: Signal,
    ) -> impl Future<Output = Result<bool, QueryError>> + Send + use<> {
        self.handle
            .call(move |worker| worker.catalog(|adapter| adapter.signal(&id, signal)))
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
/// not do, before anything reaches the adapter.
fn check(
    language: &Language,
    capabilities: &Capabilities,
    access: Access,
    text: &str,
    mode: RunMode,
    heavy: Heavy,
) -> Result<Vec<Statement>, QueryError> {
    if (mode, access) == (RunMode::Write, Access::ReadOnly) {
        return Err(QueryError::WritesDisabled);
    }
    let statements = (language.analyze)(text)
        .map_err(|error| QueryError::Rejected(Rejection::Parse(error.located(text))))?;
    check_statements(capabilities, access, &statements, mode, heavy)?;
    Ok(statements)
}

/// Refuses what an explain of `text` may not do (see
/// [`DocumentSession::explain_in`]). Whether it runs the statement: only
/// when asked to and it reads.
fn check_explain(
    language: &Language,
    capabilities: &Capabilities,
    access: Access,
    text: &str,
    execute: bool,
    heavy: Heavy,
) -> Result<bool, QueryError> {
    match capabilities.explain {
        None => {
            return Err(QueryError::Unsupported(format!(
                "Kuery shows no plans for {} here",
                language.name
            )));
        }
        Some(ExplainKind::Estimated) if execute => {
            return Err(QueryError::Unsupported(format!(
                "{} here only says what it plans to do: explain without measuring",
                language.name
            )));
        }
        Some(_) => {}
    }
    let statements = (language.analyze)(text)
        .map_err(|error| QueryError::Rejected(Rejection::Parse(error.located(text))))?;
    let info = match statements.as_slice() {
        [] => return Err(QueryError::Rejected(Rejection::Empty)),
        [statement] => &statement.info,
        _ => {
            return Err(QueryError::Explain(
                "Explain one statement at a time: put the cursor on it".into(),
            ));
        }
    };
    if !info.explainable {
        return Err(QueryError::Explain(format!(
            "{} has no plan to show",
            info.summary
        )));
    }
    let reads = info.kind == StatementKind::Read;
    if !reads {
        if execute {
            return Err(QueryError::Explain(format!(
                "Measuring a plan runs the statement, and {} writes: explain it without \
                 measuring, which never runs it",
                info.summary
            )));
        }
        if access == Access::ReadOnly {
            return Err(QueryError::WritesDisabled);
        }
    }
    if execute && info.heavy && heavy == Heavy::Refuse {
        return Err(QueryError::Rejected(Rejection::Heavy(info.summary.clone())));
    }
    Ok(execute && reads)
}

/// Refuses what a run of `statements` may not do.
fn check_statements(
    capabilities: &Capabilities,
    access: Access,
    statements: &[Statement],
    mode: RunMode,
    heavy: Heavy,
) -> Result<(), QueryError> {
    let reads_only = match (mode, access) {
        (RunMode::Write, Access::ReadOnly) => return Err(QueryError::WritesDisabled),
        (RunMode::Write, Access::ReadWrite) => false,
        (RunMode::Read, _) => true,
    };
    if statements.is_empty() {
        return Err(QueryError::Rejected(Rejection::Empty));
    }
    for statement in statements {
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
    Ok(())
}

/// Whether a write run of statements that do `infos` goes in one
/// transaction (see [`DocumentSession::all_or_nothing`]).
fn all_or_nothing<'a>(
    transactions: Transactions,
    mut infos: impl ExactSizeIterator<Item = &'a OpInfo>,
) -> bool {
    matches!(transactions, Transactions::Multi { .. })
        && (infos.len() > 1 || infos.any(|info| info.kind == StatementKind::Data))
}

/// What the statement a run stopped in had changed: what the engine said,
/// else nothing for a read or for one refused before it reached the
/// database, and otherwise unknown.
fn partly(info: &OpInfo, error: &QueryError, told: Option<Outcome>) -> Option<Outcome> {
    match (told, info.kind, error) {
        (Some(told), _, _) => Some(told),
        (None, StatementKind::Read, _)
        | (None, _, QueryError::Rejected(_) | QueryError::WritesDisabled) => {
            Some(Outcome::default())
        }
        (None, _, _) => None,
    }
}

/// Each statement's text, with what it does.
fn parts(
    language: &Language,
    text: &str,
    statements: Vec<Statement>,
) -> Result<Vec<(String, OpInfo)>, QueryError> {
    statements
        .into_iter()
        .map(|statement| {
            text.get(statement.span.clone())
                .map(|part| (part.to_string(), statement.info))
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

    /// The plan of one checked statement (see
    /// [`DocumentSession::explain_in`]).
    fn explain(
        &mut self,
        enter: Option<&str>,
        statement: &str,
        execute: bool,
        options: &RunOptions,
    ) -> Result<String, QueryError> {
        self.prepare(RunMode::Read, options, enter)?;
        self.begin_run();
        let planned = self.adapter.explain(statement, execute);
        self.end_run(planned)
    }

    /// Runs a script's statements (see [`DocumentSession::run_in`]); a
    /// write run that fails says what it left first.
    fn run(
        &mut self,
        enter: Option<&str>,
        statements: &[(String, OpInfo)],
        mode: RunMode,
        transactions: Transactions,
        options: &RunOptions,
        tx: &async_channel::Sender<DocEvent>,
    ) -> Result<DocSummary, QueryError> {
        self.run_leaving(enter, statements, mode, transactions, options, tx)
            .map_err(|(error, left)| {
                if mode == RunMode::Write {
                    let _ = tx.send_blocking(DocEvent::Left(left));
                }
                error
            })
    }

    /// Runs the statements; a failure says what the run left.
    fn run_leaving(
        &mut self,
        enter: Option<&str>,
        statements: &[(String, OpInfo)],
        mode: RunMode,
        transactions: Transactions,
        options: &RunOptions,
        tx: &async_channel::Sender<DocEvent>,
    ) -> Result<DocSummary, (QueryError, Left)> {
        let started = Instant::now();
        let nothing = |error| (error, Left::NOTHING);
        self.prepare(mode, options, enter).map_err(nothing)?;
        let atomic = mode == RunMode::Write
            && all_or_nothing(transactions, statements.iter().map(|(_, info)| info));
        if atomic {
            self.adapter.begin().map_err(nothing)?;
        }
        self.begin_run();
        let mut partial = Some(Outcome::default());
        let outcome = self.run_statements(statements, options, tx, started, &mut partial);
        match (mode, atomic) {
            (RunMode::Read, _) => self.end_run(outcome).map_err(nothing),
            (RunMode::Write, true) => {
                // Cancelled: undone, like any failure.
                let ended = match self.end_run(outcome) {
                    Ok(summary) => self.adapter.commit().map(|()| summary),
                    Err(error) => Err(WriteError::Undone(error)),
                };
                ended.map_err(|error| {
                    // Whatever the failure, nothing is left open.
                    let _ = self.adapter.abort();
                    match error {
                        WriteError::Undone(error) => (error, Left::Undone),
                        WriteError::Unknown(error) => (error, Left::Unknown),
                    }
                })
            }
            (RunMode::Write, false) => {
                // Kept as each statement ended: a late cancel undoes nothing.
                self.state.running.store(false, Ordering::SeqCst);
                self.state.cancelled.store(false, Ordering::SeqCst);
                outcome.map_err(|error| (error, Left::Kept { partial }))
            }
        }
    }

    /// Readies the adapter for a run: one that writes needs a session that
    /// may, and a run that starts in a container enters it.
    fn prepare(
        &mut self,
        mode: RunMode,
        options: &RunOptions,
        enter: Option<&str>,
    ) -> Result<(), QueryError> {
        if mode == RunMode::Write {
            self.writable()?;
        }
        self.adapter.limit(options)?;
        if let Some(enter) = enter {
            self.adapter.execute(enter, &mut Vec::new())?;
        }
        Ok(())
    }

    /// What each statement would do (see [`DocumentSession::preview_in`]).
    fn preview(
        &mut self,
        enter: Option<&str>,
        statements: &[(String, OpInfo)],
        preview: Preview,
        options: &RunOptions,
    ) -> Result<Vec<Impact>, QueryError> {
        self.prepare(RunMode::Write, options, enter)?;
        self.begin_run();
        let mut impacts = Vec::with_capacity(statements.len());
        let previewed = match preview {
            Preview::Undone => self.undone(statements, &mut impacts),
            Preview::Estimated | Preview::None => Ok(None),
        }
        .and_then(|note| self.estimated(statements, preview, note, &mut impacts));
        self.end_run(previewed).map(|()| impacts)
    }

    /// Runs the statements in a transaction while each can, and always
    /// undoes it. Returns why it stopped before the end, if a statement
    /// failed there.
    fn undone(
        &mut self,
        statements: &[(String, OpInfo)],
        impacts: &mut Vec<Impact>,
    ) -> Result<Option<String>, QueryError> {
        if !statements.first().is_some_and(|(_, info)| undoable(info)) {
            return Ok(None);
        }
        self.adapter.begin()?;
        let ran = self.run_undone(statements, impacts);
        // Whatever happened, nothing is kept and nothing is left open.
        let aborted = self.adapter.abort();
        let note = ran?;
        aborted?;
        Ok(note)
    }

    fn run_undone(
        &mut self,
        statements: &[(String, OpInfo)],
        impacts: &mut Vec<Impact>,
    ) -> Result<Option<String>, QueryError> {
        for (text, info) in statements {
            if self.state.cancelled.load(Ordering::SeqCst) {
                return Err(QueryError::Cancelled);
            }
            if !undoable(info) {
                return Ok(None);
            }
            let mut sample = Sample::default();
            match self.adapter.execute(text, &mut sample) {
                Ok(outcome) => impacts.push(Impact {
                    summary: info.summary.clone(),
                    estimate: match info.kind {
                        StatementKind::Read => Estimate::Reads,
                        _ => Estimate::Exact(outcome),
                    },
                    sample: sample.0,
                    note: None,
                }),
                Err(error) if fatal(&error) => return Err(error),
                Err(error) => {
                    return Ok(Some(format!(
                        "It failed in the transaction the preview undid: {error}"
                    )));
                }
            }
        }
        Ok(None)
    }

    /// Counts what the statements the transaction did not reach would
    /// change; the first of them gets `note`.
    fn estimated(
        &mut self,
        statements: &[(String, OpInfo)],
        preview: Preview,
        mut note: Option<String>,
        impacts: &mut Vec<Impact>,
    ) -> Result<(), QueryError> {
        for (text, info) in &statements[impacts.len()..] {
            if self.state.cancelled.load(Ordering::SeqCst) {
                return Err(QueryError::Cancelled);
            }
            let mut impact = Impact {
                summary: info.summary.clone(),
                estimate: Estimate::Unknown,
                sample: Vec::new(),
                note: note.take(),
            };
            if info.kind == StatementKind::Read {
                // Reads change nothing; one that changes where the next
                // statements work still has to run.
                if info.enters.is_some() && preview != Preview::None {
                    self.adapter.execute(text, &mut Vec::new())?;
                }
                impact.estimate = Estimate::Reads;
            } else if preview != Preview::None {
                let mut sample = Sample::default();
                match self.adapter.estimate(text, &mut sample) {
                    Ok(Some(Count::Unknown) | None) => {}
                    Ok(Some(count)) => impact.estimate = Estimate::Estimated(count),
                    Err(error) if fatal(&error) => return Err(error),
                    Err(error) => {
                        let counted = format!("It could not be counted: {error}");
                        impact.note = Some(match impact.note {
                            Some(note) => format!("{note}. {counted}"),
                            None => counted,
                        });
                    }
                }
                impact.sample = sample.0;
            }
            impacts.push(impact);
        }
        Ok(())
    }

    /// Runs each statement in turn, streaming their documents and what
    /// each did, and stops at the first failure. `partial` ends as what the
    /// statement it stopped in had changed (see [`Left::Kept`]).
    fn run_statements(
        &mut self,
        statements: &[(String, OpInfo)],
        options: &RunOptions,
        tx: &async_channel::Sender<DocEvent>,
        started: Instant,
        partial: &mut Option<Outcome>,
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
        for (statement, info) in statements {
            // A cancel stops the script before its next statement, even when
            // the one it reached ended well: what ran keeps its outcome.
            if self.state.cancelled.load(Ordering::SeqCst) {
                return Err(QueryError::Cancelled);
            }
            stream.next_statement();
            let outcome = match self.adapter.execute(statement, &mut stream) {
                Ok(outcome) => outcome,
                Err(error) => {
                    stream.stop_on_the_server(&error, &self.canceller);
                    *partial = partly(info, &error, stream.changed);
                    return Err(error);
                }
            };
            // The statement ended: a reader gone now has nothing to stop, and
            // what it did stays even if it never hears of it.
            *partial = Some(outcome);
            stream.flush()?;
            stream.send(DocEvent::Outcome(outcome))?;
            *partial = Some(Outcome::default());
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
        let none_kept = |error| ApplyFailure {
            applied: 0,
            unknown: 0,
            error,
        };
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
                // Undone with the transaction, whatever it did.
                Err(error) if atomic => {
                    let _ = self.adapter.abort();
                    return Err(none_kept(error.error()));
                }
                Err(error) => {
                    let unknown = usize::from(matches!(error, WriteError::Unknown(_)));
                    return Err(ApplyFailure {
                        applied: kept,
                        unknown,
                        error: error.error(),
                    });
                }
            }
        }
        if atomic && let Err(error) = self.adapter.commit() {
            let _ = self.adapter.abort();
            let unknown = match error {
                WriteError::Unknown(_) => changes.len(),
                WriteError::Undone(_) => 0,
            };
            return Err(ApplyFailure {
                applied: 0,
                unknown,
                error: error.error(),
            });
        }
        Ok(total)
    }

    /// Applies one change and checks it did what its kind expects.
    fn apply_one(
        &mut self,
        collection: &CollectionRef,
        change: &DocChange,
        render: fn(&Value, Style) -> String,
    ) -> Result<Outcome, WriteError> {
        let outcome = self.adapter.apply(collection, change)?;
        let (expected, actual) = match change.expected() {
            Expected::Matched(expected) => (expected, outcome.matched),
            Expected::Inserted(expected) => (expected, outcome.inserted),
        };
        if actual != expected {
            return Err(WriteError::Undone(QueryError::RowMismatch {
                sql: change.describe(render),
                expected,
                actual,
            }));
        }
        Ok(outcome)
    }
}

/// Whether a statement can run in a preview's transaction: reads and
/// changes to documents, never a schema change or what the language does
/// not know.
fn undoable(info: &OpInfo) -> bool {
    matches!(info.kind, StatementKind::Read | StatementKind::Data)
}

/// An error that ends a preview, rather than one statement's part of it.
fn fatal(error: &QueryError) -> bool {
    matches!(
        error,
        QueryError::Cancelled | QueryError::Connection(_) | QueryError::WritesDisabled
    )
}

/// The first documents of a preview's statement.
#[derive(Default)]
struct Sample(Vec<Document>);

impl DocumentSink for Sample {
    fn documents(&mut self, batch: Vec<Document>) -> Result<(), QueryError> {
        let room = PREVIEW_SAMPLE.saturating_sub(self.0.len());
        self.0.extend(batch.into_iter().take(room));
        Ok(())
    }

    fn full(&self) -> bool {
        self.0.len() >= PREVIEW_SAMPLE
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
    /// Documents the statement that runs gave so far, up to
    /// [`RunOptions::max_documents`].
    taken: usize,
    first_batch_after: Option<Duration>,
    /// What the statement that runs changed before it failed, as the
    /// adapter told.
    changed: Option<Outcome>,
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
            taken: 0,
            first_batch_after: None,
            changed: None,
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

    /// The next statement starts: it gets its own
    /// [`RunOptions::max_documents`].
    fn next_statement(&mut self) {
        self.taken = 0;
        self.changed = None;
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
    fn documents(&mut self, mut batch: Vec<Document>) -> Result<(), QueryError> {
        if let Some(cap) = self.options.max_documents {
            batch.truncate(cap.saturating_sub(self.taken));
        }
        self.taken += batch.len();
        self.pending.extend(batch);
        while self.pending.len() >= self.batch {
            let rest = self.pending.split_off(self.batch);
            let full = std::mem::replace(&mut self.pending, rest);
            self.batch = (self.batch * 4).min(self.options.max_batch_rows.max(1));
            self.ship(full)?;
        }
        Ok(())
    }

    fn full(&self) -> bool {
        self.options
            .max_documents
            .is_some_and(|cap| self.taken >= cap)
    }

    fn changed(&mut self, outcome: Outcome) {
        self.changed = Some(outcome);
    }
}

#[cfg(test)]
mod tests;
