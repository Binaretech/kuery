//! A collection's shape, read from a sample of its documents: which fields
//! they have and how often, and which types those fields hold. Engines that
//! declare a schema (a validator) say what documents must follow; this
//! says what they actually hold, which is what completion, the grid's
//! columns and the editor's templates go by.

use std::collections::{HashMap, HashSet};

use crate::document::adapter::DocumentSink;
use crate::document::capabilities::Sampling;
use crate::document::value::{Document, Value};
use crate::session::QueryError;

/// Paths deeper than this are left out: `a.b.c.d` is read, `a.b.c.d.e` not.
pub const MAX_DEPTH: usize = 4;

/// A sample stops reading once its documents hold about this much, however
/// few they are: a collection of big documents (a GridFS `fs.chunks`, of
/// 255 kB each) is sampled from fewer of them.
pub const SAMPLE_BYTES: usize = 16 * 1024 * 1024;

/// What a sample of documents holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    /// Documents in the sample.
    pub documents: usize,
    /// Every path, in the order it first appeared.
    pub fields: Vec<FieldSchema>,
}

impl Schema {
    /// The fields documents hold at their top level, in the order they
    /// first appeared.
    pub fn top_level(&self) -> impl Iterator<Item = &FieldSchema> {
        self.fields.iter().filter(|field| field.key().is_some())
    }

    /// The paths a filter names fields by, without the array markers:
    /// `items.sku` for `items[].sku`. The elements of an array have none of
    /// their own.
    pub fn query_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = Vec::new();
        for field in &self.fields {
            if let Some(path) = field.query_path()
                && !paths.contains(&path)
            {
                paths.push(path);
            }
        }
        paths
    }
}

/// A schema and how its documents were picked.
#[derive(Debug, Clone, PartialEq)]
pub struct Sampled {
    pub schema: Schema,
    pub sampling: Sampling,
    /// Whether the sample stopped at [`SAMPLE_BYTES`] before it had the
    /// documents it asked for.
    pub capped: bool,
}

impl Sampled {
    /// `Sampled from 1,000 documents`, or `First 1,000 documents` when they
    /// were not picked at random; the documents the sample holds, which
    /// are fewer than it asked for when they were big.
    pub fn label(&self) -> String {
        let documents = match self.schema.documents {
            1 => "1 document".to_string(),
            n => format!("{} documents", thousands(n)),
        };
        let label = match self.sampling {
            Sampling::Random => format!("Sampled from {documents}"),
            Sampling::FirstN => format!("First {documents}"),
        };
        match self.capped {
            true => format!("{label}, as many as fit in {} MB", SAMPLE_BYTES >> 20),
            false => label,
        }
    }
}

/// Collects a sample's documents until they hold about [`SAMPLE_BYTES`].
#[derive(Debug, Default)]
pub struct SampleSink {
    pub documents: Vec<Document>,
    bytes: usize,
}

impl SampleSink {
    /// Whether it stopped for their size.
    pub fn capped(&self) -> bool {
        self.bytes >= SAMPLE_BYTES
    }
}

impl DocumentSink for SampleSink {
    fn documents(&mut self, batch: Vec<Document>) -> Result<(), QueryError> {
        for document in batch {
            if self.capped() {
                break;
            }
            self.bytes += document_size(&document);
            self.documents.push(document);
        }
        Ok(())
    }

    fn full(&self) -> bool {
        self.capped()
    }
}

/// About the bytes a document takes, as BSON stores it.
fn document_size(document: &Document) -> usize {
    5 + document
        .0
        .iter()
        .map(|(name, value)| name.len() + 2 + size(value))
        .sum::<usize>()
}

fn size(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) => 1,
        Value::Int32(_) => 4,
        Value::Int64(_) | Value::Double(_) | Value::DateTime(_) => 8,
        Value::Decimal(_) => 16,
        Value::String(text) => text.len() + 5,
        Value::Binary { bytes, .. } => bytes.len() + 5,
        Value::Array(items) => 5 + items.iter().map(|item| size(item) + 2).sum::<usize>(),
        Value::Object(document) => document_size(document),
        Value::Ext(ext) => ext.display.len(),
    }
}

/// `1234` → `1,234`.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// One step of a path: a field by its name, or the elements of an array.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Part {
    Field(String),
    Elements,
}

/// One path of a [`Schema`].
#[derive(Debug, Clone, PartialEq)]
pub struct FieldSchema {
    /// Dotted, with `[]` for the elements of an array: `items[].sku`. A
    /// name with a dot or a bracket of its own is quoted, so a top-level
    /// `"a.b"` is not the `b` inside `a`.
    pub path: String,
    /// The steps the path takes.
    pub parts: Vec<Part>,
    /// Sampled documents that have it.
    pub present: usize,
    /// How many values of each type it holds, the commonest first.
    pub types: Vec<(&'static str, usize)>,
}

impl FieldSchema {
    /// The share of `documents` that have it, from 0 to 1.
    pub fn presence(&self, documents: usize) -> f64 {
        match documents {
            0 => 0.0,
            documents => self.present as f64 / documents as f64,
        }
    }

