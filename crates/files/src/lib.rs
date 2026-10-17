//! Local data files, independent of the UI and database sessions.
//!
//! Reading is synchronous: callers must use a background worker. Polars types
//! stay private; previews use the same ResultSet as SQL results and Profile.

mod convert;
mod preview;
mod reader;
mod source;
mod sql;
pub use sql::{SqlFile, SqlResult, query_files};

use std::path::PathBuf;

pub use preview::{ColumnSchema, FileFingerprint, LimitReason, PreviewResult, PreviewScope};
pub use reader::FileDataset;
pub use source::{
    CsvOptions, FileFormat, FileSource, MAX_JSON_BYTES, MAX_PREVIEW_ROWS, PreviewRequest,
};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Sql(String),
    #[error("Unsupported data file format: {0}")]
    UnsupportedFormat(PathBuf),
    #[error("Invalid read options: {0}")]
    InvalidOptions(String),
    #[error("Could not read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("Could not parse {format:?} file {path}: {message}")]
    Parse {
        path: PathBuf,
        format: FileFormat,
        message: String,
    },
    #[error("JSON file exceeds the {MAX_JSON_BYTES}-byte limit; use JSONL or Parquet")]
    JsonTooLarge,
    #[error("File changed while reading: {0}; reload it")]
    FileChanged(PathBuf),
    #[error("A row cannot fit in the preview byte budget ({0} bytes)")]
    RowTooLarge(usize),
}

impl FileSource {
    pub(crate) fn io_error(&self, source: std::io::Error) -> Error {
        Error::Io {
            path: self.path.clone(),
            source,
        }
    }

    pub(crate) fn parse_error(&self, message: impl Into<String>) -> Error {
        Error::Parse {
            path: self.path.clone(),
            format: self.format,
            message: message.into(),
        }
    }
}
