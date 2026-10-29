//! Writes a [`ResultSet`] as CSV, TSV or JSON.
//!
//! Values are written as the text PostgreSQL sent; NULL is an empty field in
//! CSV and TSV and `null` in JSON.

use std::io::{self, Write};
use std::path::Path;

use crate::result_set::ResultSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// RFC 4180: comma-separated, fields quoted when they need it.
    Csv,
    /// Tab-separated, with `\t`, `\n`, `\r` and `\\` escaped inside values.
    Tsv,
    /// An array with one object per row, keyed by column name.
    Json,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Csv, Format::Tsv, Format::Json];

    /// The format a file name asks for; CSV when the extension is not known.
    pub fn from_path(path: &Path) -> Self {
        match path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .as_deref()
        {
            Some("tsv" | "tab") => Format::Tsv,
            Some("json") => Format::Json,
            _ => Format::Csv,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Json => "json",
        }
    }
}

/// Writes every row of `set` to `out` in `format`. Buffer `out`: rows are
/// written field by field.
pub fn write(set: &ResultSet, format: Format, out: &mut impl Write) -> io::Result<()> {
    write_lacking(set, format, &|_, _| false, out)
}

/// [`write`] for a grid of documents, whose empty cells are either a null or
/// a field the document lacks: `absent(row, column)` says which, and JSON
/// leaves those fields out of the row's object. CSV and TSV have nothing to
/// tell them apart with, and write both empty.
pub fn write_lacking(
    set: &ResultSet,
    format: Format,
    absent: &dyn Fn(usize, usize) -> bool,
    out: &mut impl Write,
) -> io::Result<()> {
    match format {
        Format::Csv => write_separated(set, out, ',', csv_field),
        Format::Tsv => write_separated(set, out, '\t', tsv_field),
        Format::Json => write_json(set, absent, out),
    }
}

fn write_separated(
    set: &ResultSet,
    out: &mut impl Write,
    separator: char,
    field: fn(&str) -> std::borrow::Cow<'_, str>,
) -> io::Result<()> {
    let mut line = String::new();
    let mut write_line = |values: &mut dyn Iterator<Item = Option<&str>>| {
        line.clear();
        for (index, value) in values.enumerate() {
            if index > 0 {
                line.push(separator);
            }
            if let Some(value) = value {
                line.push_str(&field(value));
            }
        }
        // A lone empty field would be a blank line, which many readers skip.
        if line.is_empty() {
            line.push_str("\"\"");
        }
        line.push('\n');
        out.write_all(line.as_bytes())
    };
    write_line(&mut set.columns().iter().map(|c| Some(c.as_str())))?;
    set.try_for_each_row(|row| write_line(&mut row.iter().copied()))
}

/// Quoted, with quotes doubled, when it holds a separator, a quote, a line
/// break or leading/trailing spaces.
fn csv_field(value: &str) -> std::borrow::Cow<'_, str> {
    let needs_quotes =
        value.contains([',', '"', '\n', '\r']) || value.starts_with(' ') || value.ends_with(' ');
    if needs_quotes {
        format!("\"{}\"", value.replace('"', "\"\"")).into()
    } else {
        value.into()
    }
}

fn tsv_field(value: &str) -> std::borrow::Cow<'_, str> {
    if value.contains(['\t', '\n', '\r', '\\']) {
        value
            .replace('\\', "\\\\")
            .replace('\t', "\\t")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .into()
    } else {
        value.into()
    }
}

fn write_json(
    set: &ResultSet,
    absent: &dyn Fn(usize, usize) -> bool,
    out: &mut impl Write,
) -> io::Result<()> {
    let keys: Vec<String> = set.columns().iter().map(|c| json_string(c)).collect();
    let mut first = true;
    let mut line = String::new();
    let mut row_index = 0;
    out.write_all(b"[")?;
    set.try_for_each_row(|row| {
        line.clear();
        line.push_str(if first { "\n  {" } else { ",\n  {" });
        first = false;
        let mut written = 0;
        for (index, (key, value)) in keys.iter().zip(row).enumerate() {
            if value.is_none() && absent(row_index, index) {
                continue;
            }
            if written > 0 {
                line.push_str(", ");
            }
            written += 1;
            line.push_str(key);
            line.push_str(": ");
            match value {
                Some(value) => line.push_str(&json_string(value)),
                None => line.push_str("null"),
            }
        }
        line.push('}');
        row_index += 1;
        out.write_all(line.as_bytes())
    })?;
    out.write_all(if first { b"]\n" } else { b"\n]\n" })
}