    /// The path as a filter names it: `items.sku` for `items[].sku`; `None`
    /// for the elements of an array themselves (`items[]`), and for a path
    /// through a name with a dot of its own, which a dotted path cannot
    /// name.
    pub fn query_path(&self) -> Option<String> {
        if self.parts.last() == Some(&Part::Elements) {
            return None;
        }
        let mut names = Vec::new();
        for part in &self.parts {
            match part {
                Part::Field(name) if name.contains('.') => return None,
                Part::Field(name) => names.push(name.as_str()),
                Part::Elements => {}
            }
        }
        Some(names.join("."))
    }

    /// The field's name, when it lies at the top level of the documents.
    pub fn key(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [Part::Field(name)] => Some(name),
            _ => None,
        }
    }

    /// How deep it lies: 0 for a top-level field, 1 for `ship.city` and
    /// for `items[]`.
    pub fn depth(&self) -> usize {
        self.parts.len().saturating_sub(1)
    }

    /// Whether it holds more than one type, nulls aside.
    pub fn mixed(&self) -> bool {
        self.types
            .iter()
            .filter(|(name, _)| *name != "Null")
            .count()
            > 1
    }

    /// `Double`, or `Double 92 %, String 8 %` when it holds several types.
    pub fn label(&self) -> String {
        if let [(name, _)] = self.types.as_slice() {
            return name.to_string();
        }
        let total: usize = self.types.iter().map(|(_, count)| count).sum();
        self.types
            .iter()
            .map(|(name, count)| format!("{name} {} %", (count * 100 + total / 2) / total))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The schema of `documents`.
pub fn infer(documents: &[Document]) -> Schema {
    let mut inference = Inference::default();
    for document in documents {
        let mut seen = HashSet::new();
        for (name, value) in &document.0 {
            let part = Part::Field(name.clone());
            inference.walk(vec![part], value, &mut seen);
        }
        for index in seen {
            inference.fields[index].present += 1;
        }
    }
    let mut fields = inference.fields;
    for field in &mut fields {
        // Stable: equal counts keep their first appearance.
        field
            .types
            .sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    }
    Schema {
        documents: documents.len(),
        fields,
    }
}

#[derive(Default)]
struct Inference {
    fields: Vec<FieldSchema>,
    by_path: HashMap<Vec<Part>, usize>,
}

impl Inference {
    /// Counts `value` at `path` and walks what it holds; `seen` gathers the
    /// paths the current document has.
    fn walk(&mut self, path: Vec<Part>, value: &Value, seen: &mut HashSet<usize>) {
        let index = match self.by_path.get(&path) {
            Some(index) => *index,
            None => {
                self.fields.push(FieldSchema {
                    path: written(&path),
                    parts: path.clone(),
                    present: 0,
                    types: Vec::new(),
                });
                self.by_path.insert(path.clone(), self.fields.len() - 1);
                self.fields.len() - 1
            }
        };
        seen.insert(index);
        let name = value.type_name();
        let types = &mut self.fields[index].types;
        match types.iter_mut().find(|(seen, _)| *seen == name) {
            Some((_, count)) => *count += 1,
            None => types.push((name, 1)),
        }
        if path.len() >= MAX_DEPTH {
            return;
        }
        let deeper = |part: Part| {
            let mut deeper = path.clone();
            deeper.push(part);
            deeper
        };
        match value {
            Value::Object(document) => {
                for (name, value) in &document.0 {
                    self.walk(deeper(Part::Field(name.clone())), value, seen);
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.walk(deeper(Part::Elements), item, seen);
                }
            }
            _ => {}
        }
    }
}

/// `items[].sku`; `"a.b"` for a name that holds a dot or a bracket.
fn written(path: &[Part]) -> String {
    let mut out = String::new();
    for part in path {
        match part {
            Part::Elements => out.push_str("[]"),
            Part::Field(name) => {
                if !out.is_empty() {
                    out.push('.');
                }
                match name.contains(['.', '[', ']', '"']) {
                    true => out.push_str(&format!("{name:?}")),
                    false => out.push_str(name),
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(fields: Vec<(&str, Value)>) -> Document {
        Document(
            fields
                .into_iter()
                .map(|(name, value)| (name.to_string(), value))
                .collect(),
        )
    }

    fn field<'a>(schema: &'a Schema, path: &str) -> &'a FieldSchema {
        schema
            .fields
            .iter()
            .find(|field| field.path == path)
            .unwrap_or_else(|| panic!("no {path} in {schema:?}"))
    }

    #[test]
    fn counts_presence_and_types_by_path() {
        let item = |sku: &str| Value::Object(doc(vec![("sku", Value::String(sku.into()))]));
        let documents: Vec<Document> = (0..10)
            .map(|i| {
                let price = match i {
                    0 => Value::String("9.99".into()),
                    _ => Value::Double(9.99),
                };
                let mut fields = vec![("_id", Value::Int32(i)), ("price", price)];
                if i < 4 {
                    fields.push(("items", Value::Array(vec![item("a"), item("b")])));
                }
                doc(fields)
            })
            .collect();
        let schema = infer(&documents);
        assert_eq!(schema.documents, 10);
        let paths: Vec<&str> = schema.fields.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["_id", "price", "items", "items[]", "items[].sku"]);

        let price = field(&schema, "price");
        assert!(price.mixed());
        assert_eq!(price.label(), "Double 90 %, String 10 %");
        assert_eq!(field(&schema, "_id").label(), "Int32");

        let items = field(&schema, "items");
        assert_eq!(items.presence(schema.documents), 0.4);
        // Filters and completion name the paths without array markers.
        assert_eq!(schema.query_paths(), ["_id", "price", "items", "items.sku"]);
        let top: Vec<&str> = schema.top_level().map(|f| f.path.as_str()).collect();
        assert_eq!(top, ["_id", "price", "items"]);
        let depths: Vec<usize> = schema.fields.iter().map(FieldSchema::depth).collect();
        assert_eq!(depths, [0, 0, 0, 1, 2]);
        // Two elements a document, counted once a document for presence.
        let skus = field(&schema, "items[].sku");
        assert_eq!((skus.present, skus.types.clone()), (4, vec![("String", 8)]));
    }

    #[test]
    fn nulls_do_not_make_a_field_mixed_and_depth_stops_at_four() {
        let documents = vec![
            doc(vec![("coupon", Value::Null)]),
            doc(vec![("coupon", Value::String("X".into()))]),
        ];
        assert!(!field(&infer(&documents), "coupon").mixed());

        let mut deep = Value::Int32(1);
        for name in ["e", "d", "c", "b"] {
            deep = Value::Object(doc(vec![(name, deep)]));
        }
        let schema = infer(&[doc(vec![("a", deep)])]);
        assert!(schema.fields.iter().any(|f| f.path == "a.b.c.d"));
        assert!(!schema.fields.iter().any(|f| f.path == "a.b.c.d.e"));
        assert_eq!(infer(&[]).fields, []);
    }

    #[test]
    fn a_name_with_a_dot_is_not_a_nested_path() {
        let documents = vec![
            doc(vec![("a.b", Value::Int32(1)), ("x", Value::Int32(1))]),
            doc(vec![(
                "a",
                Value::Object(doc(vec![("b", Value::String("s".into()))])),
            )]),
        ];
        let schema = infer(&documents);
        let paths: Vec<&str> = schema.fields.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["\"a.b\"", "x", "a", "a.b"]);
        assert!(!field(&schema, "a.b").mixed());
        assert!(!field(&schema, "\"a.b\"").mixed());
        assert_eq!(field(&schema, "\"a.b\"").depth(), 0);
        // The insert template keeps it, by its own name.
        let top: Vec<Option<&str>> = schema.top_level().map(FieldSchema::key).collect();
        assert_eq!(top, [Some("a.b"), Some("x"), Some("a")]);
        // A dotted path names only the nested one.
        assert_eq!(schema.query_paths(), ["x", "a", "a.b"]);
    }

    #[test]
    fn says_how_the_documents_were_picked() {
        let sampled = |n: i32, sampling| Sampled {
            schema: infer(
                &(0..n)
                    .map(|i| doc(vec![("a", Value::Int32(i))]))
                    .collect::<Vec<_>>(),
            ),
            sampling,
            capped: false,
        };
        assert_eq!(
            sampled(1000, Sampling::Random).label(),
            "Sampled from 1,000 documents"
        );
        assert_eq!(sampled(20, Sampling::FirstN).label(), "First 20 documents");
        assert_eq!(
            sampled(1, Sampling::Random).label(),
            "Sampled from 1 document"
        );
        let capped = Sampled {
            capped: true,
            ..sampled(64, Sampling::Random)
        };
        assert_eq!(
            capped.label(),
            "Sampled from 64 documents, as many as fit in 16 MB"
        );
    }

    #[test]
    fn a_sample_of_big_documents_stops_at_its_size() {
        // As a GridFS chunk: 255 kB of data.
        let chunk = || {
            doc(vec![
                ("n", Value::Int32(0)),
                (
                    "data",
                    Value::Binary {
                        subtype: 0,
                        bytes: vec![0; 255 * 1024],
                    },
                ),
            ])
        };
        let mut sink = SampleSink::default();
        sink.documents(vec![doc(vec![("a", Value::Int32(1))]); 10])
            .unwrap();
        assert!(!sink.full());
        while !sink.full() {
            sink.documents((0..20).map(|_| chunk()).collect()).unwrap();
        }
        // Stopped at 16 MB, within one document.
        assert!(sink.capped());
        assert_eq!(sink.documents.len(), 10 + 65);
        // What comes after that is dropped.
        sink.documents(vec![chunk()]).unwrap();
        assert_eq!(sink.documents.len(), 75);
    }
}
