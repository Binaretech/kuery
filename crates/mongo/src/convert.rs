//! BSON to Kuery's document values and back, losing nothing: an Int64
//! stays one, a Decimal128 keeps its digits, and the types only MongoDB has
//! (ObjectId, Timestamp, regular expressions, MinKey…) travel as an
//! [`Ext`] holding their canonical Extended JSON, which reads back into the
//! same BSON.

use bson::spec::BinarySubtype;
use bson::{Binary, Bson, DateTime, Decimal128};
use kuery_core::document::value::{Document, Ext, Value};

use crate::literal;

/// A BSON value as a document value.
pub fn value(bson: &Bson) -> Value {
    match bson {
        Bson::Null => Value::Null,
        Bson::Boolean(value) => Value::Bool(*value),
        Bson::Int32(n) => Value::Int32(*n),
        Bson::Int64(n) => Value::Int64(*n),
        Bson::Double(x) => Value::Double(*x),
        Bson::Decimal128(decimal) => Value::Decimal(decimal.to_string()),
        Bson::String(text) => Value::String(text.clone()),
        Bson::Binary(binary) => Value::Binary {
            subtype: binary.subtype.into(),
            bytes: binary.bytes.clone(),
        },
        Bson::DateTime(date) => Value::DateTime(date.timestamp_millis()),
        Bson::Array(items) => Value::Array(items.iter().map(value).collect()),
        Bson::Document(fields) => Value::Object(document(fields)),
        other => Value::Ext(Ext {
            kind: kind(other),
            display: literal::ext_display(other),
            raw: other.clone().into_canonical_extjson(),
        }),
    }
}

/// A BSON document as a document, its fields in order.
pub fn document(fields: &bson::Document) -> Document {
    Document(
        fields
            .iter()
            .map(|(name, field)| (name.clone(), value(field)))
            .collect(),
    )
}

/// A document value as BSON; fails only on what MongoDB cannot hold (a
/// decimal that is not one, an [`Ext`] of another engine).
pub fn bson(value: &Value) -> Result<Bson, String> {
    Ok(match value {
        Value::Null => Bson::Null,
        Value::Bool(value) => Bson::Boolean(*value),
        Value::Int32(n) => Bson::Int32(*n),
        Value::Int64(n) => Bson::Int64(*n),
        Value::Double(x) => Bson::Double(*x),
        Value::Decimal(text) => Bson::Decimal128(
            text.parse::<Decimal128>()
                .map_err(|_| format!("{text} is not a decimal number"))?,
        ),
        Value::String(text) => Bson::String(text.clone()),
        Value::Binary { subtype, bytes } => Bson::Binary(Binary {
            subtype: BinarySubtype::from(*subtype),
            bytes: bytes.clone(),
        }),
        Value::DateTime(millis) => Bson::DateTime(DateTime::from_millis(*millis)),
        Value::Array(items) => Bson::Array(items.iter().map(bson).collect::<Result<_, _>>()?),
        Value::Object(fields) => Bson::Document(bson_document(fields)?),
        Value::Ext(ext) => Bson::try_from(ext.raw.clone())
            .map_err(|_| format!("MongoDB cannot hold this {} value", ext.kind))?,
    })
}

/// A document as BSON. A field named twice keeps its last value, as
/// MongoDB would.
pub fn bson_document(fields: &Document) -> Result<bson::Document, String> {
    fields
        .0
        .iter()
        .map(|(name, field)| Ok((name.clone(), bson(field)?)))
        .collect()
}

/// What the grid and the schema call a type only MongoDB has.
fn kind(bson: &Bson) -> &'static str {
    match bson {
        Bson::ObjectId(_) => "ObjectId",
        Bson::Timestamp(_) => "Timestamp",
        Bson::RegularExpression(_) => "Regex",
        Bson::MinKey => "MinKey",
        Bson::MaxKey => "MaxKey",
        Bson::Symbol(_) => "Symbol",
        Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => "JavaScript",
        Bson::DbPointer(_) => "DBPointer",
        Bson::Undefined => "Undefined",
        // Not an `Ext`: see `value`.
        _ => "BSON",
    }
}

