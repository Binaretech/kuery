//! What every document driver must do, checked against its real database:
//! the same checks for every [`DocumentAdapter`], through the
//! [`DocumentSession`] the app uses. A driver's own tests call [`run`] with
//! a way to open it and a collection of a few documents it may read;
//! passing them is what "works like any document database" means.
//!
//! The checks that write do it only where the fixture says they may
//! ([`Writes`]), and leave nothing behind there.
//!
//! [`DocumentAdapter`]: crate::document::adapter::DocumentAdapter

use std::time::{Duration, Instant};

use futures::executor::block_on;

use crate::document::adapter::WriteError;
use crate::document::capabilities::{Enforcement, Preview, Transactions};
use crate::document::change::{BrowseSpec, Count, DocChange};
use crate::document::diff::diff;
use crate::document::session::{
    ApplyFailure, DocEvent, DocSummary, DocumentOpener, DocumentSession, Estimate, Heavy, Left,
    RunMode,
};
use crate::document::value::{CollectionRef, DocKey, Document, FieldPath, Value};
use crate::session::{Access, QueryError, RunOptions};

/// How to reach the database under test.
pub struct Fixture {
    pub opener: DocumentOpener,
    /// A collection of at least four documents with an `_id` each.
    pub collection: CollectionRef,
    /// A collection holding, somewhere in its documents, an Int64 beyond
    /// 2^53, a decimal and a date.
    pub types: CollectionRef,
    /// The same database as a user the server keeps from writing, where
    /// the engine has users.
    pub reader: Option<DocumentOpener>,
    /// A read of the engine's language that runs for 20 seconds or more,
    /// for the cancel check. It may be heavy.
    pub slow: String,
    /// Where the checks that write may write; `None` skips them.
    pub writes: Option<Writes>,
}

/// A place the write checks may change, as statements of the engine's
/// language.
pub struct Writes {
    /// The collection they write to, in a database of its own.
    pub collection: CollectionRef,
    /// Removes what they wrote, with the database itself: run before and
    /// after each check.
    pub clear: String,
    /// Adds one document to the collection.
    pub insert: String,
    /// Two statements: the first adds a document to the collection, the
    /// second fails.
    pub half_failing: String,
}

/// Runs every check.
pub fn run(fixture: &Fixture) {
    browses_and_streams(fixture);
    refuses_writes_twice_when_reading(fixture);
    reads_its_catalog(fixture);
    samples(fixture);
    keeps_its_types(fixture);
    says_who_keeps_it_read_only(fixture);
    cancels_the_slow_statement(fixture);
    applies_all_or_nothing(fixture);
    previews_change_nothing(fixture);
    applies_changes_checking_each_one(fixture);
}

fn open(fixture: &Fixture, access: Access) -> DocumentSession {
    block_on(DocumentSession::open(fixture.opener.clone(), access)).expect("opens the database")
}

fn drain(rx: async_channel::Receiver<DocEvent>) -> (Vec<Document>, Result<DocSummary, QueryError>) {
    block_on(async {
        let mut documents = Vec::new();
        while let Ok(event) = rx.recv().await {
            match event {
                DocEvent::Documents(batch) => documents.extend(batch),
                DocEvent::Outcome(_) | DocEvent::Left(_) => {}
                DocEvent::Finished(summary) => return (documents, Ok(summary)),
                DocEvent::Failed(error) => return (documents, Err(error)),
            }
        }
        (documents, Err(QueryError::Cancelled))
    })
}

fn page(
    session: &DocumentSession,
    collection: &CollectionRef,
    spec: BrowseSpec,
) -> (Vec<Document>, DocSummary) {
    let (documents, summary) = drain(session.browse(
        collection.clone(),
        spec,
        Heavy::Refuse,
        RunOptions {
            first_batch_rows: 1,
            ..RunOptions::default()
        },
    ));
    (documents, summary.expect("browses the collection"))
}

pub fn browses_and_streams(fixture: &Fixture) {
    let reader = open(fixture, Access::ReadOnly);
    let (first, summary) = page(
        &reader,
        &fixture.collection,
        BrowseSpec {
            limit: 2,
            ..BrowseSpec::default()
        },
    );
    assert_eq!(first.len(), 2, "a page holds what its limit says");
    assert_eq!(summary.documents, 2);
    assert!(first.iter().all(|document| document.key().is_some()));
    let next = summary
        .next
        .expect("a page that came back full has a next one");
    let (second, _) = page(
        &reader,
        &fixture.collection,
        BrowseSpec {
            limit: 2,
            after: Some(next),
            ..BrowseSpec::default()
        },
    );
    assert!(!second.is_empty(), "the next page has documents");
    for document in &second {
        assert!(
            !first.iter().any(|seen| seen.key() == document.key()),
            "a document showed up on two pages: {:?}",
            document.key()
        );
    }
}

