//! Bounded local reads. All Polars execution stays on the caller's worker.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Cursor, Read};
use std::num::NonZeroUsize;

use polars::prelude::*;
use serde_json::Value;

use crate::{
    Error, FileFingerprint, FileFormat, FileSource, MAX_JSON_BYTES, PreviewRequest, PreviewResult,
    Result, convert,
};

/// An opened local source. Each preview creates a fresh typed Polars plan;
/// no entire file or Polars object is retained by the UI.
#[derive(Debug, Clone)]
pub struct FileDataset {
    source: FileSource,
    fingerprint: FileFingerprint,
}

impl FileDataset {
    pub fn open(mut source: FileSource) -> Result<Self> {
        source.validate()?;
        source.path = fs::canonicalize(&source.path).map_err(|e| source.io_error(e))?;
        let fingerprint = fingerprint(&source)?;
        if source.format == FileFormat::Json && fingerprint.bytes > MAX_JSON_BYTES {
            return Err(Error::JsonTooLarge);
        }
        Ok(Self {
            source,
            fingerprint,
        })
    }

    pub fn source(&self) -> &FileSource {
        &self.source
    }

    pub fn fingerprint(&self) -> &FileFingerprint {
        &self.fingerprint
    }

    /// Reads a prefix, never a representative sample or an automatic full count.
    /// Reopen the dataset to accept a changed source. This method is synchronous.
    pub fn preview(&self, request: PreviewRequest) -> Result<PreviewResult> {
        request.validate()?;
        self.check_unchanged()?;
        let result = self
            .read_rows(Some(request.row_limit + 1))
            .and_then(|frame| {
                // Preserve original CSV numeric lexemes: mixed integer/float
                // inference may promote an integer above 2^53 to Float64.
                let display = if self.source.format == FileFormat::Csv {
                    let display = csv_plan(&self.source, request.row_limit + 1, 0)
                        .and_then(|lf| lf.limit((request.row_limit + 1) as IdxSize).collect())
                        .map_err(|e| self.source.parse_error(e.to_string()))?;
                    if display.shape() != frame.shape()
                        || display.get_column_names() != frame.get_column_names()
                    {
                        return Err(Error::FileChanged(self.source.path.clone()));
                    }
                    Some(display)
                } else {
                    None
                };
                convert::preview(
                    &frame,
                    display.as_ref(),
                    &self.source,
                    request,
                    self.fingerprint.clone(),
                )
            });
        // Prefer the change error even when parsing failed during replacement.
        self.check_unchanged()?;
        result
    }

    /// Full-source plans for SQL. JSON inputs are validated in full before SQL
    /// so their deserializer cannot silently turn incompatible values into NULL.
    pub(crate) fn sql_plan(&self) -> Result<LazyFrame> {
        self.check_unchanged()?;
        match self.source.format {
            FileFormat::Csv => {
                let mut plan = csv_plan(&self.source, usize::MAX, 1_000)
                    .map_err(|e| self.source.parse_error(e.to_string()))?;
                validate_csv_float_integers(&self.source, &mut plan)?;
                Ok(plan)
            }
            FileFormat::Parquet => {
                let file = File::open(&self.source.path).map_err(|e| self.source.io_error(e))?;
                LazyFrame::scan_parquet_sources(
                    ScanSources::Files(vec![file].into()),
                    ScanArgsParquet {
                        glob: false,
                        hive_options: polars::io::HiveOptions::new_disabled(),
                        ..Default::default()
                    },
                )
                .map_err(|e| self.source.parse_error(e.to_string()))
            }
            FileFormat::Json | FileFormat::NdJson => self.read_rows(None).map(DataFrame::lazy),
        }
    }

    pub(crate) fn check_unchanged(&self) -> Result<()> {
        if fingerprint(&self.source)? != self.fingerprint {
            return Err(Error::FileChanged(self.source.path.clone()));
        }
        Ok(())
    }

