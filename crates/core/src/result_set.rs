//! Results in columnar format.
//!
//! Rows arrive in batches and are stored as Arrow columns, not as
//! `Vec<Vec<String>>`: each column is a single text buffer with offsets, so a
//! million rows take only a few allocations and the UI only formats the visible
//! cells.

use std::sync::Arc;

use arrow_array::builder::StringBuilder;
use arrow_array::{Array, StringArray};

/// A batch of rows, column by column.
#[derive(Debug, Clone)]
pub struct Batch {
    columns: Vec<Arc<StringArray>>,
    rows: usize,
}

impl Batch {
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Bytes used by the batch's Arrow buffers.
    pub fn memory_size(&self) -> usize {
        self.columns.iter().map(|c| c.get_array_memory_size()).sum()
    }
}

/// Accumulates rows until they form a [`Batch`].
pub struct BatchBuilder {
    columns: Vec<StringBuilder>,
    rows: usize,
}

impl BatchBuilder {
    pub fn new(column_count: usize, row_capacity: usize) -> Self {
        Self {
            columns: (0..column_count)
                .map(|_| StringBuilder::with_capacity(row_capacity, row_capacity * 16))
                .collect(),
            rows: 0,
        }
    }

    /// Adds a row. `value(i)` returns the cell for column `i`; `None` is NULL.
    pub fn push_row<'a>(&mut self, mut value: impl FnMut(usize) -> Option<&'a str>) {
        for (index, column) in self.columns.iter_mut().enumerate() {
            column.append_option(value(index));
        }
        self.rows += 1;
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn finish(&mut self) -> Batch {
        let rows = std::mem::take(&mut self.rows);
        Batch {
            columns: self
                .columns
                .iter_mut()
                .map(|c| Arc::new(c.finish()))
                .collect(),
            rows,
        }
    }
}

/// What a column holds, as far as statistics care. Values are always kept
/// as the text PostgreSQL sent; the kind says how to read them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColumnKind {
    Integer,
    /// Floating point or numeric.
    Number,
    Boolean,
    Date,
    Timestamp,
    Text,
    /// Nested documents and arrays, as compact literals.
    Json,
    /// A document's key (`_id`), shown as a literal.
    Id,
    /// Not reported by the server: infer it from the values.
    #[default]
    Unknown,
}

impl ColumnKind {
    pub fn is_numeric(self) -> bool {
        matches!(self, ColumnKind::Integer | ColumnKind::Number)
    }

    pub fn is_temporal(self) -> bool {
        matches!(self, ColumnKind::Date | ColumnKind::Timestamp)
    }

    pub fn label(self) -> &'static str {
        match self {
            ColumnKind::Integer => "integer",
            ColumnKind::Number => "number",
            ColumnKind::Boolean => "boolean",
            ColumnKind::Date => "date",
            ColumnKind::Timestamp => "timestamp",
            ColumnKind::Text => "text",
            ColumnKind::Json => "json",
            ColumnKind::Id => "id",
            ColumnKind::Unknown => "unknown",
        }
    }
}

/// A query result: column names and the batches received so far.
#[derive(Debug, Clone, Default)]
pub struct ResultSet {
    columns: Vec<String>,
    /// One per column; `Unknown` until the driver reports them.
    kinds: Vec<ColumnKind>,
    batches: Vec<Batch>,
    /// First row of each batch, to locate a row with binary search.
    batch_starts: Vec<usize>,
    rows: usize,
}

