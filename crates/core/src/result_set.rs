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

/// A query result: column names and the batches received so far.
#[derive(Debug, Clone, Default)]
pub struct ResultSet {
    columns: Vec<String>,
    batches: Vec<Batch>,
    /// First row of each batch, to locate a row with binary search.
    batch_starts: Vec<usize>,
    rows: usize,
}

impl ResultSet {
    pub fn new(columns: Vec<String>) -> Self {
        Self {
            columns,
            ..Default::default()
        }
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn push(&mut self, batch: Batch) {
        debug_assert_eq!(batch.columns.len(), self.columns.len());
        if batch.rows == 0 {
            return;
        }
        self.batch_starts.push(self.rows);
        self.rows += batch.rows;
        self.batches.push(batch);
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