#[cfg(test)]
mod tests {
    use bson::oid::ObjectId;
    use bson::{Regex, Timestamp, doc};

    use super::*;

    fn ext(bson: Bson) -> Ext {
        match value(&bson) {
            Value::Ext(ext) => ext,
            other => panic!("{other:?} is not an engine type"),
        }
    }

    #[test]
    fn keeps_numbers_dates_and_decimals_as_they_are() {
        let original = doc! {
            "int": 42,
            "long": 9_007_199_254_740_993_i64,
            "double": 42.0,
            "decimal": "1234567890.123456789".parse::<Decimal128>().unwrap(),
            "date": DateTime::from_millis(-14_182_940_000),
            "uuid": Binary { subtype: BinarySubtype::Uuid, bytes: vec![7; 16] },
            "nested": { "b": [1, { "c": true }], "a": Bson::Null },
        };
        let converted = document(&original);
        assert_eq!(converted.get("int"), Some(&Value::Int32(42)));
        assert_eq!(
            converted.get("long"),
            Some(&Value::Int64(9_007_199_254_740_993))
        );
        assert_eq!(converted.get("double"), Some(&Value::Double(42.0)));
        assert_eq!(
            converted.get("decimal"),
            Some(&Value::Decimal("1234567890.123456789".into()))
        );
        assert_eq!(
            converted.get("date"),
            Some(&Value::DateTime(-14_182_940_000))
        );
        assert_eq!(converted.get("uuid").unwrap().type_name(), "UUID");
        // Field order survives, nested too.
        let Some(Value::Object(nested)) = converted.get("nested") else {
            panic!("nested is a document");
        };
        let names: Vec<&str> = nested.0.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["b", "a"]);
        assert_eq!(bson_document(&converted).unwrap(), original);
    }

    #[test]
    fn engine_types_keep_their_extended_json_and_come_back() {
        let id = ObjectId::parse_str("65f0c0ffee0000000000beef").unwrap();
        let object_id = ext(Bson::ObjectId(id));
        assert_eq!(object_id.kind, "ObjectId");
        assert_eq!(object_id.display, r#"ObjectId("65f0c0ffee0000000000beef")"#);
        assert_eq!(
            object_id.raw,
            serde_json::json!({"$oid": "65f0c0ffee0000000000beef"})
        );
        let regex = Bson::RegularExpression(Regex {
            pattern: "^a".to_string().try_into().unwrap(),
            options: "i".to_string().try_into().unwrap(),
        });
        for original in [
            Bson::ObjectId(id),
            Bson::Timestamp(Timestamp {
                time: 1_761_654_896,
                increment: 3,
            }),
            regex,
            Bson::MinKey,
            Bson::MaxKey,
            Bson::Symbol("s".into()),
            Bson::JavaScriptCode("x + 1".into()),
            Bson::Undefined,
        ] {
            let converted = value(&original);
            assert!(matches!(converted, Value::Ext(_)), "{original:?}");
            assert_eq!(bson(&converted).unwrap(), original);
        }
        assert_eq!(
            ext(Bson::Timestamp(Timestamp {
                time: 1,
                increment: 2
            }))
            .display,
            "Timestamp({t: 1, i: 2})"
        );
    }

    #[test]
    fn refuses_what_mongodb_cannot_hold() {
        assert!(bson(&Value::Decimal("twelve".into())).is_err());
        let foreign = Value::Ext(Ext {
            kind: "StringSet",
            display: "<<\"a\">>".into(),
            raw: serde_json::json!({"SS": ["a"], "$weird": 1}),
        });
        // Not Extended JSON of a type: read as a plain document instead.
        assert!(matches!(bson(&foreign), Ok(Bson::Document(_))));
        let broken = Value::Ext(Ext {
            kind: "ObjectId",
            display: "ObjectId(\"x\")".into(),
            raw: serde_json::json!({"$oid": "x"}),
        });
        assert_eq!(
            bson(&broken).unwrap_err(),
            "MongoDB cannot hold this ObjectId value"
        );
    }
}