/// A JSON string literal for `text`.
pub fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result_set::BatchBuilder;

    fn sample() -> ResultSet {
        let mut set = ResultSet::new(vec!["id".into(), "note".into()]);
        let rows: [[Option<&str>; 2]; 3] = [
            [Some("1"), Some("plain")],
            [Some("2"), Some("a, \"quoted\"\nline\twith tab")],
            [Some("3"), None],
        ];
        // Two batches, to check rows are written across them in order.
        for chunk in rows.chunks(2) {
            let mut builder = BatchBuilder::new(2, chunk.len());
            for row in chunk {
                builder.push_row(|index| row[index]);
            }
            set.push(builder.finish());
        }
        set
    }

    fn render(format: Format) -> String {
        let mut out = Vec::new();
        write(&sample(), format, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn writes_csv_with_quoting() {
        assert_eq!(
            render(Format::Csv),
            "id,note\n1,plain\n2,\"a, \"\"quoted\"\"\nline\twith tab\"\n3,\n"
        );
    }

    #[test]
    fn writes_tsv_with_escapes() {
        assert_eq!(
            render(Format::Tsv),
            "id\tnote\n1\tplain\n2\ta, \"quoted\"\\nline\\twith tab\n3\t\n"
        );
    }

    #[test]
    fn a_single_column_keeps_its_null_and_empty_rows() {
        let mut set = ResultSet::new(vec!["note".into()]);
        let rows = [Some("a"), None, Some("")];
        let mut builder = BatchBuilder::new(1, rows.len());
        for row in rows {
            builder.push_row(|_| row);
        }
        set.push(builder.finish());
        for format in [Format::Csv, Format::Tsv] {
            let mut out = Vec::new();
            write(&set, format, &mut out).unwrap();
            assert_eq!(out, b"note\na\n\"\"\n\"\"\n", "{format:?}");
        }
    }

    #[test]
    fn writes_json_objects() {
        assert_eq!(
            render(Format::Json),
            "[\n  {\"id\": \"1\", \"note\": \"plain\"},\n  \
             {\"id\": \"2\", \"note\": \"a, \\\"quoted\\\"\\nline\\twith tab\"},\n  \
             {\"id\": \"3\", \"note\": null}\n]\n"
        );
        let mut out = Vec::new();
        write(&ResultSet::new(vec!["x".into()]), Format::Json, &mut out).unwrap();
        assert_eq!(out, b"[]\n");
    }

    #[test]
    fn leaves_out_the_fields_a_document_lacks() {
        // Row 3's note is absent, not null; row 1's id is never empty.
        let absent = |row: usize, column: usize| (row, column) == (2, 1) || row == 0;
        let mut out = Vec::new();
        write_lacking(&sample(), Format::Json, &absent, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "[\n  {\"id\": \"1\", \"note\": \"plain\"},\n  \
             {\"id\": \"2\", \"note\": \"a, \\\"quoted\\\"\\nline\\twith tab\"},\n  \
             {\"id\": \"3\"}\n]\n"
        );
        let mut out = Vec::new();
        write_lacking(&sample(), Format::Csv, &absent, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), render(Format::Csv));
    }

    #[test]
    fn picks_the_format_from_the_extension() {
        assert_eq!(Format::from_path(Path::new("a/b.JSON")), Format::Json);
        assert_eq!(Format::from_path(Path::new("b.tsv")), Format::Tsv);
        assert_eq!(Format::from_path(Path::new("b.csv")), Format::Csv);
        assert_eq!(Format::from_path(Path::new("b")), Format::Csv);
        assert_eq!(json_string("\u{1}"), r#""\u0001""#);
    }
}