pub fn refuses_writes_twice_when_reading(fixture: &Fixture) {
    let reader = open(fixture, Access::ReadOnly);
    let insert = DocChange::Insert {
        document: Document(vec![(
            "kuery_conformance".into(),
            Value::String("must never be written".into()),
        )]),
    };
    let refused = block_on(reader.apply(fixture.collection.clone(), vec![insert.clone()]));
    assert_eq!(
        refused,
        Err(ApplyFailure {
            applied: 0,
            unknown: 0,
            error: QueryError::WritesDisabled
        })
    );
    let (_, refused) = drain(reader.run(
        String::new(),
        RunMode::Write,
        Heavy::Refuse,
        RunOptions::default(),
    ));
    assert_eq!(refused.err(), Some(QueryError::WritesDisabled));
    // The second layer on its own: the driver opened read-only refuses
    // writes even when nothing analysed them first.
    let mut adapter = (fixture.opener)(Access::ReadOnly).expect("opens the database");
    let applied = adapter.apply(&fixture.collection, &insert);
    assert_eq!(
        applied,
        Err(WriteError::Undone(QueryError::WritesDisabled)),
        "the driver wrote"
    );
    assert!(
        adapter.begin().is_err(),
        "the driver began a transaction on a read-only connection"
    );
}

pub fn reads_its_catalog(fixture: &Fixture) {
    let reader = open(fixture, Access::ReadOnly);
    let (parent, name) = fixture
        .collection
        .path
        .split_last()
        .map(|(name, parent)| (parent.to_vec(), name.clone()))
        .expect("the collection has a path");
    let children = block_on(reader.children(parent)).unwrap();
    let listed = children
        .iter()
        .find(|child| child.name == name)
        .unwrap_or_else(|| panic!("{name} is not listed: {children:?}"));
    assert!(listed.leaf, "a collection has nothing under it");
    block_on(reader.collection(fixture.collection.clone())).unwrap();
    if let [first, ..] = fixture.collection.path.as_slice() {
        let top = block_on(reader.children(Vec::new())).unwrap();
        assert!(top.iter().any(|child| &child.name == first), "{top:?}");
    }
}

pub fn samples(fixture: &Fixture) {
    let reader = open(fixture, Access::ReadOnly);
    let sample = block_on(reader.sample(fixture.collection.clone(), 3)).unwrap();
    assert!(
        (1..=3).contains(&sample.len()),
        "sampled {} documents",
        sample.len()
    );
}

pub fn keeps_its_types(fixture: &Fixture) {
    let reader = open(fixture, Access::ReadOnly);
    let (documents, _) = page(&reader, &fixture.types, BrowseSpec::default());
    let values: Vec<&Value> = documents
        .iter()
        .flat_map(|document| document.0.iter().map(|(_, value)| value))
        .collect();
    let mut found = Vec::new();
    for value in values {
        walk(value, &mut found);
    }
    let beyond_doubles =
        |value: &&Value| matches!(value, Value::Int64(n) if n.unsigned_abs() > 1 << 53);
    assert!(
        found.iter().any(beyond_doubles),
        "no Int64 beyond 2^53 came back as one"
    );
    assert!(
        found.iter().any(|value| matches!(value, Value::Decimal(_))),
        "no decimal came back as one"
    );
    assert!(
        found
            .iter()
            .any(|value| matches!(value, Value::DateTime(_))),
        "no date came back as one"
    );
}

/// Every value in `value`, nested ones too.
fn walk<'a>(value: &'a Value, found: &mut Vec<&'a Value>) {
    found.push(value);
    match value {
        Value::Array(items) => items.iter().for_each(|item| walk(item, found)),
        Value::Object(document) => document.0.iter().for_each(|(_, value)| walk(value, found)),
        _ => {}
    }
}

pub fn says_who_keeps_it_read_only(fixture: &Fixture) {
    let reader = open(fixture, Access::ReadOnly);
    if let Enforcement::ClientOnly(why) = reader.capabilities().read_only {
        assert!(!why.is_empty());
        assert!(
            reader.notice().is_some(),
            "only Kuery refuses writes here, and the session must say so"
        );
    }
    let Some(opener) = &fixture.reader else {
        return;
    };
    let kept = block_on(DocumentSession::open(opener.clone(), Access::ReadOnly))
        .expect("opens the database as the reading user");
    assert_eq!(
        kept.capabilities().read_only,
        Enforcement::Server,
        "the server keeps this user from writing, and the session must know"
    );
}

