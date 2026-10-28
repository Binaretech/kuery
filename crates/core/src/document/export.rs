//! Writes documents as JSON, from the documents themselves and never from
//! the grid's text, so types survive: Extended JSON keeps a 64-bit integer,
//! a decimal or a date what it is, either relaxed (plain numbers where
//! JSON has them, readable dates) or canonical (every type spelled out).
//! Newline-delimited JSON streams a whole collection one document a line;
//! a JSON array suits smaller results.

use std::io::{self, Write};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value as Json, json};

use crate::document::value::{Document, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocFormat {
    /// One document a line (`.ndjson`).
    Ndjson,
    /// One array of documents (`.json`).
    JsonArray,
}

/// Which Extended JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ejson {
    /// Plain JSON numbers and ISO dates where they lose nothing.
    Relaxed,
    /// Every type spelled out (`{"$numberLong": "5"}`).
    Canonical,
}

/// A value as Extended JSON. An engine's own types ([`Value::Ext`]) are
/// written as the engine gave them.
pub fn to_ejson(value: &Value, mode: Ejson) -> Json {
    let relaxed = mode == Ejson::Relaxed;
    match value {
        Value::Null => Json::Null,
        Value::Bool(value) => Json::Bool(*value),
        Value::Int32(n) if relaxed => json!(n),
        Value::Int32(n) => json!({"$numberInt": n.to_string()}),
        Value::Int64(n) if relaxed => json!(n),
        Value::Int64(n) => json!({"$numberLong": n.to_string()}),
        Value::Double(x) if relaxed && x.is_finite() => json!(x),
        Value::Double(x) => json!({"$numberDouble": double_text(*x)}),
        Value::Decimal(text) => json!({"$numberDecimal": text}),
        Value::String(text) => Json::String(text.clone()),
        Value::Binary { subtype, bytes } => json!({"$binary": {
            "base64": STANDARD.encode(bytes),
            "subType": format!("{subtype:02x}"),
        }}),
        Value::DateTime(millis) if relaxed && (0..=MAX_RELAXED_DATE).contains(millis) => {
            json!({"$date": iso_date(*millis)})
        }
        Value::DateTime(millis) => json!({"$date": {"$numberLong": millis.to_string()}}),
        Value::Array(items) => Json::Array(items.iter().map(|item| to_ejson(item, mode)).collect()),
        Value::Object(document) => document_ejson(document, mode),
        Value::Ext(ext) => ext.raw.clone(),
    }
}

/// The last millisecond of 9999, the last date relaxed Extended JSON writes
/// as ISO text.
const MAX_RELAXED_DATE: i64 = 253_402_300_799_999;

/// A document as an Extended JSON object, its fields in order.
pub fn document_ejson(document: &Document, mode: Ejson) -> Json {
    let fields: Map<String, Json> = document
        .0
        .iter()
        .map(|(name, value)| (name.clone(), to_ejson(value, mode)))
        .collect();
    Json::Object(fields)
}

/// `1.0`, `1e300`, `NaN`, `Infinity`, `-Infinity`: a double's text that
/// reads back as a double.
fn double_text(x: f64) -> String {
    match x {
        x if x.is_nan() => "NaN".into(),
        f64::INFINITY => "Infinity".into(),
        f64::NEG_INFINITY => "-Infinity".into(),
        x => format!("{x:?}"),
    }
}

/// A date as ISO 8601 in UTC with milliseconds: `2026-10-28T12:00:00.000Z`.
pub fn iso_date(millis: i64) -> String {
    let days = millis.div_euclid(86_400_000);
    let of_day = millis.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        of_day / 3_600_000,
        of_day / 60_000 % 60,
        of_day / 1000 % 60,
        of_day % 1000
    )
}

/// The calendar date of a day counted from 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Writes documents one at a time, for exports that stream a whole result.
/// Buffer `out`: each document is one write.
pub struct DocumentWriter<W: Write> {
    out: W,
    format: DocFormat,
    mode: Ejson,
    first: bool,
}

impl<W: Write> DocumentWriter<W> {
    pub fn new(out: W, format: DocFormat, mode: Ejson) -> Self {
        DocumentWriter {
            out,
            format,
            mode,
            first: true,
        }
    }

    pub fn write(&mut self, document: &Document) -> io::Result<()> {
        let json = serde_json::to_string(&document_ejson(document, self.mode))?;
        let lead = match (self.format, self.first) {
            (DocFormat::Ndjson, _) => "",
            (DocFormat::JsonArray, true) => "[\n  ",
            (DocFormat::JsonArray, false) => ",\n  ",
        };
        self.first = false;
        let end = match self.format {
            DocFormat::Ndjson => "\n",
            DocFormat::JsonArray => "",
        };
        self.out.write_all(format!("{lead}{json}{end}").as_bytes())
    }

