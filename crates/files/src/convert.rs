//! Convert a typed prefix to Kuery's text Arrow batches, never the entire file.

use kuery_core::result_set::{Batch, BatchBuilder, ColumnKind, ResultSet};
use polars::prelude::*;
use serde_json::{Map, Number, Value};

use crate::{
    ColumnSchema, Error, FileFingerprint, FileFormat, FileSource, LimitReason, PreviewRequest,
    PreviewResult, PreviewScope, Result,
};

pub(crate) fn preview(
    frame: &DataFrame,
    display: Option<&DataFrame>,
    source: &FileSource,
    request: PreviewRequest,
    fingerprint: FileFingerprint,
) -> Result<PreviewResult> {
    let inferred = source.format != FileFormat::Parquet;
    let schema: Vec<_> = frame
        .columns()
        .iter()
        .map(|column| ColumnSchema {
            name: column.name().to_string(),
            native_type: column.dtype().to_string(),
            kind: kind(column.dtype()),
            inferred,
        })
        .collect();
    let mut results = ResultSet::new(schema.iter().map(|c| c.name.clone()).collect());
    results.set_kinds(schema.iter().map(|c| c.kind).collect());
    let mut reason = (frame.height() > request.row_limit).then_some(LimitReason::Rows);
    let rows_to_show = frame.height().min(request.row_limit);
    let mut offset = 0;
    while offset < rows_to_show {
        let count = (rows_to_show - offset).min(256);
        let mut rows = Vec::with_capacity(count);
        for index in offset..offset + count {
            let row: Vec<_> = display
                .unwrap_or(frame)
                .columns()
                .iter()
                .map(|column| column.get(index).and_then(cell))
                .collect::<PolarsResult<_>>()
                .map_err(|e| source.parse_error(e.to_string()))?;
            if row.iter().flatten().any(|s| s.len() > request.byte_limit) {
                return Err(Error::RowTooLarge(request.byte_limit));
            }
            rows.push(row);
        }
        let remaining = request.byte_limit - results.memory_size();
        let batch = build_batch(&rows, schema.len());
        if batch.memory_size() <= remaining {
            results.push(batch);
            offset += count;
            continue;
        }
        // Arrow counts buffer capacities and array overhead, not just UTF-8
        // bytes. Find a prefix whose actual allocation fits the remaining budget.
        let (mut low, mut high) = (0, count);
        while low < high {
            let middle = (low + high).div_ceil(2);
            if build_batch(&rows[..middle], schema.len()).memory_size() <= remaining {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        if low == 0 && results.rows() == 0 {
            return Err(Error::RowTooLarge(request.byte_limit));
        }
        if low > 0 {
            results.push(build_batch(&rows[..low], schema.len()));
        }
        reason = Some(LimitReason::Bytes);
        break;
    }
    let scope = match reason {
        None => PreviewScope::Complete,
        Some(reason) => PreviewScope::PrefixLimited {
            loaded_rows: results.rows(),
            row_limit: request.row_limit,
            reason,
        },
    };
    let mut warnings = Vec::new();
    if inferred {
        warnings.push(
            "Types inferred from at most 1,000 prefix records; records outside the preview are not validated"
                .into(),
        );
    }
    if schema
        .iter()
        .any(|c| c.kind.is_numeric() || c.kind.is_temporal())
    {
        warnings.push("Profile uses its existing f64 statistics and written timestamp hours; display values retain native precision/timezone".into());
    }
    Ok(PreviewResult {
        results,
        scope,
        schema,
        warnings,
        fingerprint,
    })
}

fn build_batch(rows: &[Vec<Option<String>>], width: usize) -> Batch {
    let mut builder = BatchBuilder::new(width, rows.len());
    for row in rows {
        builder.push_row(|index| row[index].as_deref());
    }
    builder.finish()
}

fn kind(dtype: &DataType) -> ColumnKind {
    if dtype.is_integer() {
        return ColumnKind::Integer;
    }
    if dtype.is_float() || matches!(dtype, DataType::Decimal(..)) {
        return ColumnKind::Number;
    }
    match dtype {
        DataType::Boolean => ColumnKind::Boolean,
        DataType::Date => ColumnKind::Date,
        DataType::Datetime(..) => ColumnKind::Timestamp,
        _ => ColumnKind::Text,
    }
}

fn cell(value: AnyValue<'_>) -> PolarsResult<Option<String>> {
    // Covers string-backed dictionary/enum values as well as ordinary strings.
    if let Some(text) = value.get_str() {
        return Ok(Some(text.to_owned()));
    }
    Ok(match value {
        AnyValue::Null => None,
        AnyValue::String(s) => Some(s.to_owned()),
        AnyValue::StringOwned(s) => Some(s.to_string()),
        AnyValue::Int8(v) => Some(v.to_string()),
        AnyValue::Int16(v) => Some(v.to_string()),
        AnyValue::Int32(v) => Some(v.to_string()),
        AnyValue::Int64(v) => Some(v.to_string()),
        AnyValue::Int128(v) => Some(v.to_string()),
        AnyValue::UInt8(v) => Some(v.to_string()),
        AnyValue::UInt16(v) => Some(v.to_string()),
        AnyValue::UInt32(v) => Some(v.to_string()),
        AnyValue::UInt64(v) => Some(v.to_string()),
        AnyValue::UInt128(v) => Some(v.to_string()),
        AnyValue::Decimal(value, _, scale) => Some(decimal(value, scale)),
        AnyValue::Date(days) => {
            let date = chrono::DateTime::<chrono::Utc>::UNIX_EPOCH
                .checked_add_signed(chrono::Duration::days(i64::from(days)))
                .ok_or_else(
                    || polars_err!(ComputeError: "Date is outside the supported calendar range"),
                )?;
            Some(date.date_naive().to_string())
        }
        AnyValue::Datetime(value, unit, zone) => Some(datetime(value, unit, zone)?),
        AnyValue::DatetimeOwned(value, unit, zone) => Some(datetime(value, unit, zone.as_deref())?),
        AnyValue::Time(nanos) => {
            if !(0..86_400_000_000_000).contains(&nanos) {
                polars_bail!(ComputeError: "Time is outside the supported time-of-day range");
            }
            Some(
                chrono::NaiveTime::from_num_seconds_from_midnight_opt(
                    (nanos / 1_000_000_000) as u32,
                    (nanos % 1_000_000_000) as u32,
                )
                .expect("validated time of day")
                .to_string(),
            )
        }
        // AnyValue's display formatter rounds floats for presentation. Use
        // Rust's round-trip representation so Profile receives all digits.
        AnyValue::Float32(f) => Some(f.to_string()),
        AnyValue::Float64(f) => Some(f.to_string()),
        AnyValue::Binary(bytes) => Some(hex(bytes)),
        AnyValue::BinaryOwned(bytes) => Some(hex(&bytes)),
        nested @ (AnyValue::List(_)
        | AnyValue::Array(..)
        | AnyValue::Struct(..)
        | AnyValue::StructOwned(_)) => Some(json_value(nested)?.to_string()),
        value => Some(value.to_string()),
    })
}

fn datetime(value: i64, unit: TimeUnit, zone: Option<&TimeZone>) -> PolarsResult<String> {
    use chrono::Offset as _;
    let factor = match unit {
        TimeUnit::Nanoseconds => 1_000_000_000,
        TimeUnit::Microseconds => 1_000_000,
        TimeUnit::Milliseconds => 1_000,
    };
    let utc = chrono::DateTime::from_timestamp(
        value.div_euclid(factor),
        (value.rem_euclid(factor) * (1_000_000_000 / factor)) as u32,
    )
    .ok_or_else(
        || polars_err!(ComputeError: "Timestamp is outside the supported calendar range"),
    )?;
    match zone {
        None => Ok(utc.naive_utc().to_string()),
        Some(zone) => {
            let zone: chrono_tz::Tz = zone
                .to_string()
                .parse()
                .map_err(|_| polars_err!(ComputeError: "Unsupported timestamp timezone"))?;
            let local = utc.with_timezone(&zone);
            utc.naive_utc().checked_add_signed(chrono::Duration::seconds(
                i64::from(local.offset().fix().local_minus_utc()),
            )).ok_or_else(|| polars_err!(ComputeError: "Local timestamp is outside the supported calendar range"))?;
            // An offset is understood by core's timestamp parser, whereas
            // Polars' display abbreviations (UTC/EST/etc.) are not.
            Ok(local.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false))
        }
    }
}

fn decimal(value: i128, scale: usize) -> String {
    let digits = value.unsigned_abs().to_string();
    let sign = if value < 0 { "-" } else { "" };
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    if scale >= digits.len() {
        format!("{sign}0.{}{digits}", "0".repeat(scale - digits.len()))
    } else {
        let (whole, fraction) = digits.split_at(digits.len() - scale);
        format!("{sign}{whole}.{fraction}")
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(2 + bytes.len() * 2);
    text.push_str("0x");
    for byte in bytes {
        write!(&mut text, "{byte:02x}").expect("write to String");
    }
    text
}

/// Lossless JSON primitives for validating JSON input and rendering nested data.
/// Non-JSON native scalars (dates/decimals/binary) are represented as strings.
pub(crate) fn json_value(value: AnyValue<'_>) -> PolarsResult<Value> {
    Ok(match value {
        AnyValue::Null => Value::Null,
        AnyValue::Boolean(v) => Value::Bool(v),
        AnyValue::String(v) => Value::String(v.to_owned()),
        AnyValue::StringOwned(v) => Value::String(v.to_string()),
        AnyValue::Int8(v) => Value::Number(v.into()),
        AnyValue::Int16(v) => Value::Number(v.into()),
        AnyValue::Int32(v) => Value::Number(v.into()),
        AnyValue::Int64(v) => Value::Number(v.into()),
        // Polars infers Int128 for JSON integers above i64::MAX, including
        // valid u64 literals. Keep those numeric during lossless validation.
        AnyValue::Int128(v) => {
            if let Ok(v) = i64::try_from(v) {
                Value::Number(v.into())
            } else if let Ok(v) = u64::try_from(v) {
                Value::Number(v.into())
            } else {
                Value::String(v.to_string())
            }
        }
        AnyValue::UInt8(v) => Value::Number(v.into()),
        AnyValue::UInt16(v) => Value::Number(v.into()),
        AnyValue::UInt32(v) => Value::Number(v.into()),
        AnyValue::UInt64(v) => Value::Number(v.into()),
        AnyValue::UInt128(v) => u64::try_from(v)
            .map(|v| Value::Number(v.into()))
            .unwrap_or_else(|_| Value::String(v.to_string())),
        AnyValue::Float32(v) => Number::from_f64(f64::from(v))
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(v.to_string())),
        AnyValue::Float64(v) => Number::from_f64(v)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(v.to_string())),
        AnyValue::List(series) | AnyValue::Array(series, _) => Value::Array(
            (0..series.len())
                .map(|i| series.get(i).and_then(json_value))
                .collect::<PolarsResult<_>>()?,
        ),
        value @ AnyValue::Struct(..) => return json_value(value.into_static()),
        AnyValue::StructOwned(values) => {
            let (values, fields) = *values;
            let mut object = Map::new();
            for (value, field) in values.into_iter().zip(fields) {
                object.insert(field.name().to_string(), json_value(value)?);
            }
            Value::Object(object)
        }
        value => Value::String(cell(value)?.expect("non-null scalar")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_temporal_scalars_return_errors_instead_of_panicking() {
        assert!(cell(AnyValue::Date(i32::MAX)).is_err());
        assert!(cell(AnyValue::Datetime(i64::MAX, TimeUnit::Milliseconds, None)).is_err());
        assert!(cell(AnyValue::Time(-1)).is_err());
    }

    #[test]
    fn decimal_formatting_handles_scale_and_minimum_integer() {
        assert_eq!(decimal(-123, 5), "-0.00123");
        assert_eq!(decimal(0, 2), "0.00");
        assert_eq!(decimal(i128::MIN, 0), i128::MIN.to_string());
    }
}
