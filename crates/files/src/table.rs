//! A result Kuery already holds, such as a database query's, as a frame SQL
//! can read next to the files.
//!
//! Results keep every value as the text the server sent, with a kind per
//! column. A column takes the kind's type only when every value in it
//! reads as one; otherwise it stays text, which SQL can still `CAST`.

use std::collections::HashSet;

use chrono::{DateTime, NaiveDate, NaiveDateTime};
use kuery_core::result_set::{ColumnKind, ResultSet};
use polars::prelude::*;

use crate::{ColumnSchema, Error, Result};

/// The columns SQL sees in `results` held as a table: named as [`frame`]
/// names them, with the type their kind asks for (a column whose values
/// don't all read as it stays text).
pub fn result_columns(results: &ResultSet) -> Vec<ColumnSchema> {
    let mut used = HashSet::new();
    results
        .columns()
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let kind = results.kinds().get(index).copied().unwrap_or_default();
            ColumnSchema {
                name: unique(name, &mut used),
                native_type: kind.label().into(),
                kind,
                inferred: false,
            }
        })
        .collect()
}

/// `results` as a frame, its columns typed by their kinds. Column names
/// repeated in a result (`a.id, b.id`) get a suffix: `id`, `id_2`.
pub(crate) fn frame(results: &ResultSet) -> Result<DataFrame> {
    if results.columns().is_empty() {
        return Err(Error::Sql("The result has no columns to query".into()));
    }
    let mut used = HashSet::new();
    let columns = results
        .columns()
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let name = unique(name, &mut used);
            let kind = results.kinds().get(index).copied().unwrap_or_default();
            column(&name, kind, results.column_values(index).collect())
        })
        .collect();
    DataFrame::new(results.rows(), columns).map_err(|e| Error::Sql(e.to_string()))
}

fn unique(name: &str, used: &mut HashSet<String>) -> String {
    let base = if name.is_empty() { "column" } else { name };
    let mut candidate = base.to_string();
    let mut suffix = 2;
    while !used.insert(candidate.clone()) {
        candidate = format!("{base}_{suffix}");
        suffix += 1;
    }
    candidate
}

fn column(name: &str, kind: ColumnKind, values: Vec<Option<&str>>) -> Column {
    let name: PlSmallStr = name.into();
    let typed = match kind {
        ColumnKind::Integer => all(&values, |v| v.parse::<i64>().ok())
            .map(|v| Int64Chunked::from_iter_options(name.clone(), v.into_iter()).into_series()),
        ColumnKind::Number => all(&values, |v| v.parse::<f64>().ok())
            .map(|v| Float64Chunked::from_iter_options(name.clone(), v.into_iter()).into_series()),
        ColumnKind::Boolean => all(&values, boolean)
            .map(|v| BooleanChunked::from_iter_options(name.clone(), v.into_iter()).into_series()),
        ColumnKind::Date => all(&values, date).map(|v| {
            Int32Chunked::from_iter_options(name.clone(), v.into_iter())
                .into_date()
                .into_series()
        }),
        ColumnKind::Timestamp => timestamps(&name, &values),
        ColumnKind::Text | ColumnKind::Json | ColumnKind::Id | ColumnKind::Unknown => None,
    };
    typed
        .unwrap_or_else(|| StringChunked::from_iter_options(name, values.into_iter()).into_series())
        .into_column()
}

/// Every value read with `parse`, NULLs kept; `None` if any does not read.
fn all<T>(values: &[Option<&str>], parse: impl Fn(&str) -> Option<T>) -> Option<Vec<Option<T>>> {
    values
        .iter()
        .map(|value| match value {
            None => Some(None),
            Some(text) => parse(text).map(Some),
        })
        .collect()
}

/// PostgreSQL writes `t`/`f`; MySQL and SQL Server `1`/`0`.
fn boolean(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "t" | "true" | "1" => Some(true),
        "f" | "false" | "0" => Some(false),
        _ => None,
    }
}

/// Days since the Unix epoch.
fn date(value: &str) -> Option<i32> {
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    i32::try_from((date - DateTime::UNIX_EPOCH.date_naive()).num_days()).ok()
}

/// A timestamp column: in UTC when every value carries an offset, on its
/// own clock when none does. A mix stays text, since the two can't be
/// compared.
fn timestamps(name: &PlSmallStr, values: &[Option<&str>]) -> Option<Series> {
    let parsed = all(values, timestamp)?;
    let mut offsets = parsed.iter().flatten().map(|(_, offset)| *offset);
    let zoned = offsets.next().unwrap_or(false);
    if offsets.any(|offset| offset != zoned) {
        return None;
    }
    let micros = parsed
        .into_iter()
        .map(|value| value.map(|(micros, _)| micros));
    Some(
        Int64Chunked::from_iter_options(name.clone(), micros)
            .into_datetime(TimeUnit::Microseconds, zoned.then_some(TimeZone::UTC))
            .into_series(),
    )
}

