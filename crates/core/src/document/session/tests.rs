//! The session's rules, against a fake document adapter that logs what it
//! is asked to do. Its language splits a script on `;` and classifies each
//! statement by its first word: `read N` returns N documents `{n: i}`,
//! `write N` changes N, `ddl x` changes the schema, `heavy` is a read that
//! weighs on the server, `begin` is transaction control, and any other word
//! counts as a write, as anything a language cannot read must. A `!`
//! anywhere is a parse error. `read slow` runs until cancelled and then
//! ends as if nothing happened, `read interrupted` runs until cancelled and
//! fails as cancelled, `read endless` streams until the reader goes away,
//! `read panic` and `write panic` panic, and `write fail` fails. `write
//! partial` changes two documents and then fails, saying so; `write cut`
//! fails as a lost connection, without saying what it changed; after
//! `write lost`, the commit does not say whether it applied. Entering a
//! container runs `enter <name>`. The fake holds documents with keys 1 to
//! 3: changes to other keys find nothing, and an insert of `{_id: 0}`
//! inserts nothing. A change to key 5 gets no answer, and after an insert
//! of `{_id: 6}` the commit does not say whether it applied.

use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use futures::executor::block_on;

use super::*;
use crate::adapter::Canceller;
use crate::document::capabilities::{Enforcement, ExplainKind, Preview, Sampling};
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
                explainable: !trimmed.starts_with("show"),
                enters: None,
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
    enter: |path| path.first().map(|name| format!("enter {name}")),
    collection_ref: |name| name.to_string(),
    key: |name| name.to_string(),
    change: |_, change| change.describe(render),
    check_change: |_| Ok(()),
    key_name: "_id",
    methods: &[],
    operators: &[],
    stages: &[],
};