pub fn cancels_the_slow_statement(fixture: &Fixture) {
    let reader = open(fixture, Access::ReadOnly);
    let started = Instant::now();
    let rx = reader.run(
        fixture.slow.clone(),
        RunMode::Read,
        Heavy::Allow,
        RunOptions::default(),
    );
    let canceller = reader.clone();
    let cancelling = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        canceller.cancel();
    });
    let (_, ended) = drain(rx);
    cancelling.join().expect("cancels");
    assert_eq!(
        ended.err(),
        Some(QueryError::Cancelled),
        "the slow statement ran to the end"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "cancelling took {:?}",
        started.elapsed()
    );
    // The session goes on working, and the cancel reached nothing after it.
    let (documents, _) = page(
        &reader,
        &fixture.collection,
        BrowseSpec {
            limit: 1,
            ..BrowseSpec::default()
        },
    );
    assert_eq!(
        documents.len(),
        1,
        "the session stopped working after a cancel"
    );
}

/// Runs a write script, for the checks that write.
fn write(session: &DocumentSession, text: &str) -> Result<DocSummary, QueryError> {
    drain(session.run(
        text.into(),
        RunMode::Write,
        Heavy::Refuse,
        RunOptions::default(),
    ))
    .1
}

/// Runs a write script that fails, and says what it said it left.
fn write_failing(session: &DocumentSession, text: &str) -> (Result<(), QueryError>, Vec<Left>) {
    let rx = session.run(
        text.into(),
        RunMode::Write,
        Heavy::Refuse,
        RunOptions::default(),
    );
    block_on(async {
        let mut left = Vec::new();
        while let Ok(event) = rx.recv().await {
            match event {
                DocEvent::Left(said) => left.push(said),
                DocEvent::Finished(_) => return (Ok(()), left),
                DocEvent::Failed(error) => return (Err(error), left),
                DocEvent::Documents(_) | DocEvent::Outcome(_) => {}
            }
        }
        (Err(QueryError::Cancelled), left)
    })
}

/// How many documents the write checks' collection holds.
fn held(session: &DocumentSession, writes: &Writes) -> Count {
    block_on(session.count(writes.collection.clone(), None, 0, Heavy::Refuse))
        .expect("counts the written collection")
}

/// A script that fails halfway keeps none of it where the engine has
/// transactions, and what ran before the failure where it has none, and
/// says which.
pub fn applies_all_or_nothing(fixture: &Fixture) {
    let Some(writes) = &fixture.writes else {
        return;
    };
    let session = open(fixture, Access::ReadWrite);
    write(&session, &writes.clear).expect("clears the written collection");
    let (failed, left) = write_failing(&session, &writes.half_failing);
    assert!(failed.is_err(), "the second statement did not fail");
    let kept = held(&session, writes);
    write(&session, &writes.clear).expect("clears the written collection");
    let (expected, said) = match session.capabilities().transactions {
        Transactions::Multi { .. } => (0, matches!(left[..], [Left::Undone])),
        Transactions::None => (1, matches!(left[..], [Left::Kept { .. }])),
    };
    assert_eq!(
        kept,
        Count::Exact(expected),
        "a script that failed halfway kept the wrong part"
    );
    assert!(said, "a script that failed halfway said it left {left:?}");
}

/// A preview says what a write would do and keeps none of it.
pub fn previews_change_nothing(fixture: &Fixture) {
    let Some(writes) = &fixture.writes else {
        return;
    };
    let session = open(fixture, Access::ReadWrite);
    write(&session, &writes.clear).expect("clears the written collection");
    let preview = |text: &str| {
        block_on(session.preview(text.into(), Heavy::Refuse, RunOptions::default()))
            .expect("previews the script")
    };
    let inserted = preview(&writes.insert);
    let half = preview(&writes.half_failing);
    let kept = held(&session, writes);
    write(&session, &writes.clear).expect("clears the written collection");
    assert_eq!(kept, Count::Exact(0), "a preview kept what it previewed");
    assert_eq!(inserted.len(), 1);
    assert_eq!(half.len(), 2);
    let estimate = &inserted[0].estimate;
    match session.capabilities().preview {
        Preview::Undone => assert!(
            matches!(estimate, Estimate::Exact(outcome) if outcome.inserted == 1),
            "{estimate:?}"
        ),
        Preview::Estimated => assert!(
            matches!(estimate, Estimate::Estimated(_) | Estimate::Unknown),
            "{estimate:?}"
        ),
        Preview::None => assert_eq!(estimate, &Estimate::Unknown),
    }
}