/// Microseconds since the Unix epoch (in UTC when the value has an offset),
/// and whether it had one. Reads `2026-01-01 02:23:00`, with a `T`, a
/// fraction, and an offset as PostgreSQL (`+00`), SQL Server (` +00:00`)
/// and ISO 8601 (`Z`) write it.
fn timestamp(value: &str) -> Option<(i64, bool)> {
    let value = value.trim().replacen('T', " ", 1);
    let value = match value.strip_suffix('Z') {
        Some(utc) => format!("{utc}+00:00"),
        None => value,
    };
    for format in ["%Y-%m-%d %H:%M:%S%.f%#z", "%Y-%m-%d %H:%M:%S%.f %#z"] {
        if let Ok(zoned) = DateTime::parse_from_str(&value, format) {
            return Some((zoned.timestamp_micros(), true));
        }
    }
    let wall = NaiveDateTime::parse_from_str(&value, "%Y-%m-%d %H:%M:%S%.f").ok()?;
    Some((wall.and_utc().timestamp_micros(), false))
}

#[cfg(test)]
mod tests {
    use kuery_core::result_set::BatchBuilder;

    use super::*;

    fn results(columns: &[(&str, ColumnKind)], rows: &[&[Option<&str>]]) -> ResultSet {
        let mut set = ResultSet::new(columns.iter().map(|(name, _)| name.to_string()).collect());
        set.set_kinds(columns.iter().map(|(_, kind)| *kind).collect());
        let mut builder = BatchBuilder::new(columns.len(), rows.len());
        for row in rows {
            builder.push_row(|index| row[index]);
        }
        set.push(builder.finish());
        set
    }

    #[test]
    fn columns_take_their_kinds_types() {
        let set = results(
            &[
                ("id", ColumnKind::Integer),
                ("total", ColumnKind::Number),
                ("paid", ColumnKind::Boolean),
                ("day", ColumnKind::Date),
                ("at", ColumnKind::Timestamp),
                ("note", ColumnKind::Text),
            ],
            &[
                &[
                    Some("1"),
                    Some("9.50"),
                    Some("t"),
                    Some("2026-01-31"),
                    Some("2026-01-31 10:00:00"),
                    Some("hi"),
                ],
                &[
                    None,
                    None,
                    Some("f"),
                    None,
                    Some("2026-02-01T08:30:00.5"),
                    None,
                ],
            ],
        );
        let frame = frame(&set).unwrap();
        let types: Vec<_> = frame.dtypes();
        assert_eq!(
            types,
            [
                DataType::Int64,
                DataType::Float64,
                DataType::Boolean,
                DataType::Date,
                DataType::Datetime(TimeUnit::Microseconds, None),
                DataType::String,
            ]
        );
        assert_eq!(frame.height(), 2);
        assert_eq!(frame.column("id").unwrap().null_count(), 1);
    }

    #[test]
    fn a_column_with_a_value_that_does_not_read_stays_text() {
        let set = results(
            &[("n", ColumnKind::Integer), ("at", ColumnKind::Timestamp)],
            &[
                &[Some("1"), Some("2026-01-31 10:00:00+00")],
                &[Some("99999999999999999999"), Some("2026-01-31 10:00:00")],
            ],
        );
        let frame = frame(&set).unwrap();
        assert_eq!(frame.dtypes(), [DataType::String, DataType::String]);
    }

    #[test]
    fn timestamps_with_offsets_are_in_utc() {
        let set = results(
            &[("at", ColumnKind::Timestamp)],
            &[
                &[Some("2026-01-31 10:00:00+02")],
                &[Some("2026-01-31 08:00:00.0000000 +00:00")],
                &[Some("2026-01-31T08:00:00Z")],
            ],
        );
        let frame = frame(&set).unwrap();
        assert_eq!(
            frame.dtypes(),
            [DataType::Datetime(
                TimeUnit::Microseconds,
                Some(TimeZone::UTC)
            )]
        );
        let at = frame.column("at").unwrap().as_materialized_series().clone();
        assert_eq!(at.n_unique().unwrap(), 1);
    }

    #[test]
    fn repeated_and_empty_names_get_their_own() {
        let set = results(
            &[
                ("id", ColumnKind::Integer),
                ("id", ColumnKind::Integer),
                ("", ColumnKind::Text),
            ],
            &[&[Some("1"), Some("2"), Some("x")]],
        );
        let names: Vec<_> = frame(&set)
            .unwrap()
            .get_column_names()
            .into_iter()
            .map(|name| name.to_string())
            .collect();
        assert_eq!(names, ["id", "id_2", "column"]);
    }

    #[test]
    fn an_empty_result_keeps_its_columns() {
        let mut set = ResultSet::new(vec!["id".into()]);
        set.set_kinds(vec![ColumnKind::Integer]);
        let frame = frame(&set).unwrap();
        assert_eq!((frame.height(), frame.width()), (0, 1));
        assert_eq!(frame.dtypes(), [DataType::Int64]);
    }
}
