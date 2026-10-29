//! A document engine's native language, described as data and pure
//! functions, like [`crate::sql::Dialect`]: how a script splits into
//! statements and what each one does, how its literals read and print, and
//! what completion offers. Nothing here runs anything, so it is safe on the
//! UI thread, and the session analyses every script with it before the
//! driver sees a byte.

use std::fmt;
use std::ops::Range;

use crate::analysis::StatementKind;
use crate::document::value::{CollectionRef, Document, FieldPath, Value};

/// How a value prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// On one line, for cells and messages.
    Compact,
    /// Indented, for the document view and the editor.
    Pretty,
}

/// Text the language cannot read, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// The bytes at fault, for the editor's diagnostic.
    pub span: Range<usize>,
    pub message: String,
}

impl ParseError {
    /// `line 2, column 5: …`, counting from 1, for messages about `text`.
    pub fn located(&self, text: &str) -> String {
        let end = (0..=self.span.start.min(text.len()))
            .rev()
            .find(|&at| text.is_char_boundary(at))
            .unwrap_or(0);
        let before = &text[..end];
        let line = before.matches('\n').count() + 1;
        let column = before
            .rfind('\n')
            .map_or(before, |newline| &before[newline + 1..])
            .chars()
            .count()
            + 1;
        format!("line {line}, column {column}: {}", self.message)
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Why a statement is easy to regret, in words the review shows as they
/// are (`deleteMany with an empty filter removes every document`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Danger(pub String);

impl fmt::Display for Danger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What one statement does, read from its text.
#[derive(Debug, Clone, PartialEq)]
pub struct OpInfo {
    /// Read, data change, schema change… What the language cannot read
    /// counts as a write.
    pub kind: StatementKind,
    /// The collection it works on, when it names one.
    pub target: Option<CollectionRef>,
    /// Short description, such as `deleteMany orders`.
    pub summary: String,
    pub dangers: Vec<Danger>,
    /// It weighs on the server (JavaScript the server runs, an index
    /// build): only allowed where the caller allows it.
    pub heavy: bool,
    /// Whether its plan can be shown.
    pub explainable: bool,
    /// The container the statements after it work in, for one that
    /// changes it (`use shop` enters `["shop"]`).
    pub enters: Option<Vec<String>>,
}

/// A statement of a script: where it is, and what it does.
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    pub span: Range<usize>,
    pub info: OpInfo,
}

/// What a filter bar's text does, beyond reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterCheck {
    /// It runs JavaScript on the server.
    pub heavy: bool,
}

/// What the text before the cursor asks completion for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionContext {
    /// A collection's name (after `db.`).
    Collection {
        prefix: String,
    },
    /// A method of a collection (after `db.orders.`).
    Method {
        collection: String,
        prefix: String,
    },
    /// An operator, in a key that starts with `$`.
    Operator {
        prefix: String,
    },
    /// A pipeline stage.
    Stage {
        prefix: String,
    },
    /// A field of the documents.
    Field {
        collection: Option<String>,
        prefix: String,
    },
    Nothing,
}

/// A document engine's native language (mongosh's commands, Mango JSON,
/// SurrealQL…).
pub struct Language {
    /// `mongosh`, for the console's language picker.
    pub name: &'static str,
    /// The editor's highlighting (`"javascript"`).
    pub editor: &'static str,
    /// Splits a script into statements and classifies each, without
    /// running anything.
    pub analyze: fn(&str) -> Result<Vec<Statement>, ParseError>,
    /// Checks a filter or projection bar's text (`{status: "paid"}`).
    pub check_filter: fn(&str) -> Result<FilterCheck, ParseError>,
    /// Reads a document from the editor.
    pub parse_document: fn(&str) -> Result<Document, ParseError>,
    /// A value as a typed literal that reads back the same:
    /// `NumberLong("5")`, `ObjectId("…")`.
    pub render: fn(&Value, Style) -> String,
    /// The filter bar's text that keeps the documents holding `value` at
    /// `path`, or, for `None`, those that lack the field. A null keeps only
    /// the documents whose field holds null, not those that lack it.
    /// `None` when the language cannot say it.
    pub field_filter: fn(&FieldPath, Option<&Value>) -> Option<String>,
    /// What completion offers at a byte offset of the text.
    pub context: fn(&str, usize) -> CompletionContext,
    /// The statement that makes the ones after it work in a container of
    /// the tree (`use shop` for `["shop"]`); `None` when the language has
    /// none for it.
    pub enter: fn(&[String]) -> Option<String>,
    /// What completion inserts for a collection where
    /// [`CompletionContext::Collection`] asks for one: `orders`, or
    /// `getCollection("order-lines")` for a name that does not read bare.
    pub collection_ref: fn(&str) -> String,
    /// A field's name as a document's key: bare where it reads so, quoted
    /// otherwise (`status`, `"ship.city"`).
    pub key: fn(&str) -> String,
    pub methods: &'static [&'static str],
    pub operators: &'static [&'static str],
    pub stages: &'static [&'static str],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locates_errors_by_line_and_column() {
        let error = ParseError {
            span: 12..13,
            message: "expected a value".into(),
        };
        assert_eq!(
            error.located("db.a.find(\n {a: }"),
            "line 2, column 2: expected a value"
        );
        let at_start = ParseError {
            span: 0..1,
            message: "no".into(),
        };
        assert_eq!(at_start.located("x"), "line 1, column 1: no");
        // A span past the end stays at the end.
        let past = ParseError {
            span: 40..41,
            message: "unfinished".into(),
        };
        assert_eq!(past.located("ab"), "line 1, column 3: unfinished");
    }
}
