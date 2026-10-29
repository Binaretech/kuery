//! Changes to a collection's documents, staged in its tab until they are
//! applied: an edit is the typed diff of the document as it was loaded and
//! as it was edited (see [`diff`]), an insert a new document, a delete the
//! loaded document as a whole. Staged changes are kept by the document's
//! key, not by its row, so the grid can page and reload while they wait,
//! and marks them on whichever rows show those documents.

use kuery_core::document::capabilities::{Capabilities, Enforcement};
use kuery_core::document::catalog::ContainerKind;
use kuery_core::document::change::DocChange;
use kuery_core::document::diff::diff;
use kuery_core::document::flatten;
use kuery_core::document::language::{Language, Style};
use kuery_core::document::value::{DocKey, Document, FieldPath, PathPart, Value};

/// One staged change, with what it was made from.
#[derive(Debug, Clone, PartialEq)]
pub enum Staged {
    Edit {
        /// The document as it was loaded.
        before: Document,
        /// As it was edited.
        after: Document,
        change: Box<DocChange>,
    },
    Insert {
        document: Document,
    },
    Delete {
        before: Document,
    },
}

impl Staged {
    /// What it asks the database to do. A delete expects the whole
    /// document as it was loaded, so one someone changed since, even by
    /// adding a field, is not deleted.
    pub fn change(&self) -> DocChange {
        match self {
            Staged::Edit { change, .. } => (**change).clone(),
            Staged::Insert { document } => DocChange::Insert {
                document: document.clone(),
            },
            Staged::Delete { before } => DocChange::Delete {
                key: before.key().unwrap_or(DocKey(Value::Null)),
                version: None,
                expect: vec![(FieldPath(Vec::new()), Some(Value::Object(before.clone())))],
            },
        }
    }

    /// The key of the loaded document it changes; `None` for an insert.
    fn key(&self) -> Option<DocKey> {
        match self {
            Staged::Edit { before, .. } | Staged::Delete { before } => before.key(),
            Staged::Insert { .. } => None,
        }
    }
}

/// How a staged change shows on the grid's row of its document.
#[derive(Debug, Clone, PartialEq)]
pub enum Mark {
    /// A field that holds something new, as the cell shows it.
    Edited {
        row: usize,
        column: usize,
        text: Option<String>,
    },
    /// A field the edit removes.
    Removed {
        row: usize,
        column: usize,
    },
    Deleted(usize),
}

/// The changes staged in a collection tab, in the order they apply.
#[derive(Debug, Default)]
pub struct Staging {
    changes: Vec<Staged>,
    /// Counts every change to what is staged, so what was reviewed can be
    /// told from what is staged now without comparing values (NaN is not
    /// equal to itself).
    generation: u64,
}

/// Why a document cannot be edited again: it changed since its staged edit
/// was made from it.
pub const STALE: &str = "This document changed since you edited it: unstage that change, then \
                         edit the document again";

