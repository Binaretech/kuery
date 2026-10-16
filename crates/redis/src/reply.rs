//! Replies, as the console shows them: a grid for the results panel and
//! `redis-cli`-style text for the output log.

use redis::Value;

/// A command's reply, with bytes already turned into text.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Nil,
    Int(i64),
    /// Bulk and verbatim strings.
    Text(String),
    /// Simple strings such as `OK` or `PONG`.
    Status(String),
    Double(f64),
    Bool(bool),
    Array(Vec<Reply>),
    Map(Vec<(Reply, Reply)>),
    /// An error reply (`WRONGTYPE …`, `ERR …`).
    Error(String),
}

/// Rows of text for a grid; `None` is shown as NULL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

/// UTF-8 as is; anything else with `\xHH` escapes, as `redis-cli` shows it.
pub fn display_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes
            .iter()
            .map(|byte| match byte {
                b'\\' => "\\\\".to_string(),
                0x20..=0x7e => (*byte as char).to_string(),
                b'\n' => "\\n".to_string(),
                b'\r' => "\\r".to_string(),
                b'\t' => "\\t".to_string(),
                _ => format!("\\x{byte:02x}"),
            })
            .collect(),
    }
}

impl From<Value> for Reply {
    fn from(value: Value) -> Self {
        match value {
            Value::Nil => Reply::Nil,
            Value::Int(n) => Reply::Int(n),
            Value::BulkString(bytes) => Reply::Text(display_bytes(&bytes)),
            Value::SimpleString(text) => Reply::Status(text),
            Value::Okay => Reply::Status("OK".into()),
            Value::Array(items) | Value::Set(items) => {
                Reply::Array(items.into_iter().map(Reply::from).collect())
            }
            Value::Push { data, .. } => Reply::Array(data.into_iter().map(Reply::from).collect()),
            Value::Map(pairs) => Reply::Map(
                pairs
                    .into_iter()
                    .map(|(k, v)| (Reply::from(k), Reply::from(v)))
                    .collect(),
            ),
            Value::Attribute { data, .. } => Reply::from(*data),
            Value::Double(n) => Reply::Double(n),
            Value::Boolean(b) => Reply::Bool(b),
            Value::VerbatimString { text, .. } => Reply::Text(text),
            Value::ServerError(error) => Reply::Error(match error.details() {
                Some(details) => format!("{} {details}", error.code()),
                None => error.code().to_string(),
            }),
            other => Reply::Text(format!("{other:?}")),
        }
    }
}