    fn read_rows(&self, limit: Option<usize>) -> Result<DataFrame> {
        let source = &self.source;
        let count = limit.unwrap_or(usize::MAX);
        // Canonical local paths cannot become cloud URLs. Disable glob expansion
        // so filenames containing brackets are read literally.
        let file = File::open(&source.path).map_err(|e| source.io_error(e))?;
        let sources = ScanSources::Files(vec![file].into());
        let frame = match source.format {
            FileFormat::Csv => {
                csv_plan(source, count, 1_000.min(count)).and_then(|lf| lf.collect())
            }
            FileFormat::Parquet => LazyFrame::scan_parquet_sources(
                sources,
                ScanArgsParquet {
                    n_rows: limit,
                    glob: false,
                    // Single opened files have no partition directory context.
                    hive_options: polars::io::HiveOptions::new_disabled(),
                    ..Default::default()
                },
            )
            .and_then(|lf| lf.collect()),
            FileFormat::NdJson => {
                // Polars' JSON deserializer can silently null incompatible
                // values. Validate the loaded prefix against the original rows.
                // Validate shape and record size before letting Polars parse.
                // These passes visit one record at a time, not a file-sized Vec.
                let mut has_fields = false;
                let records = visit_json_lines(source, count, |_, row| {
                    has_fields |= row.as_object().is_some_and(|object| !object.is_empty());
                    Ok(())
                })?;
                if records == 0 {
                    return Ok(DataFrame::empty());
                }
                if !has_fields {
                    return Ok(DataFrame::empty_with_height(records));
                }
                let frame = LazyJsonLineReader::new_with_sources(sources)
                    .with_n_rows(limit)
                    .with_infer_schema_length(NonZeroUsize::new(1_000.min(count)))
                    .with_ignore_errors(false)
                    .finish()
                    .and_then(|lf| lf.collect())
                    .map_err(|e| source.parse_error(e.to_string()))?;
                if frame.height() != records {
                    return Err(source
                        .parse_error("JSONL records do not produce a consistent table schema"));
                }
                visit_json_lines(source, count, |index, row| {
                    validate_json_row(source, &frame, index, row)
                })?;
                return Ok(frame);
            }
            FileFormat::Json => {
                let file = File::open(&source.path).map_err(|e| source.io_error(e))?;
                let mut bytes = Vec::new();
                file.take(MAX_JSON_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| source.io_error(e))?;
                if bytes.len() as u64 > MAX_JSON_BYTES {
                    return Err(Error::JsonTooLarge);
                }
                validate_integer_literals(source, &bytes)?;
                let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
                    source.parse_error(format!(
                        "Invalid JSON at line {}, column {}",
                        e.line(),
                        e.column()
                    ))
                })?;
                let Value::Array(mut rows) = value else {
                    return Err(source.parse_error("Expected a JSON array of objects"));
                };
                // The container was validated in full; only materialize the
                // prefix in Polars. The original file is never modified.
                if rows.iter().any(|row| !row.is_object()) {
                    return Err(source.parse_error("Expected a JSON array of objects"));
                }
                rows.truncate(count);
                return json_frame(source, rows);
            }
        };
        frame.map_err(|e| source.parse_error(e.to_string()))
    }
}

fn fingerprint(source: &FileSource) -> Result<FileFingerprint> {
    let metadata = fs::metadata(&source.path).map_err(|e| source.io_error(e))?;
    if !metadata.is_file() {
        return Err(Error::InvalidOptions(
            "Data source must be a regular file".into(),
        ));
    }
    Ok(FileFingerprint {
        bytes: metadata.len(),
        modified: metadata.modified().map_err(|e| source.io_error(e))?,
    })
}