impl ResultSet {
    pub fn new(columns: Vec<String>) -> Self {
        Self {
            kinds: vec![ColumnKind::Unknown; columns.len()],
            columns,
            ..Default::default()
        }
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn kinds(&self) -> &[ColumnKind] {
        &self.kinds
    }

    /// Sets the columns' kinds; ignored if the count does not match.
    pub fn set_kinds(&mut self, kinds: Vec<ColumnKind>) {
        if kinds.len() == self.columns.len() {
            self.kinds = kinds;
        }
    }

    /// Every value of one column, in order; `None` is NULL.
    pub fn column_values(&self, column: usize) -> impl Iterator<Item = Option<&str>> + '_ {
        self.batches.iter().flat_map(move |batch| {
            let array = batch.columns.get(column);
            (0..batch.rows).map(move |offset| {
                array.and_then(|array| (!array.is_null(offset)).then(|| array.value(offset)))
            })
        })
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Appends a batch with one array per column: columns that show up
    /// later come through [`Self::extend_columns`] first.
    pub fn push(&mut self, batch: Batch) {
        assert_eq!(
            batch.columns.len(),
            self.columns.len(),
            "a batch must have the result's columns; extend them first"
        );
        if batch.rows == 0 {
            return;
        }
        self.batch_starts.push(self.rows);
        self.rows += batch.rows;
        self.batches.push(batch);
    }

    /// Adds columns that showed up after rows arrived (a field only later
    /// documents have): the rows already here get NULL in each, and their
    /// kinds are `Unknown` until set.
    pub fn extend_columns(&mut self, names: &[String]) {
        for batch in &mut self.batches {
            let rows = batch.rows;
            batch
                .columns
                .extend(names.iter().map(|_| Arc::new(StringArray::new_null(rows))));
        }
        self.columns.extend(names.iter().cloned());
        self.kinds.extend(names.iter().map(|_| ColumnKind::Unknown));
    }

    /// A cell's value. `None` if the row does not exist, `Some(None)` if it is NULL.
    pub fn cell(&self, row: usize, column: usize) -> Option<Option<&str>> {
        if row >= self.rows {
            return None;
        }
        let batch_index = self.batch_starts.partition_point(|&start| start <= row) - 1;
        let batch = &self.batches[batch_index];
        let array = batch.columns.get(column)?;
        let offset = row - self.batch_starts[batch_index];
        Some(if array.is_null(offset) {
            None
        } else {
            Some(array.value(offset))
        })
    }

    /// Bytes the data takes up in memory.
    pub fn memory_size(&self) -> usize {
        self.batches.iter().map(Batch::memory_size).sum()
    }

    /// Calls `f` with every row in order, batch by batch (no lookup per
    /// cell). Stops at the first error.
    pub fn try_for_each_row<E>(
        &self,
        mut f: impl FnMut(&[Option<&str>]) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut row = Vec::with_capacity(self.columns.len());
        for batch in &self.batches {
            for offset in 0..batch.rows {
                row.clear();
                row.extend(
                    batch
                        .columns
                        .iter()
                        .map(|array| (!array.is_null(offset)).then(|| array.value(offset))),
                );
                f(&row)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(rows: &[[Option<&str>; 2]]) -> Batch {
        let mut builder = BatchBuilder::new(2, rows.len());
        for row in rows {
            builder.push_row(|i| row[i]);
        }
        builder.finish()
    }

    #[test]
    fn finds_cells_across_batches() {
        let mut set = ResultSet::new(vec!["id".into(), "name".into()]);
        set.push(batch(&[[Some("1"), Some("a")], [Some("2"), None]]));
        set.push(batch(&[]));
        set.push(batch(&[[Some("3"), Some("c")]]));

        assert_eq!(set.rows(), 3);
        assert_eq!(set.cell(0, 1), Some(Some("a")));
        assert_eq!(set.cell(1, 1), Some(None));
        assert_eq!(set.cell(2, 0), Some(Some("3")));
        assert_eq!(set.cell(3, 0), None);
        assert_eq!(set.cell(0, 5), None);
    }

    #[test]
    fn columns_that_show_up_later_are_null_in_earlier_rows() {
        let mut set = ResultSet::new(vec!["id".into(), "name".into()]);
        set.push(batch(&[[Some("1"), Some("a")]]));
        set.push(batch(&[[Some("2"), Some("b")]]));
        set.extend_columns(&["note".into()]);
        assert_eq!(set.columns(), ["id", "name", "note"]);
        assert_eq!(set.kinds()[2], ColumnKind::Unknown);
        assert_eq!(set.cell(0, 2), Some(None));
        assert_eq!(set.cell(1, 2), Some(None));

        let mut builder = BatchBuilder::new(3, 1);
        builder.push_row(|i| [Some("3"), Some("c"), Some("late")][i]);
        set.push(builder.finish());
        assert_eq!(set.cell(2, 2), Some(Some("late")));
        assert_eq!(
            set.column_values(2).collect::<Vec<_>>(),
            [None, None, Some("late")]
        );
    }

    #[test]
    #[should_panic(expected = "extend them first")]
    fn a_batch_with_other_columns_is_refused() {
        let mut set = ResultSet::new(vec!["id".into()]);
        set.push(batch(&[[Some("1"), Some("a")]]));
    }

    #[test]
    fn builder_resets_after_finish() {
        let mut builder = BatchBuilder::new(1, 4);
        builder.push_row(|_| Some("x"));
        assert_eq!(builder.finish().rows(), 1);
        assert_eq!(builder.rows(), 0);
        builder.push_row(|_| Some("y"));
        let second = builder.finish();
        let mut set = ResultSet::new(vec!["v".into()]);
        set.push(second);
        assert_eq!(set.cell(0, 0), Some(Some("y")));
    }
}