impl Staging {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.changes.len()
    }

    pub fn staged(&self) -> &[Staged] {
        &self.changes
    }

    /// What each staged change asks the database to do, in order.
    pub fn changes(&self) -> Vec<DocChange> {
        self.changes.iter().map(Staged::change).collect()
    }

    /// Which version of the staged changes this is: it moves on with every
    /// change staged, replaced or unstaged.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn moved(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn clear(&mut self) {
        self.changes.clear();
        self.moved();
    }

    /// Unstages one change.
    pub fn remove(&mut self, index: usize) {
        if index < self.changes.len() {
            self.changes.remove(index);
            self.moved();
        }
    }

    /// Unstages the first `n` changes, which were applied.
    pub fn applied(&mut self, n: usize) {
        self.changes.drain(..n.min(self.changes.len()));
        self.moved();
    }

    /// The staged change of a loaded document, if there is one.
    pub fn of(&self, loaded: &Document) -> Option<&Staged> {
        let key = loaded.key()?;
        self.changes
            .iter()
            .find(|staged| staged.key().as_ref() == Some(&key))
    }

    /// The document as an edit starts from: as already edited, if it was.
    pub fn current(&self, loaded: &Document) -> Document {
        match self.of(loaded) {
            Some(Staged::Edit { after, .. }) => after.clone(),
            _ => loaded.clone(),
        }
    }

    /// Whether the loaded document is no longer the one its staged edit
    /// was made from: someone changed it, and a reload shows that. Editing
    /// it again would show the edit over what it held before.
    pub fn stale(&self, loaded: &Document) -> bool {
        match self.of(loaded) {
            Some(Staged::Edit { before, .. }) => !matches!(diff(before, loaded), Ok(None)),
            _ => false,
        }
    }

    /// Stages the edit of a loaded document, read from the literal it was
    /// edited in. It takes the place of the document's earlier staged
    /// change; an edit back to the loaded document unstages it. A document
    /// edited before is diffed against what it held then, so the change
    /// still expects what was first loaded, and one that changed since is
    /// refused ([`STALE`]).
    pub fn edit(
        &mut self,
        loaded: &Document,
        text: &str,
        language: &Language,
    ) -> Result<(), String> {
        if self.stale(loaded) {
            return Err(STALE.into());
        }
        let before = match self.of(loaded) {
            Some(Staged::Edit { before, .. }) => before.clone(),
            _ => loaded.clone(),
        };
        let after = parse(text, language)?;
        let earlier = self.position(loaded);
        match diff(&before, &after).map_err(|error| error.to_string())? {
            Some(change) => {
                (language.check_change)(&change)?;
                let staged = Staged::Edit {
                    before,
                    after,
                    change: Box::new(change),
                };
                match earlier {
                    Some(index) => self.changes[index] = staged,
                    None => self.changes.push(staged),
                }
                self.moved();
                Ok(())
            }
            None => match earlier {
                Some(index) => {
                    self.changes.remove(index);
                    self.moved();
                    Ok(())
                }
                None => Err("Nothing changed in this document".into()),
            },
        }
    }

    /// Stages a new document, read from its literal.
    pub fn insert(&mut self, text: &str, language: &Language) -> Result<(), String> {
        let document = parse(text, language)?;
        (language.check_change)(&DocChange::Insert {
            document: document.clone(),
        })?;
        self.changes.push(Staged::Insert { document });
        self.moved();
        Ok(())
    }

    /// Stages the delete of a loaded document, in place of its edit if it
    /// had one.
    pub fn delete(&mut self, before: &Document) {
        if before.key().is_none() {
            return;
        }
        let staged = Staged::Delete {
            before: before.clone(),
        };
        match self.position(before) {
            Some(index) => self.changes[index] = staged,
            None => self.changes.push(staged),
        }
        self.moved();
    }

    fn position(&self, loaded: &Document) -> Option<usize> {
        let key = loaded.key()?;
        self.changes
            .iter()
            .position(|staged| staged.key().as_ref() == Some(&key))
    }

    /// How the staged changes show on the loaded rows, whose columns are
    /// `columns`: the fields an edit sets or removes, a delete's whole row.
    pub fn marks(
        &self,
        documents: &[Document],
        columns: &[String],
        render: fn(&Value, Style) -> String,
    ) -> Vec<Mark> {
        let mut marks = Vec::new();
        for (row, document) in documents.iter().enumerate() {
            match self.of(document) {
                Some(Staged::Delete { .. }) => marks.push(Mark::Deleted(row)),
                Some(Staged::Edit { after, change, .. }) => {
                    let DocChange::Update { set, unset, .. } = &**change else {
                        continue;
                    };
                    let cells = flatten::row(after, columns, render);
                    for (column, name) in columns.iter().enumerate() {
                        let removed = unset
                            .iter()
                            .any(|path| path.0.len() == 1 && field(path) == Some(name));
                        let touched = set
                            .iter()
                            .map(|(path, _)| path)
                            .chain(unset)
                            .any(|path| field(path) == Some(name));
                        if removed {
                            marks.push(Mark::Removed { row, column });
                        } else if touched {
                            marks.push(Mark::Edited {
                                row,
                                column,
                                text: cells[column].clone(),
                            });
                        }
                    }
                }
                Some(Staged::Insert { .. }) | None => {}
            }
        }
        marks
    }
}