fn capabilities(transactions: Transactions, preview: Preview) -> Capabilities {
    Capabilities {
        levels: &["Database", "Collection"],
        transactions,
        read_only: Enforcement::ClientOnly("it is a fake"),
        preview,
        explain: Some(ExplainKind::Measured),
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
    preview: Preview,
    /// The commit will not say whether it applied.
    lost: bool,
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
        capabilities(self.transactions, self.preview)
    }

    fn notice(&self) -> Option<String> {
        Some("Kuery refuses writes by itself".into())
    }

    fn home(&self) -> Vec<String> {
        vec!["shop".into()]
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
            ["write", "partial"] => {
                sink.changed(Outcome {
                    inserted: 2,
                    ..Outcome::default()
                });
                Err(QueryError::Database {
                    code: "11000".into(),
                    message: "failed halfway".into(),
                })
            }
            ["write", "cut"] => Err(QueryError::Connection("cut off".into())),
            ["write", "lost"] => {
                self.lost = true;
                Ok(Outcome::default())
            }
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
                // In small chunks, as a driver's cursor gives them, until
                // the sink holds all it takes.
                let mut read = 0;
                for start in (0..count).step_by(50) {
                    if sink.full() {
                        break;
                    }
                    let end = (start + 50).min(count);
                    sink.documents(numbered(start..end))?;
                    read = end;
                }
                Ok(Outcome {
                    returned: read as u64,
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

    /// `write N` touches N documents, `write fail` cannot be counted, and
    /// nothing else can be told.
    fn explain(&mut self, statement: &str, execute: bool) -> Result<String, QueryError> {
        self.plan(statement, execute)
    }

    fn estimate(
        &mut self,
        statement: &str,
        sample: &mut dyn DocumentSink,
    ) -> Result<Option<Count>, QueryError> {
        self.log.push(format!("estimate {statement}"));
        match statement.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["write", "fail"] => Err(QueryError::Database {
                code: "13".into(),
                message: "not allowed to count".into(),
            }),
            ["write", n] => {
                let n: usize = n.parse().unwrap_or(0);
                // More than a preview keeps.
                sink_all(sample, numbered(0..n * 10))?;
                Ok(Some(Count::Exact(n as u64)))
            }
            _ => Ok(None),
        }
    }

    fn begin(&mut self) -> Result<(), QueryError> {
        self.log.push("begin");
        Ok(())
    }

    fn commit(&mut self) -> Result<(), WriteError> {
        self.log.push("commit");
        match std::mem::take(&mut self.lost) {
            true => Err(WriteError::Unknown(QueryError::Connection(
                "no answer".into(),
            ))),
            false => Ok(()),
        }
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
    ) -> Result<Sampling, QueryError> {
        self.log
            .push(format!("sample {n} of {}", collection.display()));
        sink.documents(numbered(0..n))?;
        Ok(Sampling::FirstN)
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

    fn apply(&mut self, _: &CollectionRef, change: &DocChange) -> Result<Outcome, WriteError> {
        self.log.push(change.describe(render));
        let found = |key| u64::from(FakeDocs::holds(key));
        match change {
            DocChange::Update { key, .. } if *key == DocKey(Value::Int32(5)) => {
                return Err(WriteError::Unknown(QueryError::Connection(
                    "no answer".into(),
                )));
            }
            DocChange::Insert { document } if document.key() == Some(DocKey(Value::Int32(6))) => {
                self.lost = true;
            }
            _ => {}
        }
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

impl FakeDocs {
    fn plan(&mut self, statement: &str, execute: bool) -> Result<String, QueryError> {
        self.log.push(format!("explain {statement} {execute}"));
        Ok(format!("{{\"plan\": \"{statement}\"}}"))
    }
}

/// Gives a sink documents in small chunks, until it holds all it takes.
fn sink_all(sink: &mut dyn DocumentSink, documents: Vec<Document>) -> Result<(), QueryError> {
    for chunk in documents.chunks(5) {
        if sink.full() {
            break;
        }
        sink.documents(chunk.to_vec())?;
    }
    Ok(())
}

fn open_with(transactions: Transactions, access: Access) -> (DocumentSession, Log) {
    let preview = match transactions {
        Transactions::Multi { .. } => Preview::Undone,
        Transactions::None => Preview::Estimated,
    };
    open_previewing(transactions, preview, access)
}

fn open_previewing(
    transactions: Transactions,
    preview: Preview,
    access: Access,
) -> (DocumentSession, Log) {
    let log = Log::default();
    let opener_log = log.clone();
    let opener: DocumentOpener = Arc::new(move |_| {
        Ok(Box::new(FakeDocs {
            log: opener_log.clone(),
            stop: Arc::new(AtomicBool::new(false)),
            transactions,
            preview,
            lost: false,
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
    /// What a write run that failed said it left.
    left: Vec<Left>,
    end: Result<DocSummary, QueryError>,
}

fn drain(rx: async_channel::Receiver<DocEvent>) -> Drained {
    block_on(async {
        let mut drained = Drained {
            batches: Vec::new(),
            outcomes: Vec::new(),
            left: Vec::new(),
            end: Err(QueryError::Cancelled),
        };
        while let Ok(event) = rx.recv().await {
            match event {
                DocEvent::Documents(batch) => drained.batches.push(batch.len()),
                DocEvent::Outcome(outcome) => drained.outcomes.push(outcome),
                DocEvent::Left(left) => drained.left.push(left),
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
    assert_eq!(session.home(), ["shop"]);
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
            unknown: 0,
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
fn stops_reading_each_statement_at_the_document_cap() {
    let (session, _) = open(Access::ReadOnly);
    let capped = drain(session.run(
        "read 2600; read 30; read 200".into(),
        RunMode::Read,
        Heavy::Refuse,
        RunOptions {
            first_batch_rows: 100,
            max_documents: Some(120),
            ..RunOptions::default()
        },
    ));
    // 120 of each statement, no more: the third starts its own count.
    assert_eq!(capped.batches, [100, 20, 30, 120]);
    // The adapter stopped reading once the sink was full, and the
    // statements ended well.
    let read: Vec<u64> = capped
        .outcomes
        .iter()
        .map(|outcome| outcome.returned)
        .collect();
    assert_eq!(read, [150, 30, 150]);
    let summary = capped.end.unwrap();
    assert_eq!((summary.documents, summary.statements), (270, 3));
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
    // Inferred where it was sampled, and labelled as the adapter picked.
    let sampled = block_on(session.schema(orders(), 3)).unwrap();
    assert_eq!(sampled.schema.documents, 3);
    assert_eq!(sampled.label(), "First 3 documents");
    assert_eq!(sampled.schema.fields[0].path, "n");
    assert_eq!(
        log.take(),
        [
            "children of []",
            "children of [\"shop\"]",
            "collection shop.orders",
            "sample 5 of shop.orders",
            "count shop.orders None",
            "sample 3 of shop.orders",
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
    assert_eq!(failed.left, [Left::Undone]);
    assert_eq!(log.take(), ["begin", "write 1", "write fail", "abort"]);

    // A single statement that changes documents takes one too, as its
    // preview did; a lone schema change and reads take none.
    run(&session, "write 1", RunMode::Write).end.unwrap();
    assert_eq!(log.take(), ["begin", "write 1", "commit"]);
    let partial = run(&session, "write partial", RunMode::Write);
    assert_eq!(partial.left, [Left::Undone]);
    run(&session, "ddl x", RunMode::Write).end.unwrap();
    run(&session, "read 1; read 2", RunMode::Read).end.unwrap();
    let logged = log.take();
    assert_eq!(logged[..3], ["begin", "write partial", "abort"]);
    assert_eq!(logged[3..], ["ddl x", "read 1", "read 2"]);

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
    // It stopped between statements: the one it stopped in changed nothing.
    assert_eq!(cancelled.left, [Left::NOTHING]);
    assert_eq!(log.take(), ["write 1", "read slow", "cancel"]);
}

#[test]
fn a_write_run_that_fails_says_what_it_left() {
    let (session, _) = open(Access::ReadWrite);
    let inserted = Outcome {
        inserted: 2,
        ..Outcome::default()
    };
    // What the statement that failed changed, as the engine told.
    let partial = run(&session, "write 1; write partial", RunMode::Write);
    assert_eq!(partial.outcomes.len(), 1);
    assert_eq!(
        partial.left,
        [Left::Kept {
            partial: Some(inserted)
        }]
    );
    // Unknown when the engine did not tell, never nothing.
    for failed in ["write 1; write cut", "write fail"] {
        let failed = run(&session, failed, RunMode::Write);
        assert_eq!(failed.left, [Left::Kept { partial: None }]);
    }
    // A script refused before it ran changed nothing.
    let refused = run(&session, "write 1; write !", RunMode::Write);
    assert_eq!(refused.left, [Left::NOTHING]);
    // A run that ends well, or a read run, says nothing of it.
    assert!(run(&session, "write 1", RunMode::Write).left.is_empty());
    let read = run(&session, "read 1; frobnicate", RunMode::Read);
    assert!(read.end.is_err() && read.left.is_empty());

    // In a transaction: undone, or unknown when the commit does not answer.
    let (session, log) = open_with(MULTI, Access::ReadWrite);
    let partial = run(&session, "write 1; write partial", RunMode::Write);
    assert_eq!(partial.left, [Left::Undone]);
    log.take();
    let lost = run(&session, "write 1; write lost", RunMode::Write);
    assert_eq!(lost.left, [Left::Unknown]);
    assert_eq!(lost.end, Err(QueryError::Connection("no answer".into())));
    assert_eq!(
        log.take(),
        ["begin", "write 1", "write lost", "commit", "abort"]
    );
}

#[test]
fn a_run_enters_where_it_starts_in_the_same_job() {
    let (session, log) = open_with(MULTI, Access::ReadWrite);
    let start = ["sales".to_string()];
    drain(session.run_in(
        &start,
        "read 1".into(),
        RunMode::Read,
        Heavy::Refuse,
        RunOptions::default(),
    ))
    .end
    .unwrap();
    assert_eq!(log.take(), ["enter sales", "read 1"]);
    block_on(session.preview_in(
        &start,
        "write 1".into(),
        Heavy::Refuse,
        RunOptions::default(),
    ))
    .unwrap();
    assert_eq!(log.take(), ["enter sales", "begin", "write 1", "abort"]);
    // Without a start, where the last run left off.
    run(&session, "read 1", RunMode::Read).end.unwrap();
    assert_eq!(log.take(), ["read 1"]);
}

#[test]
fn says_which_write_runs_are_all_or_nothing() {
    let statements = |text: &str| analyze(text).unwrap();
    let (multi, _) = open_with(MULTI, Access::ReadWrite);
    assert!(multi.all_or_nothing(&statements("write 1")));
    assert!(multi.all_or_nothing(&statements("read 1; write 1")));
    assert!(!multi.all_or_nothing(&statements("ddl x")));
    let (standalone, _) = open(Access::ReadWrite);
    assert!(!standalone.all_or_nothing(&statements("write 1; write 2")));
    // Checked without reading the script again.
    assert_eq!(
        multi.check_statements(&statements("begin"), RunMode::Write, Heavy::Refuse),
        Err(QueryError::Unsupported(
            "Kuery runs each script's transaction itself, so begin cannot run here".into()
        ))
    );
    assert!(matches!(
        standalone.check_statements(&statements("write 1"), RunMode::Read, Heavy::Refuse),
        Err(QueryError::Rejected(Rejection::Statement(_)))
    ));
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
fn apply_says_which_changes_may_have_applied_when_no_answer_came() {
    // Without transactions, the change whose answer never came may have
    // applied; those before it did.
    let (session, log) = open(Access::ReadWrite);
    let failure = block_on(session.apply(orders(), vec![update(1), update(5), update(2)]));
    assert_eq!(
        failure,
        Err(ApplyFailure {
            applied: 1,
            unknown: 1,
            error: QueryError::Connection("no answer".into()),
        })
    );
    assert_eq!(log.take().len(), 2, "it stops at the change");
    // A refused change is known not to have applied.
    let failure = block_on(session.apply(orders(), vec![update(9)]));
    assert_eq!(failure.map_err(|failure| failure.unknown), Err(0));

    // In a transaction, a change without an answer is undone with it…
    let (session, log) = open_with(MULTI, Access::ReadWrite);
    let failure = block_on(session.apply(orders(), vec![update(1), update(5)]));
    assert_eq!(
        failure.map_err(|failure| (failure.applied, failure.unknown)),
        Err((0, 0))
    );
    assert_eq!(log.take().last().map(String::as_str), Some("abort"));
    // …but a commit that does not say leaves every change unknown.
    let failure = block_on(session.apply(orders(), vec![update(1), insert(6)]));
    assert_eq!(
        failure,
        Err(ApplyFailure {
            applied: 0,
            unknown: 2,
            error: QueryError::Connection("no answer".into()),
        })
    );
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
            unknown: 1,
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

fn preview(session: &DocumentSession, text: &str) -> Result<Vec<Impact>, QueryError> {
    block_on(session.preview(text.into(), Heavy::Refuse, RunOptions::default()))
}

fn estimates(impacts: &[Impact]) -> Vec<Estimate> {
    impacts
        .iter()
        .map(|impact| impact.estimate.clone())
        .collect()
}

fn changed(n: u64) -> Estimate {
    Estimate::Exact(Outcome {
        matched: n,
        modified: n,
        ..Outcome::default()
    })
}

#[test]
fn a_preview_runs_the_script_in_a_transaction_it_always_undoes() {
    let (session, log) = open_with(MULTI, Access::ReadWrite);
    let impacts = preview(&session, "write 2; read 30; write 3").unwrap();
    assert_eq!(
        estimates(&impacts),
        [changed(2), Estimate::Reads, changed(3)]
    );
    assert_eq!(impacts[0].summary, "write 2");
    // What a statement returned, up to what a preview keeps.
    assert_eq!(impacts[1].sample.len(), PREVIEW_SAMPLE);
    assert!(impacts.iter().all(|impact| impact.note.is_none()));
    assert_eq!(
        log.take(),
        ["begin", "write 2", "read 30", "write 3", "abort"]
    );
    // One statement too, which a run would not put in a transaction.
    preview(&session, "write 1").unwrap();
    assert_eq!(log.take(), ["begin", "write 1", "abort"]);
}

#[test]
fn a_statement_that_fails_in_the_preview_is_counted_with_the_ones_after_it() {
    let (session, log) = open_with(MULTI, Access::ReadWrite);
    let impacts = preview(&session, "write 1; write fail; write 2").unwrap();
    assert_eq!(
        estimates(&impacts),
        [
            changed(1),
            Estimate::Unknown,
            Estimate::Estimated(Count::Exact(2))
        ]
    );
    let note = impacts[1].note.as_deref().unwrap();
    assert!(
        note.contains("failed on purpose") && note.contains("not allowed to count"),
        "{note}"
    );
    assert_eq!(impacts[2].sample.len(), PREVIEW_SAMPLE);
    assert_eq!(
        log.take(),
        [
            "begin",
            "write 1",
            "write fail",
            "abort",
            "estimate write fail",
            "estimate write 2"
        ]
    );
    // A schema change never runs in the transaction: it is counted, here
    // not at all.
    let impacts = preview(&session, "ddl x").unwrap();
    assert_eq!(estimates(&impacts), [Estimate::Unknown]);
    assert_eq!(log.take(), ["estimate ddl x"]);
}

#[test]
fn a_preview_never_leaves_its_transaction_open() {
    let (session, log) = open_with(MULTI, Access::ReadWrite);
    let cancel_soon = || {
        let canceller = session.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            canceller.cancel();
        })
    };
    // A statement the engine reports as cancelled.
    let cancelling = cancel_soon();
    let interrupted = preview(&session, "write 1; read interrupted");
    assert_eq!(interrupted, Err(QueryError::Cancelled));
    let logged = log.take();
    cancelling.join().unwrap();
    assert_eq!(logged.first().map(String::as_str), Some("begin"));
    assert_eq!(logged.last().map(String::as_str), Some("abort"));
    // One that ends as if nothing happened once cancelled: the preview
    // still stops, undone.
    let cancelling = cancel_soon();
    let slow = preview(&session, "read slow; write 1");
    cancelling.join().unwrap();
    assert_eq!(slow, Err(QueryError::Cancelled));
    let logged = log.take();
    assert!(!logged.iter().any(|entry| entry == "write 1"), "{logged:?}");
    assert_eq!(logged.last().map(String::as_str), Some("abort"));
    // The session goes on, with no transaction left behind.
    preview(&session, "write 1").unwrap();
    assert_eq!(log.take(), ["begin", "write 1", "abort"]);
}

#[test]
fn without_transactions_a_preview_counts_and_runs_nothing() {
    let (session, log) = open(Access::ReadWrite);
    let impacts = preview(&session, "read 5; write 4; ddl x").unwrap();
    assert_eq!(
        estimates(&impacts),
        [
            Estimate::Reads,
            Estimate::Estimated(Count::Exact(4)),
            Estimate::Unknown
        ]
    );
    assert_eq!(impacts[1].sample.len(), PREVIEW_SAMPLE);
    assert!(impacts[0].sample.is_empty());
    assert_eq!(log.take(), ["estimate write 4", "estimate ddl x"]);
}

#[test]
fn an_engine_that_cannot_preview_says_unknown() {
    let (session, log) = open_previewing(Transactions::None, Preview::None, Access::ReadWrite);
    let impacts = preview(&session, "read 1; write 4").unwrap();
    assert_eq!(estimates(&impacts), [Estimate::Reads, Estimate::Unknown]);
    assert!(log.take().is_empty());
}

#[test]
fn a_preview_is_checked_like_a_write_run() {
    let (reader, log) = open(Access::ReadOnly);
    assert_eq!(preview(&reader, "write 1"), Err(QueryError::WritesDisabled));
    let (session, _) = open_with(MULTI, Access::ReadWrite);
    assert!(matches!(
        preview(&session, "heavy; write 1"),
        Err(QueryError::Rejected(Rejection::Heavy(_)))
    ));
    assert!(matches!(
        preview(&session, "write 1; ddl x"),
        Err(QueryError::Unsupported(_))
    ));
    assert!(log.take().is_empty());
    // What check reads, without running anything.
    let statements = session
        .check("read 1; write 2", RunMode::Write, Heavy::Refuse)
        .unwrap();
    assert_eq!(
        statements
            .iter()
            .map(|statement| statement.info.kind)
            .collect::<Vec<_>>(),
        [StatementKind::Read, StatementKind::Data]
    );
    assert_eq!(
        session.check("write 1", RunMode::Read, Heavy::Refuse),
        Err(QueryError::Rejected(Rejection::Statement("write 1".into())))
    );
}

/// Holds the session's worker in a job that runs nothing until it is
/// released: the job has begun once the first receiver hears from it.
fn hold(session: &DocumentSession) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered, entering) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    drop(session.handle.call(move |_| {
        let _ = entered.send(());
        let _ = released.recv();
        Ok(())
    }));
    entering.recv().unwrap();
    (entering, release)
}

#[test]
fn a_preview_cancelled_before_it_starts_never_reaches_the_adapter() {
    let (session, log) = open_with(MULTI, Access::ReadWrite);
    let (_held, release) = hold(&session);
    let previewed = session.preview_in(
        &["shop".into()],
        "write 1".into(),
        Heavy::Refuse,
        RunOptions::default(),
    );
    session.cancel();
    release.send(()).unwrap();
    assert_eq!(block_on(previewed), Err(QueryError::Cancelled));
    assert!(log.take().is_empty());
    // The session goes on.
    preview(&session, "write 1").unwrap();
    assert_eq!(log.take(), ["begin", "write 1", "abort"]);
}

#[test]
fn a_run_cancelled_before_it_starts_never_reaches_the_adapter() {
    let (session, log) = open_with(MULTI, Access::ReadWrite);
    let (_held, release) = hold(&session);
    let rx = session.run_in(
        &["shop".into()],
        "write 1; write 2".into(),
        RunMode::Write,
        Heavy::Refuse,
        RunOptions::default(),
    );
    session.cancel();
    release.send(()).unwrap();
    let cancelled = drain(rx);
    assert!(matches!(cancelled.end, Err(QueryError::Cancelled)));
    assert_eq!(cancelled.left, [Left::NOTHING]);
    assert!(cancelled.outcomes.is_empty());
    assert!(log.take().is_empty());
    run(&session, "write 1", RunMode::Write).end.unwrap();
}

#[test]
fn explains_reads_measured_and_writes_only_planned() {
    let options = RunOptions::default;
    let explain = |session: &DocumentSession, text: &str, execute: bool, heavy: Heavy| {
        block_on(session.explain_in(&["shop".into()], text.into(), execute, heavy, options()))
    };
    let (reader, log) = open(Access::ReadOnly);
    // A read is measured by running it, where it starts.
    assert_eq!(
        explain(&reader, "read 1", true, Heavy::Refuse).unwrap(),
        "{\"plan\": \"read 1\"}"
    );
    assert_eq!(log.take(), ["enter shop", "explain read 1 true"]);
    // A write is never planned on a read-only session, and never run.
    assert_eq!(
        explain(&reader, "write 1", false, Heavy::Refuse),
        Err(QueryError::WritesDisabled)
    );
    assert!(matches!(
        explain(&reader, "write 1", true, Heavy::Refuse),
        Err(QueryError::Explain(message)) if message.contains("write 1 writes")
    ));
    // Heavy work runs only where allowed, and only matters when measured.
    assert_eq!(
        explain(&reader, "heavy", true, Heavy::Refuse),
        Err(QueryError::Rejected(Rejection::Heavy("heavy".into())))
    );
    explain(&reader, "heavy", false, Heavy::Refuse).unwrap();
    explain(&reader, "heavy", true, Heavy::Allow).unwrap();
    // One explainable statement at a time.
    assert!(matches!(
        explain(&reader, "read 1; read 2", false, Heavy::Refuse),
        Err(QueryError::Explain(_))
    ));
    assert!(matches!(
        explain(&reader, "show things", false, Heavy::Refuse),
        Err(QueryError::Explain(message)) if message == "show things has no plan to show"
    ));
    assert_eq!(
        log.take(),
        [
            "enter shop",
            "explain heavy false",
            "enter shop",
            "explain heavy true"
        ]
    );

    // An engine that only plans is never asked to measure.
    let mut planner = capabilities(Transactions::None, Preview::None);
    planner.explain = Some(ExplainKind::Estimated);
    assert_eq!(
        check_explain(
            &FAKE,
            &planner,
            Access::ReadOnly,
            "read 1",
            false,
            Heavy::Refuse
        ),
        Ok(false)
    );
    assert!(matches!(
        check_explain(&FAKE, &planner, Access::ReadOnly, "read 1", true, Heavy::Refuse),
        Err(QueryError::Unsupported(message)) if message.contains("explain without measuring")
    ));

    // A session that may write plans a write, without running it.
    let (writer, log) = open(Access::ReadWrite);
    explain(&writer, "write 1", false, Heavy::Refuse).unwrap();
    assert_eq!(log.take(), ["enter shop", "explain write 1 false"]);
    assert!(explain(&writer, "write 1", true, Heavy::Refuse).is_err());
    assert!(
        log.take().is_empty(),
        "a measured write never reached the driver"
    );
}

#[test]
fn an_engine_without_activity_says_so() {
    let (session, _) = open(Access::ReadOnly);
    assert!(matches!(
        block_on(session.activity()),
        Err(QueryError::Unsupported(message)) if message.contains("activity")
    ));
    assert!(matches!(
        block_on(session.signal("7".into(), Signal::Cancel)),
        Err(QueryError::Unsupported(message)) if message.contains("operations to stop")
    ));
}