impl Reply {
    /// The reply on one line: text as is, nested values as `[a, b]` and
    /// `{k: v}`.
    pub fn inline(&self) -> String {
        match self {
            Reply::Nil => "(nil)".into(),
            Reply::Int(n) => n.to_string(),
            Reply::Text(text) | Reply::Status(text) => text.clone(),
            Reply::Double(n) => n.to_string(),
            Reply::Bool(b) => b.to_string(),
            Reply::Error(text) => format!("(error) {text}"),
            Reply::Array(items) => format!(
                "[{}]",
                items
                    .iter()
                    .map(Reply::inline)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Reply::Map(pairs) => format!(
                "{{{}}}",
                pairs
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k.inline(), v.inline()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// A grid cell: NULL for nil, the inline text otherwise.
    fn cell(&self) -> Option<String> {
        match self {
            Reply::Nil => None,
            other => Some(other.inline()),
        }
    }

    /// The reply as a grid: one value; a list numbered from 1 (as
    /// `redis-cli` does); a map as key and value. A list of even length
    /// that alternates names and values (`HGETALL` on RESP2, `CONFIG GET`)
    /// stays a numbered list.
    pub fn table(&self) -> Table {
        match self {
            Reply::Array(items) => Table {
                columns: vec!["#".into(), "value".into()],
                rows: items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| vec![Some((i + 1).to_string()), item.cell()])
                    .collect(),
            },
            Reply::Map(pairs) => Table {
                columns: vec!["key".into(), "value".into()],
                rows: pairs
                    .iter()
                    .map(|(k, v)| vec![k.cell(), v.cell()])
                    .collect(),
            },
            other => Table {
                columns: vec!["value".into()],
                rows: vec![vec![other.cell()]],
            },
        }
    }

    /// The reply as `redis-cli` prints it.
    pub fn format_cli(&self) -> String {
        let mut out = String::new();
        self.write_cli(&mut out, 0);
        out.trim_end().to_string()
    }

    fn write_cli(&self, out: &mut String, indent: usize) {
        match self {
            Reply::Nil => out.push_str("(nil)\n"),
            Reply::Int(n) => out.push_str(&format!("(integer) {n}\n")),
            Reply::Text(text) => out.push_str(&format!("{text:?}\n")),
            Reply::Status(text) => out.push_str(&format!("{text}\n")),
            Reply::Double(n) => out.push_str(&format!("(double) {n}\n")),
            Reply::Bool(b) => out.push_str(&format!("({b})\n")),
            Reply::Error(text) => out.push_str(&format!("(error) {text}\n")),
            Reply::Array(items) if items.is_empty() => out.push_str("(empty array)\n"),
            Reply::Array(items) => {
                let width = items.len().to_string().len();
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(&" ".repeat(indent));
                    }
                    let label = format!("{:>width$}) ", i + 1);
                    out.push_str(&label);
                    item.write_cli(out, indent + label.len());
                }
            }
            Reply::Map(pairs) if pairs.is_empty() => out.push_str("(empty hash)\n"),
            Reply::Map(pairs) => {
                let width = pairs.len().to_string().len();
                for (i, (key, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push_str(&" ".repeat(indent));
                    }
                    let label = format!("{:>width$}# ", i + 1);
                    out.push_str(&label);
                    out.push_str(&format!("{} => ", key.inline()));
                    value.write_cli(out, indent + label.len() + 4);
                }
            }
        }
    }
}

/// `XRANGE` entries as rows: the id, then each field as a column, in the
/// order fields first appear. Also returns the last id, to page on.
pub(crate) fn stream_table(entries: &Value) -> (Table, Option<String>) {
    let mut columns = vec!["id".to_string()];
    let mut parsed: Vec<(String, Vec<(String, String)>)> = Vec::new();
    if let Value::Array(entries) = entries {
        for entry in entries {
            let Value::Array(parts) = entry else {
                continue;
            };
            let (Some(id), Some(fields)) = (parts.first(), parts.get(1)) else {
                continue;
            };
            let id = text_of(id);
            let pairs: Vec<(String, String)> = match fields {
                Value::Array(items) => items
                    .chunks(2)
                    .map(|pair| {
                        (
                            text_of(&pair[0]),
                            pair.get(1).map(text_of).unwrap_or_default(),
                        )
                    })
                    .collect(),
                Value::Map(items) => items
                    .iter()
                    .map(|(k, v)| (text_of(k), text_of(v)))
                    .collect(),
                _ => Vec::new(),
            };
            for (field, _) in &pairs {
                if !columns.contains(field) {
                    columns.push(field.clone());
                }
            }
            parsed.push((id, pairs));
        }
    }
    let last = parsed.last().map(|(id, _)| id.clone());
    let rows = parsed
        .into_iter()
        .map(|(id, pairs)| {
            let mut row = vec![None; columns.len()];
            row[0] = Some(id);
            for (field, value) in pairs {
                if let Some(index) = columns.iter().position(|c| *c == field) {
                    row[index] = Some(value);
                }
            }
            row
        })
        .collect();
    (Table { columns, rows }, last)
}

fn text_of(value: &Value) -> String {
    match value {
        Value::BulkString(bytes) => display_bytes(bytes),
        Value::SimpleString(text) => text.clone(),
        other => Reply::from(other.clone()).inline(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_bytes_like_redis_cli() {
        assert_eq!(display_bytes(b"caf\xc3\xa9"), "café");
        assert_eq!(display_bytes(b"\xff\x00a\n"), "\\xff\\x00a\\n");
    }

    #[test]
    fn turns_replies_into_grids() {
        let list = Reply::Array(vec![Reply::Text("a".into()), Reply::Nil]);
        let table = list.table();
        assert_eq!(table.columns, ["#", "value"]);
        assert_eq!(
            table.rows,
            [
                vec![Some("1".into()), Some("a".into())],
                vec![Some("2".into()), None]
            ]
        );
        let map = Reply::Map(vec![(Reply::Text("hits".into()), Reply::Int(3))]);
        assert_eq!(
            map.table().rows,
            [vec![Some("hits".into()), Some("3".into())]]
        );
        assert_eq!(
            Reply::Status("OK".into()).table().rows,
            [vec![Some("OK".into())]]
        );
        let nested = Reply::Array(vec![
            Reply::Text("0".into()),
            Reply::Array(vec![Reply::Text("k".into())]),
        ]);
        assert_eq!(nested.table().rows[1][1].as_deref(), Some("[k]"));
    }

    #[test]
    fn prints_like_redis_cli() {
        let reply = Reply::Array(vec![
            Reply::Text("a".into()),
            Reply::Int(2),
            Reply::Array(vec![Reply::Nil, Reply::Status("OK".into())]),
        ]);
        assert_eq!(
            reply.format_cli(),
            "1) \"a\"\n2) (integer) 2\n3) 1) (nil)\n   2) OK"
        );
        assert_eq!(Reply::Array(vec![]).format_cli(), "(empty array)");
        assert_eq!(
            Reply::Error("ERR nope".into()).format_cli(),
            "(error) ERR nope"
        );
    }

    #[test]
    fn reads_stream_entries_into_rows() {
        let entry = |id: &str, fields: &[&str]| {
            Value::Array(vec![
                Value::BulkString(id.into()),
                Value::Array(
                    fields
                        .iter()
                        .map(|f| Value::BulkString(f.as_bytes().to_vec()))
                        .collect(),
                ),
            ])
        };
        let entries = Value::Array(vec![
            entry("1-0", &["a", "1", "b", "2"]),
            entry("2-0", &["b", "3", "c", "4"]),
        ]);
        let (table, last) = stream_table(&entries);
        assert_eq!(table.columns, ["id", "a", "b", "c"]);
        assert_eq!(
            table.rows[1],
            [Some("2-0".into()), None, Some("3".into()), Some("4".into())]
        );
        assert_eq!(last.as_deref(), Some("2-0"));
    }
}