/// Edits apply only to a document that still holds what was loaded, keep
/// the types of what they set, remove what was removed (never set it to
/// null), and a batch with a stale change keeps none of it where the
/// engine has transactions, and what came before it where it has none. A
/// delete that expects the whole document refuses one that changed in any
/// way, a field added included.
pub fn applies_changes_checking_each_one(fixture: &Fixture) {
    let Some(writes) = &fixture.writes else {
        return;
    };
    let session = open(fixture, Access::ReadWrite);
    write(&session, &writes.clear).expect("clears the written collection");
    let apply =
        |changes: Vec<DocChange>| block_on(session.apply(writes.collection.clone(), changes));
    let loaded = Document(vec![
        ("_id".into(), Value::String("kuery-edit".into())),
        ("n".into(), Value::Int64(1)),
        ("price".into(), Value::Decimal("9.99".into())),
        ("note".into(), Value::String("gone soon".into())),
    ]);
    let inserted = apply(vec![DocChange::Insert {
        document: loaded.clone(),
    }])
    .expect("inserts a document");
    assert_eq!(inserted.inserted, 1);

    let mut edited = loaded.clone();
    edited.0[1].1 = Value::Int64(2);
    edited.0.retain(|(name, _)| name != "note");
    edited.0.push(("added".into(), Value::Null));
    let change = diff(&loaded, &edited)
        .expect("the key stayed")
        .expect("something changed");
    let updated = apply(vec![change.clone()]).expect("applies the edit");
    assert_eq!(updated.matched, 1);
    let (stored, _) = page(&session, &writes.collection, BrowseSpec::default());
    assert_eq!(stored, [edited.clone()], "the edit was not stored as made");

    // The same edit again expects what the document no longer holds.
    let stale = apply(vec![change.clone()]);
    assert!(
        matches!(
            &stale,
            Err(ApplyFailure {
                applied: 0,
                unknown: 0,
                error: QueryError::RowMismatch { .. }
            })
        ),
        "a stale edit applied: {stale:?}"
    );

    let other = Document(vec![("_id".into(), Value::String("kuery-edit-2".into()))]);
    let batch = apply(vec![
        DocChange::Insert {
            document: other.clone(),
        },
        change,
    ]);
    let (stored, _) = page(&session, &writes.collection, BrowseSpec::default());
    let (applied, kept) = match session.capabilities().transactions {
        Transactions::Multi { .. } => (0, vec![edited.clone()]),
        Transactions::None => (1, vec![edited.clone(), other]),
    };
    assert!(
        matches!(
            &batch,
            Err(ApplyFailure { applied: said, unknown: 0, error: QueryError::RowMismatch { .. } }) if *said == applied
        ),
        "a batch with a stale change said {batch:?}"
    );
    assert_eq!(
        stored, kept,
        "a batch with a stale change kept the wrong part"
    );

    let key = DocKey(Value::String("kuery-edit".into()));
    let delete = |n| DocChange::Delete {
        key: key.clone(),
        version: None,
        expect: vec![(FieldPath::field("n"), Some(Value::Int64(n)))],
    };
    let refused = apply(vec![delete(1)]);
    assert!(
        matches!(
            &refused,
            Err(ApplyFailure {
                error: QueryError::RowMismatch { .. },
                ..
            })
        ),
        "a stale delete applied: {refused:?}"
    );
    let deleted = apply(vec![delete(2)]).expect("deletes the document");
    assert_eq!(deleted.matched, 1);

    // A delete that expects the whole document as loaded refuses one that
    // gained a field since.
    let loaded = Document(vec![
        ("_id".into(), Value::String("kuery-delete".into())),
        ("n".into(), Value::Int64(1)),
    ]);
    apply(vec![DocChange::Insert {
        document: loaded.clone(),
    }])
    .expect("inserts a document");
    let whole = |document: &Document| DocChange::Delete {
        key: DocKey(Value::String("kuery-delete".into())),
        version: None,
        expect: vec![(FieldPath(Vec::new()), Some(Value::Object(document.clone())))],
    };
    let mut theirs = loaded.clone();
    theirs.0.push(("added".into(), Value::Bool(true)));
    let added = diff(&loaded, &theirs)
        .expect("the key stayed")
        .expect("something changed");
    apply(vec![added]).expect("adds a field");
    let refused = apply(vec![whole(&loaded)]);
    assert!(
        matches!(
            &refused,
            Err(ApplyFailure {
                error: QueryError::RowMismatch { .. },
                ..
            })
        ),
        "a delete of a document that gained a field applied: {refused:?}"
    );
    let deleted = apply(vec![whole(&theirs)]).expect("deletes the document as it is");
    assert_eq!(deleted.matched, 1);
    write(&session, &writes.clear).expect("clears the written collection");
}