/// The top-level field a path starts at: the grid's column.
fn field(path: &FieldPath) -> Option<&String> {
    match path.0.first() {
        Some(PathPart::Field(name)) => Some(name),
        _ => None,
    }
}

fn parse(text: &str, language: &Language) -> Result<Document, String> {
    (language.parse_document)(text).map_err(|error| error.located(text))
}

/// A new document's starting point: the fields of the loaded documents,
/// in the grid's order, each holding an empty value of the type most of
/// them hold (`""`, `0`, `NumberLong("0")`, `[]`…); null where that type
/// has no empty value. The key is left for the database to choose.
pub fn template(documents: &[Document]) -> Document {
    let fields = flatten::columns(documents)
        .into_iter()
        .filter(|name| name != "_id")
        .map(|name| {
            let empty = most_common(documents, &name).map_or(Value::Null, empty);
            (name, empty)
        })
        .collect();
    Document(fields)
}

/// The value most documents hold in `field`, nulls aside.
fn most_common<'a>(documents: &'a [Document], field: &str) -> Option<&'a Value> {
    let mut counts: Vec<(&'static str, usize, &Value)> = Vec::new();
    for value in documents.iter().filter_map(|document| document.get(field)) {
        if *value == Value::Null {
            continue;
        }
        match counts
            .iter_mut()
            .find(|(kind, _, _)| *kind == value.type_name())
        {
            Some((_, count, _)) => *count += 1,
            None => counts.push((value.type_name(), 1, value)),
        }
    }
    counts
        .iter()
        .rev()
        .max_by_key(|(_, count, _)| *count)
        .map(|(_, _, value)| *value)
}

/// An empty value of the same type.
fn empty(value: &Value) -> Value {
    match value {
        Value::Bool(_) => Value::Bool(false),
        Value::Int32(_) => Value::Int32(0),
        Value::Int64(_) => Value::Int64(0),
        Value::Double(_) => Value::Double(0.0),
        Value::Decimal(_) => Value::Decimal("0".into()),
        Value::String(_) => Value::String(String::new()),
        Value::Array(_) => Value::Array(Vec::new()),
        Value::Object(_) => Value::Object(Document::default()),
        Value::Null | Value::Binary { .. } | Value::DateTime(_) | Value::Ext(_) => Value::Null,
    }
}

/// A copy of a document to insert: everything but its key, which the
/// database chooses anew.
pub fn duplicate(document: &Document) -> Document {
    Document(
        document
            .0
            .iter()
            .filter(|(name, _)| name != "_id")
            .cloned()
            .collect(),
    )
}

