//! A result looked at more closely than the grid shows it: one value whole
//! (JSON indented), and the totals of one column. Plain computation over a
//! [`ResultSet`], like [`crate::stats`]; [`totals`] is meant to run off the
//! UI thread.

use crate::result_set::{ColumnKind, ResultSet};
use crate::stats::{self, parse_number};

/// Values read to infer a column's kind when the server did not report it.
const KIND_SAMPLE: usize = 1_000;

/// A value as the inspector shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Inspected {
    /// The value, indented when it is JSON.
    pub text: String,
    pub json: bool,
    /// Of the value as stored, not as indented.
    pub chars: usize,
    pub lines: usize,
}

/// `value` whole: a JSON object or array indented, anything else as it is.
pub fn inspect(value: &str) -> Inspected {
    let trimmed = value.trim();
    let json = (trimmed.starts_with('{') || trimmed.starts_with('['))
        .then(|| serde_json::from_str::<serde_json::Value>(trimmed).ok())
        .flatten()
        .and_then(|parsed| serde_json::to_string_pretty(&parsed).ok());
    Inspected {
        json: json.is_some(),
        text: json.unwrap_or_else(|| value.to_string()),
        chars: value.chars().count(),
        lines: value.lines().count().max(1),
    }
}

/// What a column of a result adds up to.
#[derive(Debug, Clone, PartialEq)]
pub struct Totals {
    /// Values that are not NULL.
    pub values: usize,
    pub nulls: usize,
    /// For a numeric column with at least one number.
    pub numbers: Option<Numbers>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Numbers {
    pub count: usize,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}

impl Numbers {
    pub fn mean(&self) -> f64 {
        self.sum / self.count as f64
    }
}

/// The kind of `column`: as the server reported it, else inferred from the
/// first values.
pub fn column_kind(set: &ResultSet, column: usize) -> ColumnKind {
    match set.kinds().get(column).copied().unwrap_or_default() {
        ColumnKind::Unknown => {
            stats::infer_kind(set.column_values(column).flatten().take(KIND_SAMPLE))
        }
        kind => kind,
    }
}

/// The totals of `column` over every row `set` holds.
pub fn totals(set: &ResultSet, column: usize) -> Totals {
    let numeric = column_kind(set, column).is_numeric();
    let (mut values, mut nulls) = (0, 0);
    let mut numbers: Option<Numbers> = None;
    for value in set.column_values(column) {
        let Some(value) = value else {
            nulls += 1;
            continue;
        };
        values += 1;
        if let Some(n) = numeric.then(|| parse_number(value)).flatten() {
            let totals = numbers.get_or_insert(Numbers {
                count: 0,
                sum: 0.0,
                min: n,
                max: n,
            });
            totals.count += 1;
            totals.sum += n;
            totals.min = totals.min.min(n);
            totals.max = totals.max.max(n);
        }
    }
    Totals {
        values,
        nulls,
        numbers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result_set::BatchBuilder;

    fn set(kind: ColumnKind, values: &[Option<&str>]) -> ResultSet {
        let mut set = ResultSet::new(vec!["c".into()]);
        set.set_kinds(vec![kind]);
        let mut builder = BatchBuilder::new(1, values.len());
        for value in values {
            builder.push_row(|_| *value);
        }
        set.push(builder.finish());
        set
    }

    #[test]
    fn indents_json_and_leaves_the_rest_alone() {
        let object = inspect(r#"{"b":1,"a":[true,null]}"#);
        assert!(object.json);
        assert_eq!(
            object.text,
            "{\n  \"b\": 1,\n  \"a\": [\n    true,\n    null\n  ]\n}"
        );
        assert_eq!((object.chars, object.lines), (23, 1));

        let text = inspect("{not json}\nsecond line");
        assert!(!text.json);
        assert_eq!(text.text, "{not json}\nsecond line");
        assert_eq!(text.lines, 2);
        assert_eq!(inspect("crème").chars, 5);
        assert!(!inspect("42").json);
    }

    #[test]
    fn adds_up_a_numeric_column() {
        let totals = totals(
            &set(
                ColumnKind::Number,
                &[Some("1.5"), None, Some("-2"), Some("10"), None],
            ),
            0,
        );
        assert_eq!((totals.values, totals.nulls), (3, 2));
        let numbers = totals.numbers.unwrap();
        assert_eq!(
            (numbers.count, numbers.sum, numbers.min, numbers.max),
            (3, 9.5, -2.0, 10.0)
        );
        assert!((numbers.mean() - 9.5 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn counts_text_and_infers_unreported_kinds() {
        let text = totals(&set(ColumnKind::Text, &[Some("12"), Some("x"), None]), 0);
        assert_eq!((text.values, text.nulls, text.numbers), (2, 1, None));

        let inferred = totals(&set(ColumnKind::Unknown, &[Some("3"), Some("4")]), 0);
        assert_eq!(inferred.numbers.map(|n| n.sum), Some(7.0));

        let empty = totals(&set(ColumnKind::Integer, &[None]), 0);
        assert_eq!((empty.values, empty.nulls, empty.numbers), (0, 1, None));
    }
}