    /// Ends the output (closing the array) and hands back the writer.
    pub fn finish(mut self) -> io::Result<W> {
        match (self.format, self.first) {
            (DocFormat::Ndjson, _) => {}
            (DocFormat::JsonArray, true) => self.out.write_all(b"[]\n")?,
            (DocFormat::JsonArray, false) => self.out.write_all(b"\n]\n")?,
        }
        Ok(self.out)
    }
}

/// Writes every document to `out` in `format`.
pub fn write_documents<'a>(
    documents: impl IntoIterator<Item = &'a Document>,
    format: DocFormat,
    mode: Ejson,
    out: &mut impl Write,
) -> io::Result<()> {
    let mut writer = DocumentWriter::new(out, format, mode);
    for document in documents {
        writer.write(document)?;
    }
    writer.finish().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::value::Ext;

    fn order(id: i32) -> Document {
        Document(vec![
            ("_id".into(), Value::Int32(id)),
            ("total".into(), Value::Decimal("9.99".into())),
            ("big".into(), Value::Int64(9_007_199_254_740_993)),
            ("ratio".into(), Value::Double(1.0)),
            ("at".into(), Value::DateTime(0)),
        ])
    }

    fn written(documents: &[Document], format: DocFormat, mode: Ejson) -> String {
        let mut out = Vec::new();
        write_documents(documents, format, mode, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn writes_one_document_a_line() {
        let text = written(&[order(1), order(2)], DocFormat::Ndjson, Ejson::Relaxed);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            r#"{"_id":1,"total":{"$numberDecimal":"9.99"},"big":9007199254740993,"ratio":1.0,"at":{"$date":"1970-01-01T00:00:00.000Z"}}"#
        );
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn canonical_spells_every_type_and_reads_back() {
        let text = written(&[order(1)], DocFormat::Ndjson, Ejson::Canonical);
        let back: Json = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(back["_id"], json!({"$numberInt": "1"}));
        assert_eq!(back["big"], json!({"$numberLong": "9007199254740993"}));
        assert_eq!(back["total"], json!({"$numberDecimal": "9.99"}));
        assert_eq!(back["ratio"], json!({"$numberDouble": "1.0"}));
        assert_eq!(back["at"], json!({"$date": {"$numberLong": "0"}}));
    }

    #[test]
    fn dates_outside_the_iso_years_are_numbers() {
        let date = |millis| to_ejson(&Value::DateTime(millis), Ejson::Relaxed);
        assert_eq!(date(-1), json!({"$date": {"$numberLong": "-1"}}));
        assert_eq!(
            date(1_792_324_800_123),
            json!({"$date": "2026-10-18T12:00:00.123Z"})
        );
        assert_eq!(
            date(MAX_RELAXED_DATE),
            json!({"$date": "9999-12-31T23:59:59.999Z"})
        );
        assert_eq!(
            date(MAX_RELAXED_DATE + 1),
            json!({"$date": {"$numberLong": "253402300800000"}})
        );
        assert_eq!(iso_date(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(iso_date(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn keeps_binary_doubles_and_engine_types() {
        let uuid = Value::Binary {
            subtype: 4,
            bytes: vec![0xff; 4],
        };
        assert_eq!(
            to_ejson(&uuid, Ejson::Relaxed),
            json!({"$binary": {"base64": "/////w==", "subType": "04"}})
        );
        assert_eq!(
            to_ejson(&Value::Double(f64::NAN), Ejson::Relaxed),
            json!({"$numberDouble": "NaN"})
        );
        assert_eq!(
            to_ejson(&Value::Double(f64::NEG_INFINITY), Ejson::Canonical),
            json!({"$numberDouble": "-Infinity"})
        );
        let oid = json!({"$oid": "65f0a1b2c3d4e5f601234567"});
        let ext = Value::Ext(Ext {
            kind: "ObjectId",
            display: String::new(),
            raw: oid.clone(),
        });
        assert_eq!(to_ejson(&ext, Ejson::Relaxed), oid);
    }

    #[test]
    fn writes_arrays_even_empty_ones() {
        assert_eq!(written(&[], DocFormat::JsonArray, Ejson::Relaxed), "[]\n");
        assert_eq!(written(&[], DocFormat::Ndjson, Ejson::Relaxed), "");
        let one = Document(vec![("a".into(), Value::Array(vec![Value::Null]))]);
        assert_eq!(
            written(&[one.clone(), one], DocFormat::JsonArray, Ejson::Relaxed),
            "[\n  {\"a\":[null]},\n  {\"a\":[null]}\n]\n"
        );
    }
}
