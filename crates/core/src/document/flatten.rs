//! Documents as rows of the grid: each top-level field a column, `_id`
//! first, the fields most documents have next. Documents of one collection
//! need not share their fields, so a field may be absent from a row (a
//! dimmed dash), which is not the same as holding null (the usual NULL);
//! the grid tells them apart with [`is_absent`]. Nested documents and
//! arrays read as compact literals, which the inspector formats.

use std::collections::HashMap;

use crate::document::language::Style;
use crate::document::value::{Document, Value};
use crate::result_set::{Batch, BatchBuilder, ColumnKind};

/// Columns past this are left out: wider grids are unreadable, and the
/// document view still shows every field.
pub const MAX_COLUMNS: usize = 200;

/// The first batch's columns: `_id` first, then by how many documents have
/// the field, then by first appearance; at most [`MAX_COLUMNS`].
pub fn columns(documents: &[Document]) -> Vec<String> {
    let mut order: Vec<&str> = Vec::new();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for document in documents {
        for (name, _) in &document.0 {
            let count = counts.entry(name.as_str()).or_insert(0);
            if *count == 0 {
                order.push(name.as_str());
            }
            *count += 1;
        }
    }
    // Stable: equal counts keep their first appearance.
    order.sort_by_key(|name| (*name != "_id", std::cmp::Reverse(counts[name])));
    order
        .into_iter()
        .take(MAX_COLUMNS)
        .map(str::to_string)
        .collect()
}

/// Fields of a later batch that `known` lacks, by first appearance, as many
/// as fit under [`MAX_COLUMNS`].
pub fn more_columns(known: &[String], documents: &[Document]) -> Vec<String> {
    let room = MAX_COLUMNS.saturating_sub(known.len());
    let mut more: Vec<String> = Vec::new();
    for document in documents {
        for (name, _) in &document.0 {
            if more.len() == room {
                return more;
            }
            if !known.contains(name) && !more.contains(name) {
                more.push(name.clone());
            }
        }
    }
    more
}

/// One row as text: strings as they are, every other value as the engine's
/// literal (`render` with [`Style::Compact`]); `None` for null and for an
/// absent field ([`is_absent`] tells them apart).
pub fn row(
    document: &Document,
    columns: &[String],
    render: fn(&Value, Style) -> String,
) -> Vec<Option<String>> {
    columns
        .iter()
        .map(|column| match document.get(column)? {
            Value::Null => None,
            Value::String(text) => Some(text.clone()),
            value => Some(render(value, Style::Compact)),
        })
        .collect()
}

/// Rows of `documents` as a batch for the result set.
pub fn batch(
    documents: &[Document],
    columns: &[String],
    render: fn(&Value, Style) -> String,
) -> Batch {
    let mut builder = BatchBuilder::new(columns.len(), documents.len());
    for document in documents {
        let cells = row(document, columns, render);
        builder.push_row(|index| cells.get(index).and_then(Option::as_deref));
    }
    builder.finish()
}

/// Whether the document lacks the field altogether.
pub fn is_absent(document: &Document, column: &str) -> bool {
    document.get(column).is_none()
}

/// The kind of a column, from the type most of its values have: `_id` is
/// always [`ColumnKind::Id`], nested documents and arrays
/// [`ColumnKind::Json`]. Nulls do not count; with no other value it is
/// `Unknown`.
pub fn kind<'a>(column: &str, values: impl Iterator<Item = &'a Value>) -> ColumnKind {
    if column == "_id" {
        return ColumnKind::Id;
    }
    let mut counts: Vec<(ColumnKind, usize)> = Vec::new();
    for value in values {
        let kind = match value {
            Value::Null => continue,
            Value::Bool(_) => ColumnKind::Boolean,
            Value::Int32(_) | Value::Int64(_) => ColumnKind::Integer,
            Value::Double(_) | Value::Decimal(_) => ColumnKind::Number,
            Value::DateTime(_) => ColumnKind::Timestamp,
            Value::Array(_) | Value::Object(_) => ColumnKind::Json,
            Value::String(_) | Value::Binary { .. } | Value::Ext(_) => ColumnKind::Text,
        };
        match counts.iter_mut().find(|(seen, _)| *seen == kind) {
            Some((_, count)) => *count += 1,
            None => counts.push((kind, 1)),
        }
    }
    // The first kind seen wins a tie.
    counts
        .iter()
        .rev()
        .max_by_key(|(_, count)| *count)
        .map_or(ColumnKind::Unknown, |(kind, _)| *kind)
}