/// Why documents of this collection cannot be edited here, if they
/// cannot: a view holds none of its own, and a user the server keeps from
/// writing would only be refused.
pub fn read_only_reason(kind: ContainerKind, capabilities: &Capabilities) -> Option<&'static str> {
    if kind == ContainerKind::View {
        return Some("Read-only view: its documents belong to the collections it reads");
    }
    if capabilities.read_only == Enforcement::Server {
        return Some("Read-only: at connect time, this user could only read");
    }
    None
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kuery_core::document::capabilities::{Preview, Sampling, Transactions};
    use kuery_core::document::value::FieldPath;

    use super::*;

    const MONGO: &Language = &kuery_mongo::LANGUAGE;

    fn doc(fields: &[(&str, Value)]) -> Document {
        Document(
            fields
                .iter()
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect(),
        )
    }

    fn order() -> Document {
        doc(&[
            ("_id", Value::Int32(7)),
            ("qty", Value::Int64(2)),
            ("total", Value::Decimal("9.99".into())),
            ("note", Value::String("fragile".into())),
        ])
    }

    fn render(value: &Value, style: Style) -> String {
        (MONGO.render)(value, style)
    }

    #[test]
    fn an_edit_is_staged_as_its_typed_diff() {
        let mut staging = Staging::default();
        staging
            .edit(
                &order(),
                r#"{_id: 7, qty: NumberLong("3"), total: NumberDecimal("9.99")}"#,
                MONGO,
            )
            .unwrap();
        let changes = staging.changes();
        let [
            DocChange::Update {
                set, unset, expect, ..
            },
        ] = changes.as_slice()
        else {
            panic!("{changes:?}");
        };
        // NumberLong stays Int64; the removed field is unset, not null.
        assert_eq!(set, &[(FieldPath::field("qty"), Value::Int64(3))]);
        assert_eq!(unset, &[FieldPath::field("note")]);
        assert_eq!(
            expect,
            &[
                (FieldPath::field("qty"), Some(Value::Int64(2))),
                (
                    FieldPath::field("note"),
                    Some(Value::String("fragile".into()))
                ),
            ]
        );
    }

    #[test]
    fn refuses_what_cannot_be_an_edit() {
        let mut staging = Staging::default();
        let changed_key = staging.edit(&order(), "{_id: 8}", MONGO);
        assert_eq!(
            changed_key,
            Err("The _id cannot change: duplicate the document instead".into())
        );
        let unreadable = staging
            .edit(&order(), "{_id: 7, qty: }", MONGO)
            .unwrap_err();
        assert!(unreadable.starts_with("line 1, column"), "{unreadable}");
        let same = render(&Value::Object(order()), Style::Pretty);
        assert_eq!(
            staging.edit(&order(), &same, MONGO),
            Err("Nothing changed in this document".into())
        );
        assert!(staging.is_empty());
    }

    #[test]
    fn one_change_per_document_and_back_to_none() {
        let mut staging = Staging::default();
        let loaded = order();
        staging
            .edit(&loaded, r#"{_id: 7, qty: NumberLong("3")}"#, MONGO)
            .unwrap();
        // Editing again starts from the edit and replaces it.
        assert_eq!(staging.current(&loaded).get("note"), None);
        staging
            .edit(
                &loaded,
                r#"{_id: 7, qty: NumberLong("4"), total: NumberDecimal("9.99"), note: "fragile"}"#,
                MONGO,
            )
            .unwrap();
        assert_eq!(staging.len(), 1);
        // Back to what was loaded: nothing is staged.
        staging
            .edit(
                &loaded,
                &render(&Value::Object(loaded.clone()), Style::Pretty),
                MONGO,
            )
            .unwrap();
        assert!(staging.is_empty());
        // A delete takes the place of an edit.
        staging
            .edit(&loaded, r#"{_id: 7, qty: NumberLong("3")}"#, MONGO)
            .unwrap();
        staging.insert("{a: 1}", MONGO).unwrap();
        staging.delete(&loaded);
        assert_eq!(staging.len(), 2);
        // It expects the whole document as loaded.
        assert_eq!(
            staging.changes()[0],
            DocChange::Delete {
                key: DocKey(Value::Int32(7)),
                version: None,
                expect: vec![(FieldPath(Vec::new()), Some(Value::Object(loaded.clone())))],
            }
        );
        staging.applied(1);
        assert!(matches!(staging.staged(), [Staged::Insert { .. }]));
        staging.remove(0);
        assert!(staging.is_empty());
    }

    #[test]
    fn an_edit_again_after_a_reload_keeps_what_was_first_loaded() {
        let mut staging = Staging::default();
        let first = doc(&[
            ("_id", Value::Int32(1)),
            ("a", Value::Int32(1)),
            ("b", Value::Int32(1)),
        ]);
        staging.edit(&first, "{_id: 1, a: 2, b: 1}", MONGO).unwrap();
        // Reloaded as it was: editing again still expects what was first
        // loaded, not what the first edit made of it.
        let same = first.clone();
        assert!(!staging.stale(&same));
        staging.edit(&same, "{_id: 1, a: 3, b: 1}", MONGO).unwrap();
        let changes = staging.changes();
        let [DocChange::Update { set, expect, .. }] = changes.as_slice() else {
            panic!("{changes:?}");
        };
        assert_eq!(set, &[(FieldPath::field("a"), Value::Int32(3))]);
        assert_eq!(expect, &[(FieldPath::field("a"), Some(Value::Int32(1)))]);

        // Someone else set b meanwhile, and the page reloaded. The editor
        // would show the staged b over theirs: editing it again is refused,
        // and the staged edit still expects b as first loaded.
        let theirs = doc(&[
            ("_id", Value::Int32(1)),
            ("a", Value::Int32(1)),
            ("b", Value::Int32(5)),
        ]);
        assert!(staging.stale(&theirs));
        let shown = render(&Value::Object(staging.current(&theirs)), Style::Compact);
        assert_eq!(
            staging.edit(&theirs, &shown.replace("a: 3", "a: 4"), MONGO),
            Err(STALE.into())
        );
        let changes = staging.changes();
        let [DocChange::Update { set, expect, .. }] = changes.as_slice() else {
            panic!("{changes:?}");
        };
        assert_eq!(set, &[(FieldPath::field("a"), Value::Int32(3))]);
        assert_eq!(expect, &[(FieldPath::field("a"), Some(Value::Int32(1)))]);
        // Its marks show what the edit changes, not where theirs differs.
        let columns: Vec<String> = ["_id", "a", "b"].map(String::from).to_vec();
        assert_eq!(
            staging.marks(std::slice::from_ref(&theirs), &columns, render),
            [Mark::Edited {
                row: 0,
                column: 1,
                text: Some("3".into())
            }]
        );
        // Deleting it instead deletes it as it is now.
        staging.delete(&theirs);
        assert!(!staging.stale(&theirs));
    }

    #[test]
    fn staged_changes_holding_nan_still_apply() {
        let mut staging = Staging::default();
        let start = staging.generation();
        // A duplicate of a document holding NaN, which is not equal to
        // itself: what was reviewed is told by its generation instead.
        staging.insert("{ratio: NaN, n: 1}", MONGO).unwrap();
        assert_ne!(staging.changes(), staging.changes());
        let reviewed = staging.generation();
        assert_ne!(reviewed, start);
        assert_eq!(staging.generation(), reviewed);
        // Every change to what is staged moves it on.
        let loaded = doc(&[
            ("_id", Value::Int32(7)),
            ("ratio", Value::Double(f64::NAN)),
            ("n", Value::Int32(1)),
        ]);
        let mut seen = vec![reviewed];
        staging
            .edit(&loaded, "{_id: 7, ratio: NaN, n: 2}", MONGO)
            .unwrap();
        seen.push(staging.generation());
        staging.delete(&loaded);
        seen.push(staging.generation());
        staging.remove(1);
        seen.push(staging.generation());
        staging.applied(1);
        seen.push(staging.generation());
        staging.clear();
        seen.push(staging.generation());
        let mut distinct = seen.clone();
        distinct.dedup();
        assert_eq!(distinct, seen);
        // A NaN that stayed is not marked as edited.
        staging
            .edit(&loaded, "{_id: 7, ratio: NaN, n: 2}", MONGO)
            .unwrap();
        let columns: Vec<String> = ["_id", "ratio", "n"].map(String::from).to_vec();
        assert_eq!(
            staging.marks(&[loaded], &columns, render),
            [Mark::Edited {
                row: 0,
                column: 2,
                text: Some("2".into())
            }]
        );
    }

    #[test]
    fn refuses_to_stage_what_the_engine_cannot_apply() {
        let mut staging = Staging::default();
        for edited in [
            r#"{_id: 7, qty: NumberLong("2"), total: NumberDecimal("9.99"), note: "fragile", "a.b": 1}"#,
            r#"{_id: 7, qty: NumberLong("2"), total: NumberDecimal("9.99"), note: "fragile", $x: 1}"#,
            r#"{_id: 7, qty: NumberLong("2"), total: NumberDecimal("9.99"), note: "fragile", "": 1}"#,
        ] {
            let refused = staging.edit(&order(), edited, MONGO).unwrap_err();
            assert!(refused.contains("cannot edit the field"), "{refused}");
        }
        // Inside a document too.
        let nested = doc(&[
            ("_id", Value::Int32(8)),
            ("o", Value::Object(Document::default())),
        ]);
        let refused = staging
            .edit(&nested, r#"{_id: 8, o: {"a.b": 1}}"#, MONGO)
            .unwrap_err();
        assert!(refused.contains("o.a.b"), "{refused}");
        assert!(staging.is_empty());
        // A new document may hold such a field: it is inserted whole.
        staging.insert(r#"{"a.b": 1}"#, MONGO).unwrap();
        assert_eq!(staging.len(), 1);
    }

    #[test]
    fn marks_the_rows_of_the_documents_it_changes() {
        let mut staging = Staging::default();
        let other = doc(&[("_id", Value::Int32(8)), ("qty", Value::Int64(1))]);
        let documents = vec![other.clone(), order()];
        let columns: Vec<String> = ["_id", "qty", "total", "note"].map(String::from).to_vec();
        staging
            .edit(
                &order(),
                r#"{_id: 7, qty: NumberLong("3"), total: NumberDecimal("9.99")}"#,
                MONGO,
            )
            .unwrap();
        staging.delete(&other);
        assert_eq!(
            staging.marks(&documents, &columns, render),
            [
                Mark::Deleted(0),
                Mark::Edited {
                    row: 1,
                    column: 1,
                    text: Some("NumberLong(\"3\")".into())
                },
                Mark::Removed { row: 1, column: 3 },
            ]
        );
    }

    #[test]
    fn a_new_document_starts_from_the_fields_loaded() {
        let documents = vec![
            order(),
            doc(&[
                ("_id", Value::Int32(8)),
                ("qty", Value::Int64(5)),
                ("tags", Value::Array(vec![Value::String("a".into())])),
                ("note", Value::Null),
                ("when", Value::DateTime(0)),
            ]),
        ];
        assert_eq!(
            template(&documents),
            doc(&[
                ("qty", Value::Int64(0)),
                ("note", Value::String(String::new())),
                ("total", Value::Decimal("0".into())),
                ("tags", Value::Array(Vec::new())),
                ("when", Value::Null),
            ])
        );
        assert_eq!(template(&[]), Document::default());
        assert_eq!(duplicate(&order()).get("_id"), None);
        assert_eq!(duplicate(&order()).0.len(), 3);
    }

    #[test]
    fn views_and_users_that_only_read_offer_no_edits() {
        let mut capabilities = Capabilities {
            levels: &["Database", "Collection"],
            transactions: Transactions::Multi {
                lifetime: Duration::from_secs(60),
            },
            read_only: Enforcement::ClientOnly("no"),
            preview: Preview::Undone,
            explain: None,
            activity: false,
            kill: false,
            sampling: Sampling::Random,
            version_token: false,
        };
        assert_eq!(
            read_only_reason(ContainerKind::Collection, &capabilities),
            None
        );
        assert!(read_only_reason(ContainerKind::View, &capabilities).is_some());
        capabilities.read_only = Enforcement::Server;
        assert!(read_only_reason(ContainerKind::Collection, &capabilities).is_some());
    }
}
