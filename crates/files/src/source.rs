//! File descriptions and read options, independent of Polars and the UI.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

pub const MAX_JSON_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_PREVIEW_ROWS: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    Csv,
    Json,
    NdJson,
    Parquet,
}

impl FileFormat {
    pub fn from_path(path: &Path) -> Result<Self> {
        let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        match extension.to_ascii_lowercase().as_str() {
            "csv" | "tsv" => Ok(Self::Csv),
            "json" => Ok(Self::Json),
            "jsonl" | "ndjson" => Ok(Self::NdJson),
            "parquet" => Ok(Self::Parquet),
            _ => Err(Error::UnsupportedFormat(path.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvOptions {
    pub separator: u8,
    pub has_header: bool,
    /// An unquoted empty field is NULL by default. Quoted "" stays empty.
    pub empty_is_null: bool,
    /// Optional additional literal interpreted as NULL in every column.
    pub null_value: Option<String>,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            separator: b',',
            has_header: true,
            empty_is_null: true,
            null_value: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSource {
    pub path: PathBuf,
    pub format: FileFormat,
    pub csv: CsvOptions,
}

impl FileSource {
    /// Detects format from the extension without reading the file.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let format = FileFormat::from_path(&path)?;
        let mut csv = CsvOptions::default();
        if path
            .extension()
            .is_some_and(|s| s.eq_ignore_ascii_case("tsv"))
        {
            csv.separator = b'\t';
        }
        Ok(Self { path, format, csv })
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if !matches!(self.csv.separator, b',' | b';' | b'\t') {
            return Err(Error::InvalidOptions(
                "CSV separator must be comma, semicolon or tab".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewRequest {
    pub row_limit: usize,
    /// Budget for the text ResultSet buffers, not total parser memory/RSS.
    pub byte_limit: usize,
}

impl Default for PreviewRequest {
    fn default() -> Self {
        Self {
            row_limit: 10_000,
            byte_limit: 64 * 1024 * 1024,
        }
    }
}

impl PreviewRequest {
    pub(crate) fn validate(self) -> Result<()> {
        if self.row_limit == 0 || self.row_limit > MAX_PREVIEW_ROWS {
            return Err(Error::InvalidOptions(format!(
                "Preview row limit must be between 1 and {MAX_PREVIEW_ROWS}"
            )));
        }
        if self.byte_limit == 0 {
            return Err(Error::InvalidOptions(
                "Preview byte limit must be positive".into(),
            ));
        }
        Ok(())
    }
}
