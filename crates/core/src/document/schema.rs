//! A collection's shape, read from a sample of its documents: which fields
//! they have and how often, and which types those fields hold. Engines that
//! declare a schema (a validator) say what documents must follow; this
//! says what they actually hold, which is what completion, the grid's
//! columns and the editor's templates go by.

use std::collections::{HashMap, HashSet};

use crate::document::value::{Document, Value};

/// Paths deeper than this are left out: `a.b.c.d` is read, `a.b.c.d.e` not.
pub const MAX_DEPTH: usize = 4;

/// What a sample of documents holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    /// Documents in the sample.
    pub documents: usize,
    /// Every path, in the order it first appeared.
    pub fields: Vec<FieldSchema>,
}

/// One path of a [`Schema`].
#[derive(Debug, Clone, PartialEq)]
pub struct FieldSchema {
    /// Dotted, with `[]` for the elements of an array: `items[].sku`.
    pub path: String,
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
            inference.walk(name.clone(), value, 1, &mut seen);
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
    by_path: HashMap<String, usize>,
}

impl Inference {
    /// Counts `value` at `path` and walks what it holds; `seen` gathers the
    /// paths the current document has.
    fn walk(&mut self, path: String, value: &Value, depth: usize, seen: &mut HashSet<usize>) {
        let index = match self.by_path.get(&path) {
            Some(index) => *index,
            None => {
                self.fields.push(FieldSchema {
                    path: path.clone(),
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
        if depth >= MAX_DEPTH {
            return;
        }
        match value {
            Value::Object(document) => {
                for (name, value) in &document.0 {
                    self.walk(format!("{path}.{name}"), value, depth + 1, seen);
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.walk(format!("{path}[]"), item, depth + 1, seen);
                }
            }
            _ => {}
        }
    }
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
}
