//! The values documents hold, whichever the engine. Wider than JSON, so
//! nothing is lost on the way through Kuery: a 64-bit integer stays one, a
//! decimal keeps its digits, a date is a date, and a document keeps its
//! fields in the order the engine gave them. Types only one engine has
//! (MongoDB's ObjectId and Timestamp, DynamoDB's sets…) are an [`Ext`],
//! which keeps the engine's own lossless form.

/// One value of a document.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int32(i32),
    Int64(i64),
    Double(f64),
    /// A decimal as the engine writes it (`9.99`, `1E+3`), kept as text so
    /// no digit is lost.
    Decimal(String),
    String(String),
    /// Bytes, with the engine's subtype (4 is a UUID in BSON).
    Binary {
        subtype: u8,
        bytes: Vec<u8>,
    },
    /// Milliseconds since the Unix epoch, UTC.
    DateTime(i64),
    Array(Vec<Value>),
    Object(Document),
    Ext(Ext),
}

impl Value {
    /// The name the schema and the grid show for its type: `Int64`,
    /// `Decimal128`, `Date`, an [`Ext`]'s kind.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "Null",
            Value::Bool(_) => "Boolean",
            Value::Int32(_) => "Int32",
            Value::Int64(_) => "Int64",
            Value::Double(_) => "Double",
            Value::Decimal(_) => "Decimal128",
            Value::String(_) => "String",
            Value::Binary { subtype: 4, .. } => "UUID",
            Value::Binary { .. } => "Binary",
            Value::DateTime(_) => "Date",
            Value::Array(_) => "Array",
            Value::Object(_) => "Object",
            Value::Ext(ext) => ext.kind,
        }
    }
}

/// A document: its fields, in the order the engine gave them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Document(pub Vec<(String, Value)>);

impl Document {
    /// The value of a top-level field; `None` when the document has no such
    /// field (a field set to null is `Some(Value::Null)`).
    pub fn get(&self, field: &str) -> Option<&Value> {
        self.0
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, value)| value)
    }

    /// The value at `path`, through nested documents and array elements;
    /// `None` when something on the way is absent.
    pub fn get_path(&self, path: &FieldPath) -> Option<&Value> {
        let (first, rest) = path.0.split_first()?;
        let mut value = self.get(&first.to_string())?;
        for part in rest {
            value = match (value, part) {
                (Value::Object(document), part) => document.get(&part.to_string())?,
                (Value::Array(items), PathPart::Index(index)) => items.get(*index)?,
                _ => return None,
            };
        }
        Some(value)
    }

    /// Its identity: the `_id` field, which every engine Kuery reads by
    /// key fills in (a DynamoDB driver puts its key attributes there).
    pub fn key(&self) -> Option<DocKey> {
        self.get("_id").cloned().map(DocKey)
    }
}

/// A type only some engines have, as the engine's driver reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Ext {
    /// What the grid and the schema call it (`ObjectId`, `Timestamp`).
    pub kind: &'static str,
    /// How it reads in a cell (`ObjectId("…")`).
    pub display: String,
    /// The engine's own lossless form (canonical Extended JSON for MongoDB),
    /// which the driver reads back unchanged.
    pub raw: serde_json::Value,
}

/// One step of a [`FieldPath`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathPart {
    Field(String),
    /// An array element, or a field whose name is that number.
    Index(usize),
}

impl std::fmt::Display for PathPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathPart::Field(name) => f.write_str(name),
            PathPart::Index(index) => write!(f, "{index}"),
        }
    }
}

/// Where a value sits in a document: `a.b.0.c`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldPath(pub Vec<PathPart>);

impl FieldPath {
    /// A top-level field.
    pub fn field(name: &str) -> FieldPath {
        FieldPath(vec![PathPart::Field(name.to_string())])
    }

    /// Reads a dotted path; parts that are a number written plainly are
    /// indexes, so a field named `007` stays that field.
    pub fn parse(dotted: &str) -> FieldPath {
        FieldPath(
            dotted
                .split('.')
                .map(|part| match part.parse::<usize>() {
                    Ok(index) if index.to_string() == part => PathPart::Index(index),
                    _ => PathPart::Field(part.to_string()),
                })
                .collect(),
        )
    }

