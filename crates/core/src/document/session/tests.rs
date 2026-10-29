//! The session's rules, against a fake document adapter that logs what it
//! is asked to do. Its language splits a script on `;` and classifies each
//! statement by its first word: `read N` returns N documents `{n: i}`,
//! `write N` changes N, `ddl x` changes the schema, `heavy` is a read that
//! weighs on the server, `begin` is transaction control, and any other word
//! counts as a write, as anything a language cannot read must. A `!`
//! anywhere is a parse error. `read slow` runs until cancelled and then
//! ends as if nothing happened, `read interrupted` runs until cancelled and
//! fails as cancelled, `read endless` streams until the reader goes away,
//! `read panic` and `write panic` panic, and `write fail` fails. The fake
//! holds documents with keys 1 to 3: changes to other keys find nothing,
//! and an insert of `{_id: 0}` inserts nothing.

use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use futures::executor::block_on;

use super::*;
use crate::adapter::Canceller;
use crate::document::capabilities::{Enforcement, Preview, Sampling};
use crate::document::catalog::ContainerKind;
use crate::document::language::{CompletionContext, FilterCheck, OpInfo, ParseError, Statement};
use crate::document::value::DocKey;

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn push(&self, entry: impl Into<String>) {
        self.0.lock().unwrap().push(entry.into());
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

fn analyze(text: &str) -> Result<Vec<Statement>, ParseError> {
    if let Some(at) = text.find('!') {
        return Err(ParseError {
            span: at..at + 1,
            message: "unexpected !".into(),
        });
    }
    let mut statements = Vec::new();
    let mut start = 0;
    for part in text.split(';') {
        let span = start..start + part.len();
        start = span.end + 1;
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let offset = part.find(trimmed).unwrap_or(0);
        let kind = match trimmed.split_whitespace().next() {
            Some("read" | "heavy") => StatementKind::Read,
            Some("ddl") => StatementKind::Schema,
            Some("begin") => StatementKind::Transaction,
            _ => StatementKind::Data,
        };
        statements.push(Statement {
            span: span.start + offset..span.start + offset + trimmed.len(),
            info: OpInfo {
                kind,
                target: None,
                summary: trimmed.to_string(),
                dangers: Vec::new(),
                heavy: trimmed == "heavy",
                explainable: false,
            },
        });
    }
    Ok(statements)
}

fn check_filter(text: &str) -> Result<FilterCheck, ParseError> {
    match text.find('!') {
        Some(at) => Err(ParseError {
            span: at..at + 1,
            message: "unexpected !".into(),
        }),
        None => Ok(FilterCheck {
            heavy: text.contains("$where"),
        }),
    }
}

fn render(value: &Value, _: Style) -> String {
    match value {
        Value::Int32(n) => n.to_string(),
        other => format!("{other:?}"),
    }
}

static FAKE: Language = Language {
    name: "fake",
    editor: "text",
    analyze,
    check_filter,
    parse_document: |_| Ok(Document::default()),
    render,
    field_filter: |_, _| None,
    context: |_, _| CompletionContext::Nothing,
    methods: &[],
    operators: &[],
    stages: &[],
};

fn capabilities(transactions: Transactions) -> Capabilities {
    Capabilities {
        levels: &["Database", "Collection"],
        transactions,
        read_only: Enforcement::ClientOnly("it is a fake"),
        preview: Preview::None,
        explain: None,
        activity: false,
        kill: false,
        sampling: Sampling::FirstN,
        version_token: false,
    }
}

const MULTI: Transactions = Transactions::Multi {
    lifetime: Duration::from_secs(60),
};

struct FakeDocs {
    log: Log,
    stop: Arc<AtomicBool>,
    transactions: Transactions,
}

fn numbered(range: std::ops::Range<usize>) -> Vec<Document> {
    range
        .map(|n| Document(vec![("n".into(), Value::Int32(n as i32))]))
        .collect()
}

impl FakeDocs {
    fn until_stopped(&self) {
        while !self.stop.swap(false, Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn holds(key: &DocKey) -> bool {
        matches!(key.0, Value::Int32(1..=3))
    }
}

impl DocumentAdapter for FakeDocs {
    fn language(&self) -> &'static Language {
        &FAKE
    }

    fn server(&self) -> String {
        "Fake Docs 1.0".into()
    }

    fn capabilities(&self) -> Capabilities {
        capabilities(self.transactions)
    }

    fn notice(&self) -> Option<String> {
        Some("Kuery refuses writes by itself".into())
    }

    fn limit(&mut self, _: &RunOptions) -> Result<(), QueryError> {
        Ok(())
    }

    fn canceller(&self) -> Canceller {
        let (stop, log) = (self.stop.clone(), self.log.clone());
        Arc::new(move || {
            log.push("cancel");
            stop.store(true, Ordering::SeqCst);
        })
    }

    fn execute(
        &mut self,
        statement: &str,
        sink: &mut dyn DocumentSink,
    ) -> Result<Outcome, QueryError> {
        self.log.push(statement);
        let words: Vec<&str> = statement.split_whitespace().collect();
        match words.as_slice() {
            [_, "panic"] => panic!("the fake driver broke"),
            [_, "fail"] => Err(QueryError::Database {
                code: "11000".into(),
                message: "failed on purpose".into(),
            }),
            ["read", "slow"] => {
                self.until_stopped();
                sink.documents(numbered(0..1))?;
                Ok(Outcome {
                    returned: 1,
                    ..Outcome::default()
                })
            }
            ["read", "interrupted"] => {
                self.until_stopped();
                Err(QueryError::Cancelled)
            }
            ["read", "endless"] => loop {
                sink.documents(numbered(0..10))?;
            },
            ["read", n] => {
                let count: usize = n.parse().unwrap_or(1);
                // In small chunks, as a driver's cursor gives them.
                for start in (0..count).step_by(50) {
                    sink.documents(numbered(start..(start + 50).min(count)))?;
                }
                Ok(Outcome {
                    returned: count as u64,
                    ..Outcome::default()
                })
            }
            ["write", n] => {
                let changed = n.parse().unwrap_or(0);
                Ok(Outcome {
                    matched: changed,
                    modified: changed,
                    ..Outcome::default()
                })
            }
            _ => Ok(Outcome::default()),
        }
    }

    fn begin(&mut self) -> Result<(), QueryError> {
        self.log.push("begin");
        Ok(())
    }

    fn commit(&mut self) -> Result<(), QueryError> {
        self.log.push("commit");
        Ok(())
    }

    fn abort(&mut self) -> Result<(), QueryError> {
        self.log.push("abort");
        Ok(())
    }

    fn children(&mut self, parent: &[String]) -> Result<Vec<Container>, QueryError> {
        self.log.push(format!("children of {parent:?}"));
        let (name, kind, leaf) = match parent {
            [] => ("shop", ContainerKind::Database, false),
            _ => ("orders", ContainerKind::Collection, true),
        };
        Ok(vec![Container {
            name: name.into(),
            kind,
            leaf,
            estimated: None,
            size: None,
        }])
    }

    fn collection(&mut self, collection: &CollectionRef) -> Result<CollectionInfo, QueryError> {
        self.log
            .push(format!("collection {}", collection.display()));
        Ok(CollectionInfo {
            kind: ContainerKind::Collection,
            indexes: Vec::new(),
            validator: None,
            options: Document::default(),
        })
    }

    fn sample(
        &mut self,
        collection: &CollectionRef,
        n: usize,
        sink: &mut dyn DocumentSink,
    ) -> Result<(), QueryError> {
        self.log
            .push(format!("sample {n} of {}", collection.display()));
        sink.documents(numbered(0..n))
    }

    fn browse(
        &mut self,
        collection: &CollectionRef,
        spec: &BrowseSpec,
        sink: &mut dyn DocumentSink,
    ) -> Result<Option<Continuation>, QueryError> {
        self.log
            .push(format!("browse {} {:?}", collection.display(), spec.filter));
        sink.documents(numbered(0..spec.limit))?;
        Ok(Some(Continuation::Skip(spec.limit as u64)))
    }

    fn count(
        &mut self,
        collection: &CollectionRef,
        filter: Option<&str>,
        _: u64,
    ) -> Result<Count, QueryError> {
        self.log
            .push(format!("count {} {filter:?}", collection.display()));
        Ok(Count::Exact(42))
    }

    fn apply(&mut self, _: &CollectionRef, change: &DocChange) -> Result<Outcome, QueryError> {
        self.log.push(change.describe(render));
        let found = |key| u64::from(FakeDocs::holds(key));
        Ok(match change {
            DocChange::Insert { document } => Outcome {
                inserted: u64::from(document.key() != Some(DocKey(Value::Int32(0)))),
                ..Outcome::default()
            },
            DocChange::Update { key, .. } | DocChange::Replace { key, .. } => Outcome {
                matched: found(key),
                modified: found(key),
                ..Outcome::default()
            },
            DocChange::Delete { key, .. } => Outcome {
                matched: found(key),
                deleted: found(key),
                ..Outcome::default()
            },
        })
    }
}

fn open_with(transactions: Transactions, access: Access) -> (DocumentSession, Log) {
    let log = Log::default();
    let opener_log = log.clone();
    let opener: DocumentOpener = Arc::new(move |_| {
        Ok(Box::new(FakeDocs {
            log: opener_log.clone(),
            stop: Arc::new(AtomicBool::new(false)),
            transactions,
        }))
    });
    (
        block_on(DocumentSession::open(opener, access)).unwrap(),
        log,
    )
}

fn open(access: Access) -> (DocumentSession, Log) {
    open_with(Transactions::None, access)
}

/// What a run sent: the size of each batch of documents, each statement's
/// outcome, and how it ended.
struct Drained {
    batches: Vec<usize>,
    outcomes: Vec<Outcome>,
    end: Result<DocSummary, QueryError>,
}

fn drain(rx: async_channel::Receiver<DocEvent>) -> Drained {
    block_on(async {
        let mut drained = Drained {
            batches: Vec::new(),
            outcomes: Vec::new(),
            end: Err(QueryError::Cancelled),
        };
        while let Ok(event) = rx.recv().await {
            match event {
                DocEvent::Documents(batch) => drained.batches.push(batch.len()),
                DocEvent::Outcome(outcome) => drained.outcomes.push(outcome),
                DocEvent::Finished(summary) => {
                    drained.end = Ok(summary);
                    break;
                }
                DocEvent::Failed(error) => {
                    drained.end = Err(error);
                    break;
                }
            }
        }
        drained
    })
}

fn run(session: &DocumentSession, text: &str, mode: RunMode) -> Drained {
    drain(session.run(text.into(), mode, Heavy::Refuse, RunOptions::default()))
}

fn orders() -> CollectionRef {
    CollectionRef::new(&["shop", "orders"])
}

fn update(key: i32) -> DocChange {
    DocChange::Update {
        key: DocKey(Value::Int32(key)),
        version: None,
        set: vec![(
            crate::document::value::FieldPath::field("status"),
            Value::String("paid".into()),
        )],
        unset: Vec::new(),
        expect: Vec::new(),
    }
}

fn insert(key: i32) -> DocChange {
    DocChange::Insert {
        document: Document(vec![("_id".into(), Value::Int32(key))]),
    }
}

#[test]
fn rejects_writes_on_a_read_only_session_before_the_adapter_sees_them() {
    let (session, log) = open(Access::ReadOnly);
    assert_eq!(session.server_label(), "Fake Docs 1.0");
    assert_eq!(session.notice(), Some("Kuery refuses writes by itself"));
    let refused = run(&session, "read 1; write 1", RunMode::Read).end;
    assert!(
        matches!(&refused, Err(QueryError::Rejected(Rejection::Statement(s))) if s == "write 1"),
        "{refused:?}"
    );
    // What the language cannot read counts as a write.
    let unknown = run(&session, "frobnicate", RunMode::Read).end;
    assert!(
        matches!(unknown, Err(QueryError::Rejected(Rejection::Statement(_)))),
        "{unknown:?}"
    );
    let write = run(&session, "read 1", RunMode::Write).end;
    assert!(
        matches!(write, Err(QueryError::WritesDisabled)),
        "{write:?}"
    );
    // Refused before it is even read.
    let unreadable = run(&session, "write !", RunMode::Write).end;
    assert!(matches!(unreadable, Err(QueryError::WritesDisabled)));
    let applied = block_on(session.apply(orders(), vec![insert(4)]));
    assert_eq!(
        applied,
        Err(ApplyFailure {
            applied: 0,
            error: QueryError::WritesDisabled
        })
    );
    assert!(log.take().is_empty());
}

#[test]
fn a_read_run_refuses_writes_even_when_the_session_may_write() {
    let (session, log) = open(Access::ReadWrite);
    let refused = run(&session, "read 1; write 1", RunMode::Read).end;
    assert!(
        matches!(refused, Err(QueryError::Rejected(Rejection::Statement(_)))),
        "{refused:?}"
    );
    assert!(log.take().is_empty());
    let read = run(&session, "read 3", RunMode::Read);
    assert_eq!(read.end.unwrap().documents, 3);
    assert_eq!(log.take(), ["read 3"]);
}

#[test]
fn says_where_a_script_cannot_be_read_and_refuses_an_empty_one() {
    let (session, log) = open(Access::ReadWrite);
    let refused = run(&session, "read 1;\n read !", RunMode::Read).end;
    assert!(
        matches!(&refused, Err(QueryError::Rejected(Rejection::Parse(m))) if m == "line 2, column 7: unexpected !"),
        "{refused:?}"
    );
    let empty = run(&session, " ; ", RunMode::Write).end;
    assert!(
        matches!(empty, Err(QueryError::Rejected(Rejection::Empty))),
        "{empty:?}"
    );
    // Kuery owns the transaction of every script.
    let begin = run(&session, "begin; write 1", RunMode::Write).end;
    assert!(
        matches!(begin, Err(QueryError::Unsupported(_))),
        "{begin:?}"
    );
    assert!(log.take().is_empty());
}

#[test]
fn heavy_runs_only_when_allowed() {
    let (session, log) = open(Access::ReadWrite);
    let refused = run(&session, "read 1; heavy", RunMode::Read).end;
    assert!(
        matches!(&refused, Err(QueryError::Rejected(Rejection::Heavy(s))) if s == "heavy"),
        "{refused:?}"
    );
    let browse = drain(session.browse(
        orders(),
        BrowseSpec {
            filter: Some("{$where: 'sleep(100)'}".into()),
            limit: 10,
            ..BrowseSpec::default()
        },
        Heavy::Refuse,
        RunOptions::default(),
    ));
    assert!(
        matches!(browse.end, Err(QueryError::Rejected(Rejection::Heavy(_)))),
        "{:?}",
        browse.end
    );
    let counted = block_on(session.count(orders(), Some("{$where: 1}".into()), 10, Heavy::Refuse));
    assert!(matches!(
        counted,
        Err(QueryError::Rejected(Rejection::Heavy(_)))
    ));
    let projection = drain(session.browse(
        orders(),
        BrowseSpec {
            projection: Some("{a: !}".into()),
            ..BrowseSpec::default()
        },
        Heavy::Allow,
        RunOptions::default(),
    ));
    assert!(matches!(
        projection.end,
        Err(QueryError::Rejected(Rejection::Parse(_)))
    ));
    assert!(log.take().is_empty());

    let allowed = drain(session.run(
        "heavy".into(),
        RunMode::Read,
        Heavy::Allow,
        RunOptions::default(),
    ));
    allowed.end.unwrap();
    let counted = block_on(session.count(orders(), Some("{$where: 1}".into()), 10, Heavy::Allow));
    assert_eq!(counted, Ok(Count::Exact(42)));
    assert_eq!(
        log.take(),
        ["heavy", "count shop.orders Some(\"{$where: 1}\")"]
    );
}

#[test]
fn streams_documents_in_growing_batches() {
    let (session, _) = open(Access::ReadOnly);
    let read = drain(session.run(
        "read 2600".into(),
        RunMode::Read,
        Heavy::Refuse,
        RunOptions {
            first_batch_rows: 100,
            ..RunOptions::default()
        },
    ));
    assert_eq!(read.batches, [100, 400, 1600, 500]);
    assert_eq!(
        read.outcomes,
        [Outcome {
            returned: 2600,
            ..Outcome::default()
        }]
    );
    let summary = read.end.unwrap();
    assert_eq!((summary.documents, summary.statements), (2600, 1));
    assert!(summary.first_batch_after.is_some());

    // Each statement's documents, then what it did.
    let two = run(&session, "read 2; read 3", RunMode::Read);
    assert_eq!(two.batches, [2, 3]);
    assert_eq!(two.end.unwrap().outcome.returned, 5);
}

#[test]
fn browses_a_page_and_says_where_the_next_starts() {
    let (session, log) = open(Access::ReadOnly);
    let page = drain(session.browse(
        orders(),
        BrowseSpec {
            filter: Some("{status: 'paid'}".into()),
            limit: 30,
            ..BrowseSpec::default()
        },
        Heavy::Refuse,
        RunOptions::default(),
    ));
    let summary = page.end.unwrap();
    assert_eq!(summary.documents, 30);
    assert_eq!(summary.next, Some(Continuation::Skip(30)));
    assert_eq!(
        log.take(),
        ["browse shop.orders Some(\"{status: 'paid'}\")"]
    );
}

#[test]
fn reads_the_catalog() {
    let (session, log) = open(Access::ReadOnly);
    let databases = block_on(session.children(Vec::new())).unwrap();
    assert_eq!(databases[0].kind, ContainerKind::Database);
    let collections = block_on(session.children(vec!["shop".into()])).unwrap();
    assert!(collections[0].leaf);
    let info = block_on(session.collection(orders())).unwrap();
    assert_eq!(info.kind, ContainerKind::Collection);
    let sample = block_on(session.sample(orders(), 5)).unwrap();
    assert_eq!(sample.len(), 5);
    let count = block_on(session.count(orders(), None, 1000, Heavy::Refuse));
    assert_eq!(count, Ok(Count::Exact(42)));
    assert_eq!(
        log.take(),
        [
            "children of []",
            "children of [\"shop\"]",
            "collection shop.orders",
            "sample 5 of shop.orders",
            "count shop.orders None"
        ]
    );
    assert_eq!(session.capabilities().levels, ["Database", "Collection"]);
}

#[test]
fn a_write_script_runs_in_one_transaction_where_there_are_transactions() {
    let (session, log) = open_with(MULTI, Access::ReadWrite);
    let ran = run(&session, "write 1; write 2", RunMode::Write);
    assert_eq!(ran.outcomes.len(), 2);
    let summary = ran.end.unwrap();
    assert_eq!((summary.outcome.modified, summary.statements), (3, 2));
    assert_eq!(log.take(), ["begin", "write 1", "write 2", "commit"]);

    let failed = run(&session, "write 1; write fail", RunMode::Write);
    assert!(matches!(failed.end, Err(QueryError::Database { .. })));
    assert_eq!(log.take(), ["begin", "write 1", "write fail", "abort"]);

    // A single statement needs no transaction, and reads take none.
    run(&session, "write 1", RunMode::Write).end.unwrap();
    run(&session, "read 1; read 2", RunMode::Read).end.unwrap();
    assert_eq!(log.take(), ["write 1", "read 1", "read 2"]);

    // Cancelled halfway: undone.
    let rx = session.run(
        "write 1; read slow".into(),
        RunMode::Write,
        Heavy::Refuse,
        RunOptions::default(),
    );
    std::thread::sleep(Duration::from_millis(50));
    session.cancel();
    assert!(matches!(drain(rx).end, Err(QueryError::Cancelled)));
    let logged = log.take();
    assert_eq!(logged.first().map(String::as_str), Some("begin"));
    assert_eq!(logged.last().map(String::as_str), Some("abort"));
}

#[test]
fn a_write_script_stops_at_the_first_failure_without_transactions() {
    let (session, log) = open(Access::ReadWrite);
    let ran = run(
        &session,
        "write 1; write 2; write fail; write 3",
        RunMode::Write,
    );
    // What ran is what the outcomes tell.
    assert_eq!(ran.outcomes.len(), 2);
    assert!(matches!(ran.end, Err(QueryError::Database { .. })));
    assert_eq!(log.take(), ["write 1", "write 2", "write fail"]);

    // Cancelled halfway: what ran stays, and nothing after it runs.
    let rx = session.run(
        "write 1; read slow; write 2".into(),
        RunMode::Write,
        Heavy::Refuse,
        RunOptions::default(),
    );
    std::thread::sleep(Duration::from_millis(50));
    session.cancel();
    let cancelled = drain(rx);
    assert!(matches!(cancelled.end, Err(QueryError::Cancelled)));
    assert_eq!(cancelled.outcomes.len(), 2);
    assert_eq!(log.take(), ["write 1", "read slow", "cancel"]);
}

#[test]
fn schema_changes_are_refused_inside_a_transaction() {
    let (session, log) = open_with(MULTI, Access::ReadWrite);
    let refused = run(&session, "write 1; ddl x", RunMode::Write).end;
    assert!(
        matches!(&refused, Err(QueryError::Unsupported(m)) if m.starts_with("ddl x cannot run inside a transaction")),
        "{refused:?}"
    );
    assert!(log.take().is_empty());
    run(&session, "ddl x", RunMode::Write).end.unwrap();
    assert_eq!(log.take(), ["ddl x"]);

    // Without transactions there is nothing to refuse it from.
    let (session, log) = open(Access::ReadWrite);
    run(&session, "write 1; ddl x", RunMode::Write).end.unwrap();
    assert_eq!(log.take(), ["write 1", "ddl x"]);
}

#[test]
fn apply_checks_each_change_by_its_kind() {
    let (session, log) = open(Access::ReadWrite);
    let key = |key| DocKey(Value::Int32(key));
    let applied = block_on(session.apply(
        orders(),
        vec![
            update(1),
            insert(4),
            DocChange::Delete {
                key: key(2),
                version: None,
                expect: Vec::new(),
            },
        ],
    ))
    .unwrap();
    assert_eq!(
        (applied.matched, applied.inserted, applied.deleted),
        (2, 1, 1)
    );
    assert_eq!(log.take().len(), 3);

    let gone = [
        update(9),
        DocChange::Replace {
            key: key(9),
            version: None,
            document: Document::default(),
        },
        DocChange::Delete {
            key: key(9),
            version: None,
            expect: Vec::new(),
        },
    ];
    for change in gone {
        let failure = block_on(session.apply(orders(), vec![change.clone()])).unwrap_err();
        assert_eq!(
            failure.error,
            QueryError::RowMismatch {
                sql: change.describe(render),
                expected: 1,
                actual: 0
            }
        );
    }
    // An insert is checked by what it inserted, not what it matched.
    let failure = block_on(session.apply(orders(), vec![insert(0)])).unwrap_err();
    assert_eq!(
        failure.error,
        QueryError::RowMismatch {
            sql: "insert of Object(Document([(\"_id\", Int32(0))]))".into(),
            expected: 1,
            actual: 0
        }
    );
    assert_eq!(
        block_on(session.apply(orders(), Vec::new())),
        Ok(Outcome::default())
    );
}

#[test]
fn apply_without_transactions_says_what_was_kept() {
    let (session, log) = open(Access::ReadWrite);
    let failure = block_on(session.apply(orders(), vec![update(1), update(9), update(2)]));
    assert_eq!(failure.as_ref().map_err(|f| f.applied), Err(1));
    assert_eq!(log.take().len(), 2, "it stops at the failure");

    let (session, log) = open_with(MULTI, Access::ReadWrite);
    let failure = block_on(session.apply(orders(), vec![update(1), update(9), update(2)]));
    assert_eq!(failure.as_ref().map_err(|f| f.applied), Err(0));
    let logged = log.take();
    assert_eq!(
        (
            logged.first().map(String::as_str),
            logged.last().map(String::as_str)
        ),
        (Some("begin"), Some("abort"))
    );
    block_on(session.apply(orders(), vec![update(1), insert(5)])).unwrap();
    let logged = log.take();
    assert_eq!(logged.last().map(String::as_str), Some("commit"));
}

#[test]
fn a_job_that_panics_ends_the_session_and_says_so() {
    let (session, _) = open(Access::ReadWrite);
    let outcome = run(&session, "write panic", RunMode::Write).end;
    assert!(
        matches!(outcome, Err(QueryError::Connection(_))),
        "{outcome:?}"
    );
    let next = run(&session, "read 1", RunMode::Read).end;
    assert!(matches!(next, Err(QueryError::Connection(_))), "{next:?}");
    let children = block_on(session.children(Vec::new()));
    assert!(matches!(children, Err(QueryError::Connection(_))));
    let applied = block_on(session.apply(orders(), vec![update(1)]));
    assert!(matches!(
        applied,
        Err(ApplyFailure {
            applied: 0,
            error: QueryError::Connection(_)
        })
    ));
}

#[test]
fn a_cancelled_read_counts_as_cancelled_even_if_it_ends_well() {
    let (session, _) = open(Access::ReadOnly);
    let rx = session.run(
        "read slow".into(),
        RunMode::Read,
        Heavy::Refuse,
        RunOptions::default(),
    );
    std::thread::sleep(Duration::from_millis(50));
    session.cancel();
    assert!(matches!(drain(rx).end, Err(QueryError::Cancelled)));
    run(&session, "read 1", RunMode::Read).end.unwrap();
}

#[test]
fn a_cancel_does_not_reach_the_next_run() {
    let (session, log) = open(Access::ReadOnly);
    let rx = session.run(
        "read interrupted".into(),
        RunMode::Read,
        Heavy::Refuse,
        RunOptions::default(),
    );
    std::thread::sleep(Duration::from_millis(50));
    session.cancel();
    assert!(matches!(drain(rx).end, Err(QueryError::Cancelled)));
    // Started while the cancel's retries could still be waiting.
    let next = session.run(
        "read slow".into(),
        RunMode::Read,
        Heavy::Refuse,
        RunOptions::default(),
    );
    std::thread::sleep(Duration::from_millis(400));
    let ended = std::iter::from_fn(|| next.try_recv().ok())
        .any(|event| matches!(event, DocEvent::Finished(_) | DocEvent::Failed(_)));
    assert!(!ended, "the next run was stopped by the earlier cancel");
    session.cancel();
    assert!(matches!(drain(next).end, Err(QueryError::Cancelled)));
    let cancels = log.take().into_iter().filter(|e| e == "cancel").count();
    assert_eq!(cancels, 2, "one cancel for each run");
}

#[test]
fn a_reader_that_goes_away_stops_the_run() {
    let (session, log) = open(Access::ReadOnly);
    drop(session.run(
        "read endless".into(),
        RunMode::Read,
        Heavy::Refuse,
        RunOptions::default(),
    ));
    run(&session, "read 1", RunMode::Read).end.unwrap();
    let log = log.take();
    let endless = log.iter().position(|e| e == "read endless").unwrap();
    assert_eq!(
        log.get(endless + 1).map(String::as_str),
        Some("cancel"),
        "{log:?}"
    );
}

#[test]
fn reopens_another_session_alike() {
    let (session, _) = open_with(MULTI, Access::ReadWrite);
    let other = block_on(session.reopen(Access::ReadOnly)).unwrap();
    assert_eq!(other.access(), Access::ReadOnly);
    assert_eq!(other.language().name, "fake");
    assert_eq!(other.capabilities().transactions, MULTI);
    let refused = run(&other, "write 1", RunMode::Write).end;
    assert!(matches!(refused, Err(QueryError::WritesDisabled)));
}
