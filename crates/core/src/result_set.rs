//! Resultados en formato columnar.
//!
//! Las filas llegan por lotes y se guardan como columnas de Arrow, no como
//! `Vec<Vec<String>>`: cada columna es un único buffer de texto con offsets, así
//! que un millón de filas son pocas asignaciones y la interfaz solo formatea las
//! celdas visibles.

use std::sync::Arc;

use arrow_array::builder::StringBuilder;
use arrow_array::{Array, StringArray};

/// Un lote de filas, columna a columna.
#[derive(Debug, Clone)]
pub struct Batch {
    columns: Vec<Arc<StringArray>>,
    rows: usize,
}

impl Batch {
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Bytes que ocupan los buffers de Arrow del lote.
    pub fn memory_size(&self) -> usize {
        self.columns.iter().map(|c| c.get_array_memory_size()).sum()
    }
}

/// Acumula filas hasta formar un [`Batch`].
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

    /// Añade una fila. `value(i)` devuelve la celda de la columna `i`, `None` es NULL.
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

/// Resultado de una query: nombres de columna y lotes recibidos hasta ahora.
#[derive(Debug, Clone, Default)]
pub struct ResultSet {
    columns: Vec<String>,
    batches: Vec<Batch>,
    /// Primera fila de cada lote, para localizar una fila con búsqueda binaria.
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

    /// Valor de una celda. `None` si la fila no existe, `Some(None)` si es NULL.
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

    /// Bytes que ocupan los datos en memoria.
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