    /// The path written with dots, as engines take it.
    pub fn dotted(&self) -> String {
        self.0
            .iter()
            .map(PathPart::to_string)
            .collect::<Vec<_>>()
            .join(".")
    }

    /// The same path one step further down.
    pub fn child(&self, part: PathPart) -> FieldPath {
        let mut parts = self.0.clone();
        parts.push(part);
        FieldPath(parts)
    }
}

/// A document's identity, opaque to Kuery: MongoDB's `_id`, a DynamoDB
/// partition and sort key, a Firestore path.
#[derive(Debug, Clone, PartialEq)]
pub struct DocKey(pub Value);

/// Where a collection sits: `[database, collection]` in MongoDB, `[bucket,
/// scope, collection]` in Couchbase, `[database]` where there is one level.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CollectionRef {
    pub path: Vec<String>,
}

impl CollectionRef {
    pub fn new(path: &[&str]) -> CollectionRef {
        CollectionRef {
            path: path.iter().map(|part| part.to_string()).collect(),
        }
    }

    /// The collection's own name, the last step of its path.
    pub fn name(&self) -> &str {
        self.path.last().map_or("", String::as_str)
    }

    /// `shop.orders`, for titles and messages.
    pub fn display(&self) -> String {
        self.path.join(".")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order() -> Document {
        Document(vec![
            ("_id".into(), Value::Int32(7)),
            (
                "items".into(),
                Value::Array(vec![
                    Value::Object(Document(vec![("sku".into(), Value::String("a".into()))])),
                    Value::Object(Document(vec![("sku".into(), Value::String("b".into()))])),
                ]),
            ),
            ("coupon".into(), Value::Null),
        ])
    }

    #[test]
    fn paths_round_trip_through_dots() {
        let path = FieldPath::parse("a.b.0.c");
        assert_eq!(
            path.0,
            [
                PathPart::Field("a".into()),
                PathPart::Field("b".into()),
                PathPart::Index(0),
                PathPart::Field("c".into()),
            ]
        );
        assert_eq!(path.dotted(), "a.b.0.c");
        assert_eq!(FieldPath::parse("+1").0, [PathPart::Field("+1".into())]);
        assert_eq!(FieldPath::parse("a.007").dotted(), "a.007");
        assert_eq!(FieldPath::parse("007").0, [PathPart::Field("007".into())]);
        assert_eq!(
            FieldPath::field("a").child(PathPart::Index(2)).dotted(),
            "a.2"
        );
    }

    #[test]
    fn walks_nested_documents_and_arrays_by_index() {
        let order = order();
        assert_eq!(
            order.get_path(&FieldPath::parse("items.1.sku")),
            Some(&Value::String("b".into()))
        );
        assert_eq!(order.get_path(&FieldPath::parse("items.2.sku")), None);
        assert_eq!(order.get_path(&FieldPath::parse("items.sku")), None);
        assert_eq!(order.get("coupon"), Some(&Value::Null));
        assert_eq!(order.get("missing"), None);
        assert_eq!(order.key(), Some(DocKey(Value::Int32(7))));
        assert_eq!(Document::default().key(), None);
        // A field named like a number is found by its name.
        let numbered = Document(vec![(
            "a".into(),
            Value::Object(Document(vec![("0".into(), Value::Bool(true))])),
        )]);
        assert_eq!(
            numbered.get_path(&FieldPath::parse("a.0")),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn names_types_and_collections() {
        assert_eq!(Value::Int64(1).type_name(), "Int64");
        assert_eq!(Value::Decimal("1.5".into()).type_name(), "Decimal128");
        assert_eq!(
            Value::Binary {
                subtype: 4,
                bytes: vec![0; 16]
            }
            .type_name(),
            "UUID"
        );
        let ext = Ext {
            kind: "ObjectId",
            display: "ObjectId(\"…\")".into(),
            raw: serde_json::json!({"$oid": "65f0a1b2c3d4e5f601234567"}),
        };
        assert_eq!(Value::Ext(ext).type_name(), "ObjectId");
        let orders = CollectionRef::new(&["shop", "orders"]);
        assert_eq!(
            (orders.name(), orders.display()),
            ("orders", "shop.orders".into())
        );
    }
}
