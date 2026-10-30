//! Documents in the grid: each batch flattened into rows (off the UI
//! thread), the columns it adds, and the documents themselves kept beside
//! the rows, since a cell's text is never the value it shows.

use kuery_core::catalog::ColumnInfo;
use kuery_core::document::flatten;
use kuery_core::document::language::Style;
use kuery_core::document::value::{Document, Value};
use kuery_core::result_set::Batch;

/// Documents beside the grid's rows (row i ↔ document i): the source of
/// truth for the detail, copies and exports; a cell's text never is.
#[derive(Default)]
pub struct DocumentRows {
    columns: Vec<String>,
    documents: Vec<Document>,
}

/// A batch made ready for the grid.
pub struct Flattened {
    /// Fields this batch is the first to have, as columns.
    pub new_columns: Vec<ColumnInfo>,
    /// Its rows, over the columns known before it and the new ones.
    pub batch: Batch,
}

impl DocumentRows {
    /// Flattens a batch (off the UI thread): the rows to add and the columns
    /// it adds. The first batch picks the columns' order; later ones only
    /// add theirs, up to the column cap.
    pub fn flatten(
        columns: &[String],
        batch: &[Document],
        render: fn(&Value, Style) -> String,
    ) -> Flattened {
        let new = match columns.is_empty() {
            true => flatten::columns(batch),
            false => flatten::more_columns(columns, batch),
        };
        let all: Vec<String> = columns.iter().chain(&new).cloned().collect();
        Flattened {
            new_columns: new.iter().map(|name| column_info(name, batch)).collect(),
            batch: flatten::batch(batch, &all, render),
        }
    }

    /// Flattens a batch into the columns `order` names, in that order,
    /// whichever fields its documents have: a SQL statement's columns. The
    /// first batch adds them all.
    pub fn flatten_ordered(
        columns: &[String],
        batch: &[Document],
        order: &[String],
        render: fn(&Value, Style) -> String,
    ) -> Flattened {
        let new: Vec<String> = order
            .iter()
            .filter(|name| !columns.contains(name))
            .cloned()
            .collect();
        let all: Vec<String> = columns.iter().chain(&new).cloned().collect();
        Flattened {
            new_columns: new.iter().map(|name| column_info(name, batch)).collect(),
            batch: flatten::batch(batch, &all, render),
        }
    }

    /// Keeps a batch's documents, after the grid took its rows.
    pub fn push(&mut self, batch: Vec<Document>, flattened: &Flattened) {
        self.columns
            .extend(flattened.new_columns.iter().map(|info| info.name.clone()));
        self.documents.extend(batch);
    }

    pub fn clear(&mut self) {
        self.columns.clear();
        self.documents.clear();
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// The documents of the rows, in row order.
    pub fn documents(&self) -> &[Document] {
        &self.documents
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    pub fn document(&self, row: usize) -> Option<&Document> {
        self.documents.get(row)
    }

    /// Whether the document of `row` lacks the field of `column` (an empty
    /// cell that is not NULL).
    pub fn is_absent(&self, row: usize, column: usize) -> bool {
        match (self.documents.get(row), self.columns.get(column)) {
            (Some(document), Some(name)) => flatten::is_absent(document, name),
            _ => false,
        }
    }
}

/// A field as a grid column: its type as most documents of the batch have
/// it, and `_id` marked as the key.
fn column_info(name: &str, batch: &[Document]) -> ColumnInfo {
    ColumnInfo {
        name: name.to_string(),
        data_type: flatten::type_label(batch, name),
        nullable: true,
        default: None,
        primary_key: name == "_id",
        foreign_key: None,
        generated: false,
        comment: None,
    }
}

#[cfg(test)]
mod tests {
    use kuery_core::result_set::ResultSet;

    use super::*;

    fn render(value: &Value, style: Style) -> String {
        (kuery_mongo::LANGUAGE.render)(value, style)
    }

    fn doc(fields: &[(&str, Value)]) -> Document {
        Document(
            fields
                .iter()
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect(),
        )
    }

    /// Flattens batches one after another, as the collection tab does, into
    /// a result set the way the grid holds it.
    fn load(batches: Vec<Vec<Document>>) -> (DocumentRows, ResultSet) {
        let mut rows = DocumentRows::default();
        let mut set = ResultSet::default();
        for batch in batches {
            let flattened = DocumentRows::flatten(rows.columns(), &batch, render);
            let names: Vec<String> = flattened
                .new_columns
                .iter()
                .map(|info| info.name.clone())
                .collect();
            set.extend_columns(&names);
            set.push(flattened.batch.clone());
            rows.push(batch, &flattened);
        }
        (rows, set)
    }

    #[test]
    fn a_later_batch_adds_a_column_that_earlier_rows_lack() {
        let first = vec![
            doc(&[
                ("_id", Value::Int32(1)),
                ("status", Value::String("paid".into())),
            ]),
            doc(&[("_id", Value::Int32(2)), ("status", Value::Null)]),
        ];
        let second = vec![doc(&[
            ("_id", Value::Int32(3)),
            ("coupon", Value::String("WELCOME".into())),
        ])];
        let (rows, set) = load(vec![first, second]);
        assert_eq!(set.columns(), ["_id", "status", "coupon"]);
        assert_eq!(rows.columns(), set.columns());
        assert_eq!(set.rows(), 3);
        assert_eq!(rows.len(), 3);
        // Earlier rows lack the late column: empty and absent.
        assert_eq!(set.cell(0, 2), Some(None));
        assert!(rows.is_absent(0, 2));
        assert_eq!(set.cell(2, 2), Some(Some("WELCOME")));
        assert!(!rows.is_absent(2, 2));
        // The third document lacks `status`.
        assert!(rows.is_absent(2, 1));
        assert_eq!(
            rows.document(2).and_then(|d| d.get("coupon")),
            Some(&Value::String("WELCOME".into()))
        );
    }

    #[test]
    fn tells_null_from_absent() {
        let (rows, set) = load(vec![vec![
            doc(&[("_id", Value::Int32(1)), ("note", Value::Null)]),
            doc(&[("_id", Value::Int32(2))]),
        ]]);
        // Both cells are empty in the grid…
        assert_eq!(set.cell(0, 1), Some(None));
        assert_eq!(set.cell(1, 1), Some(None));
        // …but only the second document lacks the field.
        assert!(!rows.is_absent(0, 1));
        assert!(rows.is_absent(1, 1));
        assert!(!rows.is_absent(9, 1));
    }

    #[test]
    fn stops_at_two_hundred_columns() {
        let wide = |from: i32| {
            Document(
                (from..from + 250)
                    .map(|i| (format!("f{i}"), Value::Int32(i)))
                    .collect(),
            )
        };
        let (rows, set) = load(vec![vec![wide(0)], vec![wide(1000)]]);
        assert_eq!(rows.columns().len(), flatten::MAX_COLUMNS);
        assert_eq!(set.columns().len(), 200);
        assert_eq!(set.rows(), 2);
    }

    #[test]
    fn marks_the_key_and_labels_types() {
        let batch = vec![doc(&[
            ("_id", Value::Int64(1)),
            ("total", Value::Decimal("9.99".into())),
        ])];
        let flattened = DocumentRows::flatten(&[], &batch, render);
        let columns: Vec<(&str, &str, bool)> = flattened
            .new_columns
            .iter()
            .map(|info| {
                (
                    info.name.as_str(),
                    info.data_type.as_str(),
                    info.primary_key,
                )
            })
            .collect();
        assert_eq!(
            columns,
            [("_id", "Int64", true), ("total", "Decimal128", false)]
        );
    }
}
