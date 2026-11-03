//! What Kuery asks of a collection, in terms every engine can serve: the
//! changes it makes to documents (each driver turns them into its own
//! commands) and the pages it browses.

use crate::browse::{Direction, Filter};
use crate::document::language::Style;
use crate::document::value::{DocKey, Document, FieldPath, Value};

/// A change to one document, made from an edit in the grid. Each carries
/// what the document held when it was loaded, so the driver only applies
/// it to a document that still holds that (optimistic checking).
#[derive(Debug, Clone, PartialEq)]
pub enum DocChange {
    /// Some fields set and some removed. `expect` holds each touched
    /// field's earlier value (`None`: it was absent).
    Update {
        key: DocKey,
        /// The engine's version token, where documents have one.
        version: Option<Value>,
        set: Vec<(FieldPath, Value)>,
        unset: Vec<FieldPath>,
        expect: Vec<(FieldPath, Option<Value>)>,
    },
    /// The whole document replaced, keeping its key.
    Replace {
        key: DocKey,
        version: Option<Value>,
        document: Document,
    },
    Insert {
        document: Document,
    },
    /// `expect` as an update's; an empty path expects the whole document,
    /// so one that gained a field does not match either.
    Delete {
        key: DocKey,
        version: Option<Value>,
        expect: Vec<(FieldPath, Option<Value>)>,
    },
}

/// What a change must do to count as applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expected {
    /// Find exactly this many documents (still as they were loaded).
    Matched(u64),
    /// Insert exactly this many.
    Inserted(u64),
}

impl DocChange {
    /// One document each: found, or inserted.
    pub fn expected(&self) -> Expected {
        match self {
            DocChange::Insert { .. } => Expected::Inserted(1),
            DocChange::Update { .. } | DocChange::Replace { .. } | DocChange::Delete { .. } => {
                Expected::Matched(1)
            }
        }
    }

    /// The change in a line, with values as the engine's literals (`update
    /// of {_id: 7}`), for errors and the audit log.
    pub fn describe(&self, render: fn(&Value, Style) -> String) -> String {
        let key = |key: &DocKey| render(&key.0, Style::Compact);
        match self {
            DocChange::Update {
                key: id,
                set,
                unset,
                ..
            } => {
                let fields: Vec<String> = set
                    .iter()
                    .map(|(path, _)| path.dotted())
                    .chain(unset.iter().map(FieldPath::dotted))
                    .collect();
                format!("update of {} ({})", key(id), fields.join(", "))
            }
            DocChange::Replace { key: id, .. } => format!("replace of {}", key(id)),
            DocChange::Insert { document } => format!(
                "insert of {}",
                render(&Value::Object(document.clone()), Style::Compact)
            ),
            DocChange::Delete { key: id, .. } => format!("delete of {}", key(id)),
        }
    }
}

/// Where the next page of a browse starts.
#[derive(Debug, Clone, PartialEq)]
pub enum Continuation {
    /// After the document with this key (paging by key, in key order).
    After(Value),
    /// After skipping this many documents (sorted by something else).
    Skip(u64),
    /// The engine's own bookmark.
    Token(String),
}

/// A page of a collection, as the collection tab asks for it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BrowseSpec {
    /// The filter bar's text, in the engine's language.
    pub filter: Option<String>,
    /// Filters picked from the column headers.
    pub quick: Vec<Filter>,
    pub sort: Vec<(FieldPath, Direction)>,
    /// The projection bar's text, in the engine's language.
    pub projection: Option<String>,
    /// Documents in the page; 0 for all of them (exports).
    pub limit: usize,
    /// Where the page starts; `None` for the first.
    pub after: Option<Continuation>,
}

/// How many documents match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    Exact(u64),
    /// Counting stopped at this many.
    AtLeast(u64),
    /// Counting took too long.
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(value: &Value, _: Style) -> String {
        match value {
            Value::Int32(n) => n.to_string(),
            Value::Object(document) => format!("{{{} fields}}", document.0.len()),
            _ => "…".into(),
        }
    }

    #[test]
    fn each_kind_expects_one_document() {
        let key = DocKey(Value::Int32(7));
        let changes = [
            DocChange::Update {
                key: key.clone(),
                version: None,
                set: vec![(FieldPath::field("a"), Value::Int32(1))],
                unset: vec![FieldPath::parse("b.c")],
                expect: Vec::new(),
            },
            DocChange::Replace {
                key: key.clone(),
                version: None,
                document: Document::default(),
            },
            DocChange::Insert {
                document: Document(vec![("_id".into(), Value::Int32(8))]),
            },
            DocChange::Delete {
                key,
                version: None,
                expect: Vec::new(),
            },
        ];
        let expected: Vec<Expected> = changes.iter().map(DocChange::expected).collect();
        assert_eq!(
            expected,
            [
                Expected::Matched(1),
                Expected::Matched(1),
                Expected::Inserted(1),
                Expected::Matched(1)
            ]
        );
        let described: Vec<String> = changes.iter().map(|c| c.describe(render)).collect();
        assert_eq!(
            described,
            [
                "update of 7 (a, b.c)",
                "replace of 7",
                "insert of {1 fields}",
                "delete of 7"
            ]
        );
    }
}