/// What the column header says about a field's type: its type name
/// (`Int64`) when at least nine in ten documents that have it agree,
/// `mixed` otherwise, `absent` when no document has it.
pub fn type_label(documents: &[Document], column: &str) -> String {
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    let mut present = 0;
    for value in documents.iter().filter_map(|document| document.get(column)) {
        present += 1;
        let name = value.type_name();
        match counts.iter_mut().find(|(seen, _)| *seen == name) {
            Some((_, count)) => *count += 1,
            None => counts.push((name, 1)),
        }
    }
    match counts.iter().rev().max_by_key(|(_, count)| *count) {
        None => "absent".into(),
        Some((name, count)) if count * 10 >= present * 9 => name.to_string(),
        Some(_) => "mixed".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(value: &Value, _: Style) -> String {
        match value {
            Value::Int32(n) => n.to_string(),
            Value::Object(_) => "{…}".into(),
            other => format!("{other:?}"),
        }
    }

    fn doc(fields: &[(&str, Value)]) -> Document {
        Document(
            fields
                .iter()
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect(),
        )
    }

    fn sample() -> Vec<Document> {
        let nested = Value::Object(doc(&[("x", Value::Int32(1))]));
        vec![
            doc(&[("_id", Value::Int32(1)), ("a", Value::Int32(1))]),
            doc(&[("_id", Value::Int32(2)), ("b", Value::Null)]),
            doc(&[
                ("a", Value::Int32(2)),
                ("_id", Value::Int32(3)),
                ("c", nested),
            ]),
        ]
    }

    #[test]
    fn puts_the_key_first_then_the_commonest_fields() {
        assert_eq!(columns(&sample()), ["_id", "a", "b", "c"]);
        let late = doc(&[("d", Value::Int32(1)), ("a", Value::Int32(1))]);
        assert_eq!(more_columns(&["_id".into(), "a".into()], &[late]), ["d"]);
        assert!(columns(&[]).is_empty());
    }

    #[test]
    fn stops_at_the_column_cap() {
        let wide = Document(
            (0..250)
                .map(|i| (format!("f{i}"), Value::Int32(i)))
                .collect(),
        );
        let first = columns(std::slice::from_ref(&wide));
        assert_eq!(first.len(), MAX_COLUMNS);
        assert_eq!(first[0], "f0");
        assert!(more_columns(&first, std::slice::from_ref(&wide)).is_empty());
        assert_eq!(more_columns(&first[..190], &[wide]).len(), 10);
    }

    #[test]
    fn tells_an_absent_field_from_a_null_one() {
        let documents = sample();
        let names = columns(&documents);
        let second = row(&documents[1], &names, render);
        assert_eq!(second, [Some("2".into()), None, None, None]);
        assert!(is_absent(&documents[1], "a"));
        assert!(!is_absent(&documents[1], "b"));
        assert_eq!(
            row(&documents[2], &names, render),
            [Some("3".into()), Some("2".into()), None, Some("{…}".into())]
        );
        let text = doc(&[("s", Value::String("paid".into()))]);
        assert_eq!(row(&text, &["s".into()], render), [Some("paid".into())]);

        let batch = batch(&documents, &names, render);
        assert_eq!(batch.rows(), 3);
    }

    #[test]
    fn reads_a_kind_from_the_commonest_type() {
        let values = [Value::Int32(1), Value::Int32(2), Value::String("x".into())];
        assert_eq!(kind("n", values.iter()), ColumnKind::Integer);
        let nested = [Value::Object(Document::default()), Value::Array(Vec::new())];
        assert_eq!(kind("n", nested.iter()), ColumnKind::Json);
        assert_eq!(kind("_id", values.iter()), ColumnKind::Id);
        assert_eq!(kind("n", [Value::Null].iter()), ColumnKind::Unknown);
        let tie = [Value::Bool(true), Value::DateTime(0)];
        assert_eq!(kind("n", tie.iter()), ColumnKind::Boolean);
    }

    #[test]
    fn labels_a_field_mixed_below_nine_in_ten() {
        let prices = |doubles: usize, strings: usize| -> Vec<Document> {
            (0..doubles)
                .map(|_| doc(&[("price", Value::Double(1.5))]))
                .chain((0..strings).map(|_| doc(&[("price", Value::String("1.5".into()))])))
                .chain([Document::default()])
                .collect()
        };
        assert_eq!(type_label(&prices(8, 2), "price"), "mixed");
        assert_eq!(type_label(&prices(9, 1), "price"), "Double");
        assert_eq!(type_label(&prices(3, 0), "other"), "absent");
    }
}