fn csv_plan(source: &FileSource, count: usize, infer_rows: usize) -> PolarsResult<LazyFrame> {
    let file = File::open(&source.path)?;
    LazyCsvReader::new_with_sources(ScanSources::Files(vec![file].into()))
        .with_glob(false)
        .with_has_header(source.csv.has_header)
        .with_separator(source.csv.separator)
        .with_missing_is_null(source.csv.empty_is_null)
        .with_null_values(
            source
                .csv
                .null_value
                .as_ref()
                .map(|s| NullValues::AllColumnsSingle(s.as_str().into())),
        )
        .with_infer_schema_length(Some(infer_rows))
        .with_n_rows((count != usize::MAX).then_some(count))
        .with_ignore_errors(false)
        .with_raise_if_empty(false)
        .finish()
}

fn visit_json_lines(
    source: &FileSource,
    count: usize,
    mut visit: impl FnMut(usize, &Value) -> Result<()>,
) -> Result<usize> {
    let file = File::open(&source.path).map_err(|e| source.io_error(e))?;
    let mut reader = BufReader::new(file);
    let mut records = 0;
    let mut line = Vec::new();
    let mut line_number = 0;
    while records < count {
        line.clear();
        // take() bounds allocation even for a huge record without a newline.
        let bytes = Read::by_ref(&mut reader)
            .take(MAX_JSON_BYTES + 1)
            .read_until(b'\n', &mut line)
            .map_err(|e| source.io_error(e))?;
        if bytes == 0 {
            break;
        }
        line_number += 1;
        if bytes as u64 > MAX_JSON_BYTES {
            return Err(source.parse_error(format!(
                "JSONL record {line_number} exceeds the 32 MiB record limit"
            )));
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        validate_integer_literals(source, &line)?;
        let value: Value = serde_json::from_slice(&line).map_err(|e| {
            source.parse_error(format!(
                "Invalid JSONL at line {line_number}, column {}",
                e.column()
            ))
        })?;
        if !value.is_object() {
            return Err(
                source.parse_error(format!("JSONL line {line_number} must contain an object"))
            );
        }
        visit(records, &value)?;
        records += 1;
    }
    Ok(records)
}

fn json_frame(source: &FileSource, rows: Vec<Value>) -> Result<DataFrame> {
    if rows.is_empty() {
        return Ok(DataFrame::empty());
    }
    if rows
        .iter()
        .all(|row| row.as_object().is_some_and(|object| object.is_empty()))
    {
        return Ok(DataFrame::empty_with_height(rows.len()));
    }
    let bytes = serde_json::to_vec(&rows).map_err(|e| source.parse_error(e.to_string()))?;
    let frame = JsonReader::new(Cursor::new(bytes))
        .with_json_format(JsonFormat::Json)
        .infer_schema_len(NonZeroUsize::new(1_000))
        .with_ignore_errors(false)
        .finish()
        .map_err(|e| source.parse_error(e.to_string()))?;
    if frame.height() != rows.len() {
        return Err(source.parse_error("JSON records do not produce a consistent table schema"));
    }
    // Compare decoded values, not display strings. This catches silent NULLs,
    // dropped fields, coercions and integer precision loss (including nested data).
    for (index, row) in rows.iter().enumerate() {
        validate_json_row(source, &frame, index, row)?;
    }
    Ok(frame)
}

fn validate_json_row(
    source: &FileSource,
    frame: &DataFrame,
    index: usize,
    row: &Value,
) -> Result<()> {
    let object = row.as_object().expect("validated JSON objects");
    for (name, expected) in object {
        let column = frame.column(name).map_err(|_| {
            source.parse_error(format!(
                "JSON row {} has a field absent from the inferred schema: {name}",
                index + 1
            ))
        })?;
        let actual = column
            .get(index)
            .map_err(|e| source.parse_error(e.to_string()))?;
        let actual = convert::json_value(actual).map_err(|e| source.parse_error(e.to_string()))?;
        if !json_equivalent(expected, &actual) {
            return Err(source.parse_error(format!(
                "JSON row {}, field {name}: value conflicts with the inferred schema",
                index + 1
            )));
        }
    }
    Ok(())
}

fn json_equivalent(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Number(a), Value::Number(b)) => {
            if a == b {
                return true;
            }
            // Polars may promote mixed integer/float columns to Float64. Accept
            // exact numeric promotion only; do not round large JSON integers.
            if let Some(integer) = a.as_i64() {
                return b.as_f64().is_some_and(|f| {
                    f.is_finite() && f.fract() == 0.0 && (f as i128) == i128::from(integer)
                });
            }
            if let Some(integer) = a.as_u64() {
                return b.as_f64().is_some_and(|f| {
                    f.is_finite()
                        && f >= 0.0
                        && f.fract() == 0.0
                        && (f as u128) == u128::from(integer)
                });
            }
            a.as_f64() == b.as_f64()
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| json_equivalent(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.iter()
                .all(|(key, a)| b.get(key).is_some_and(|b| json_equivalent(a, b)))
                && b.iter().all(|(key, b)| a.contains_key(key) || b.is_null())
        }
        _ => expected == actual,
    }
}

/// serde_json otherwise parses out-of-range integer literals as rounded f64.
/// This lexical guard only checks integers outside strings; serde_json remains
/// responsible for validating the complete JSON grammar and UTF-8.
fn validate_integer_literals(source: &FileSource, bytes: &[u8]) -> Result<()> {
    let mut index = 0;
    let mut quoted = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if quoted {
            match byte {
                b'\\' => {
                    index += 2;
                    continue;
                }
                b'"' => quoted = false,
                _ => {}
            }
        } else if byte == b'"' {
            quoted = true;
        } else if byte == b'-' || byte.is_ascii_digit() {
            let start = index;
            while index < bytes.len()
                && matches!(bytes[index], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
            {
                index += 1;
            }
            let token = &bytes[start..index];
            let digits = token.strip_prefix(b"-").unwrap_or(token);
            if !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) {
                let text = std::str::from_utf8(token).expect("ASCII numeric token");
                let in_range = if byte == b'-' {
                    text.parse::<i64>().is_ok()
                } else {
                    text.parse::<u64>().is_ok()
                };
                if !in_range {
                    return Err(source.parse_error(format!(
                        "JSON integer at byte {} exceeds the supported 64-bit range; encode it as a string to preserve precision", start + 1,
                    )));
                }
            }
            continue;
        }
        index += 1;
    }
    Ok(())
}

/// CSV inference can promote mixed integer/float columns to f64. Inspect the
/// original integer lexemes in batches before SQL can group/join rounded IDs.
fn validate_csv_float_integers(source: &FileSource, plan: &mut LazyFrame) -> Result<()> {
    let schema = plan
        .collect_schema()
        .map_err(|e| source.parse_error(e.to_string()))?;
    let columns: Vec<_> = schema
        .iter()
        .filter(|(_, dtype)| dtype.is_float())
        .map(|(name, _)| col(name.clone()))
        .collect();
    if columns.is_empty() {
        return Ok(());
    }
    csv_plan(source, usize::MAX, 0)
        .and_then(|lf| lf.select(columns).sink_batches(
            PlanCallback::new(|batch: DataFrame| {
                for column in batch.columns() {
                    for text in column.str()?.iter().flatten() {
                        let digits = text.trim().trim_start_matches(['-', '+']);
                        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) { continue; }
                        let exact = digits.parse::<u128>().ok().is_some_and(|n| {
                            let shift = (128u32 - n.leading_zeros()).saturating_sub(53);
                            n & ((1u128 << shift) - 1) == 0
                        });
                        polars_ensure!(exact, ComputeError: "Column {} mixes floating-point values with an integer that cannot be represented exactly; store the column as text or use an exact Parquet type", column.name());
                    }
                }
                Ok(false)
            }), false, NonZeroUsize::new(4096),
        )?.collect_with_engine(Engine::Streaming))
        .map(|_| ())
        .map_err(|e| source.parse_error(e.to_string()))
}
