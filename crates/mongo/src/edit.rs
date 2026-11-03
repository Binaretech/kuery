//! A document change from the grid as the command that makes it: an
//! `update` of the leaves that changed (`$set`, and `$unset` for what was
//! removed, never `null`), a `replace`, an `insert` or a `delete`, each
//! found by `_id` and by what the change expects the document to hold, so a
//! document someone changed since it was loaded matches nothing.

use bson::{Bson, Document as BsonDocument, doc};
use kuery_core::document::change::DocChange;
use kuery_core::document::value::{CollectionRef, DocKey, FieldPath, PathPart, Value};
use kuery_core::session::QueryError;

use crate::command::Write;
use crate::shell::{Method, Op};
use crate::{convert, literal};

/// The change as the mongosh statement that does what [`command`] does:
/// `db.orders.updateOne({_id: 7, $expr: {$and: [{$eq: ["$qty", {$literal:
/// NumberLong("2")}]}, {$eq: [{$type: "$qty"}, "long"]}]}}, {$set: {qty:
/// NumberLong("3")}})`. A delete that expects the whole document shows
/// its `_id` alone: the review and the audit log keep to what changes,
/// not whole documents. A change MongoDB cannot hold is described instead.
pub(crate) fn statement(collection: &CollectionRef, change: &DocChange) -> String {
    let name = collection.name();
    let shown = match change {
        DocChange::Delete {
            key,
            version,
            expect,
        } => DocChange::Delete {
            key: key.clone(),
            version: version.clone(),
            expect: expect
                .iter()
                .filter(|(path, _)| !path.0.is_empty())
                .cloned()
                .collect(),
        },
        other => other.clone(),
    };
    let Ok((command, _)) = command(name, &shown) else {
        return change.describe(literal::render);
    };
    let first = |field: &str| {
        command
            .get_array(field)
            .ok()
            .and_then(|items| items.first())
            .and_then(Bson::as_document)
            .cloned()
            .unwrap_or_default()
    };
    let part = |document: &BsonDocument, field: &str| match document.get(field) {
        Some(Bson::Document(part)) => Value::Object(convert::document(part)),
        _ => Value::Object(Default::default()),
    };
    let (method, args) = match change {
        DocChange::Update { .. } | DocChange::Replace { .. } => {
            let update = first("updates");
            let method = match change {
                DocChange::Replace { .. } => Method::ReplaceOne,
                _ => Method::UpdateOne,
            };
            (method, vec![part(&update, "q"), part(&update, "u")])
        }
        DocChange::Insert { .. } => (
            Method::InsertOne,
            vec![Value::Object(convert::document(&first("documents")))],
        ),
        DocChange::Delete { .. } => (Method::DeleteOne, vec![part(&first("deletes"), "q")]),
    };
    crate::shell::print(&Op::Collection {
        target: CollectionRef::new(&[name]),
        method,
        args,
        chain: Vec::new(),
    })
}

/// The command that applies `change` to the collection `name`, and the
/// kind of write its answer counts.
pub(crate) fn command(name: &str, change: &DocChange) -> Result<(BsonDocument, Write), QueryError> {
    Ok(match change {
        DocChange::Update {
            key,
            set,
            unset,
            expect,
            ..
        } => {
            let mut update = BsonDocument::new();
            if !set.is_empty() {
                let mut fields = BsonDocument::new();
                for (path, value) in set {
                    fields.insert(dotted(path)?, bson(value)?);
                }
                update.insert("$set", fields);
            }
            if !unset.is_empty() {
                let mut fields = BsonDocument::new();
                for path in unset {
                    fields.insert(dotted(path)?, "");
                }
                update.insert("$unset", fields);
            }
            let update = doc! { "q": filter(key, expect)?, "u": update };
            (doc! { "update": name, "updates": [update] }, Write::Update)
        }
        DocChange::Replace { key, document, .. } => {
            let replacement = convert::bson_document(document).map_err(QueryError::Unsupported)?;
            let update = doc! { "q": filter(key, &[])?, "u": replacement };
            (doc! { "update": name, "updates": [update] }, Write::Update)
        }
        DocChange::Insert { document } => {
            let document = convert::bson_document(document).map_err(QueryError::Unsupported)?;
            (
                doc! { "insert": name, "documents": [document] },
                Write::Insert,
            )
        }
        DocChange::Delete { key, expect, .. } => {
            let delete = doc! { "q": filter(key, expect)?, "limit": 1 };
            (doc! { "delete": name, "deletes": [delete] }, Write::Delete)
        }
    })
}

/// Why MongoDB cannot make the change, if it cannot: what [`command`]
/// refuses, found before anything is sent.
pub(crate) fn check(change: &DocChange) -> Result<(), String> {
    command("check", change)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// The document by its `_id`, holding what the change expects: each value
/// equal to the earlier one and of the same type, nested values too, and
/// absent where it was absent. The checks are an `$expr`, which compares
/// whole values: a query's `{f: v}` also finds an array that holds `v`,
/// `{f: null}` a document without `f`, and `2` equals `NumberLong(2)`
/// both ways. `_id` stays a plain equality, which the index serves.
fn filter(key: &DocKey, expect: &[(FieldPath, Option<Value>)]) -> Result<BsonDocument, QueryError> {
    let id = match bson(&key.0)? {
        // A document is compared as a whole, even with keys such as `$gt`.
        id @ Bson::Document(_) => Bson::Document(doc! { "$eq": id }),
        id => id,
    };
    let mut filter = doc! { "_id": id };
    let mut checks = Vec::new();
    let mut containers: Vec<&[PathPart]> = Vec::new();
    for (path, value) in expect {
        // What the path goes through was a document (or an array) too.
        for depth in 1..path.0.len() {
            let parent = &path.0[..depth];
            if containers.contains(&parent) {
                continue;
            }
            containers.push(parent);
            let kind = match path.0[depth] {
                PathPart::Field(_) => "object",
                PathPart::Index(_) => "array",
            };
            checks.push(type_is(reach(parent)?, kind));
        }
        let at = reach(&path.0)?;
        match value {
            None => checks.push(type_is(at, "missing")),
            Some(value) => holds(at, &bson(value)?, &mut checks),
        }
    }
    if !checks.is_empty() {
        filter.insert("$expr", all(checks));
    }
    Ok(filter)
}

/// The expression for the value at a path, absent where it is: `"$qty"`,
/// `{$getField: {field: "city", input: "$address"}}`, the whole document
/// for an empty path. `$getField` reaches any name, and finds nothing in
/// what is not a document.
fn reach(path: &[PathPart]) -> Result<Bson, QueryError> {
    let mut at = Bson::String("$$ROOT".into());
    for (depth, part) in path.iter().enumerate() {
        at = match part {
            PathPart::Field(name) if depth == 0 && plain(name) => Bson::String(format!("${name}")),
            PathPart::Field(name) => field(at, name),
            PathPart::Index(index) => {
                let index = i32::try_from(*index).map_err(|_| {
                    QueryError::Unsupported(format!("No array has an element {index}"))
                })?;
                // Only reached once the check before it found an array.
                element(at, index)
            }
        };
    }
    Ok(at)
}

/// Whether a name reads as itself in a field path (`$name`).
fn plain(name: &str) -> bool {
    !name.is_empty() && !name.contains('.') && !name.starts_with('$')
}

/// The field `name` of the document at `input`.
fn field(input: Bson, name: &str) -> Bson {
    // A name that starts with `$` would read as a path.
    let name = match name.starts_with('$') {
        true => Bson::Document(doc! { "$literal": name }),
        false => Bson::String(name.into()),
    };
    Bson::Document(doc! { "$getField": { "field": name, "input": input } })
}

/// The element `index` of the array at `input`.
fn element(input: Bson, index: i32) -> Bson {
    Bson::Document(doc! { "$arrayElemAt": [input, index] })
}

/// The checks that the value at `at` is `value`: equal (`$literal`, so
/// text such as `"$foo"` is not read as a path), of its type, and of the
/// same types inside, since equality alone takes `2` for `NumberLong(2)`.
/// Undefined, which a query cannot compare to, is told by its type alone:
/// it holds nothing else.
fn holds(at: Bson, value: &Bson, checks: &mut Vec<Bson>) {
    if let Bson::Undefined = value {
        checks.push(type_is(at, "undefined"));
        return;
    }
    checks.push(Bson::Document(doc! {
        "$eq": [at.clone(), { "$literal": value.clone() }],
    }));
    checks.push(type_is(at.clone(), type_alias(value)));
    if let Some(inside) = inside(&at, value) {
        checks.push(inside);
    }
}

/// The check that what an array or a document holds is of the types it
/// held, at every depth: its elements' types as a list, then inside each
/// element that holds more. Only looked at once `at` is of its type, since
/// listing what is not an array fails.
fn inside(at: &Bson, value: &Bson) -> Option<Bson> {
    let holds_more = |item: &Bson| match item {
        Bson::Array(items) => !items.is_empty(),
        Bson::Document(fields) => !fields.is_empty(),
        _ => false,
    };
    let (kind, list, types, more): (_, Bson, Vec<&str>, Vec<Bson>) = match value {
        Bson::Array(items) if !items.is_empty() => (
            "array",
            doc! { "$map": { "input": at.clone(), "in": { "$type": "$$this" } } }.into(),
            items.iter().map(type_alias).collect(),
            items
                .iter()
                .zip(0..)
                .filter(|(item, _)| holds_more(item))
                .filter_map(|(item, index)| inside(&element(at.clone(), index), item))
                .collect(),
        ),
        Bson::Document(fields) if !fields.is_empty() => (
            "object",
            doc! { "$map": { "input": { "$objectToArray": at.clone() }, "in": { "$type": "$$this.v" } } }
                .into(),
            fields.values().map(type_alias).collect(),
            fields
                .iter()
                .filter(|(_, item)| holds_more(item))
                .filter_map(|(name, item)| inside(&field(at.clone(), name), item))
                .collect(),
        ),
        _ => return None,
    };
    let mut checks = vec![Bson::Document(doc! { "$eq": [list, types] })];
    checks.extend(more);
    Some(Bson::Document(doc! {
        "$cond": [type_is(at.clone(), kind), all(checks), false],
    }))
}

/// `{$eq: [{$type: at}, kind]}`; `"missing"` for what is absent.
fn type_is(at: Bson, kind: &str) -> Bson {
    Bson::Document(doc! { "$eq": [{ "$type": at }, kind] })
}

/// All of the checks.
fn all(mut checks: Vec<Bson>) -> Bson {
    match checks.len() {
        1 => checks.remove(0),
        _ => Bson::Document(doc! { "$and": checks }),
    }
}

/// What `$type` calls a value's type.
fn type_alias(value: &Bson) -> &'static str {
    match value {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) => "javascript",
        Bson::JavaScriptCodeWithScope(_) => "javascriptWithScope",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Symbol(_) => "symbol",
        Bson::Decimal128(_) => "decimal",
        Bson::Undefined => "undefined",
        Bson::MaxKey => "maxKey",
        Bson::MinKey => "minKey",
        Bson::DbPointer(_) => "dbPointer",
    }
}

fn bson(value: &Value) -> Result<Bson, QueryError> {
    convert::bson(value).map_err(QueryError::Unsupported)
}

/// A path as MongoDB's update operators read it. A field whose name has a
/// dot or starts with `$` cannot be reached that way.
fn dotted(path: &FieldPath) -> Result<String, QueryError> {
    let unreachable = path.0.iter().any(|part| match part {
        PathPart::Field(name) => name.is_empty() || name.contains('.') || name.starts_with('$'),
        PathPart::Index(_) => false,
    });
    match unreachable {
        true => Err(QueryError::Unsupported(format!(
            "Kuery cannot edit the field {} in place: its name has a dot or starts with $. Change it from the console",
            path.dotted()
        ))),
        false => Ok(path.dotted()),
    }
}

#[cfg(test)]
mod tests {
    use bson::spec::BinarySubtype;
    use bson::{Binary, Regex};
    use kuery_core::document::value::Document;

    use super::*;

    fn key() -> DocKey {
        DocKey(Value::Int32(7))
    }

    /// The `q` of a command's first update or delete.
    fn query(command: &BsonDocument) -> BsonDocument {
        let list = match command.get_array("updates") {
            Ok(list) => list,
            Err(_) => command.get_array("deletes").unwrap(),
        };
        list[0]
            .as_document()
            .unwrap()
            .get_document("q")
            .unwrap()
            .clone()
    }

    fn eq(at: impl Into<Bson>, value: impl Into<Bson>) -> Bson {
        Bson::Document(doc! { "$eq": [at.into(), { "$literal": value.into() }] })
    }

    fn typed(at: impl Into<Bson>, kind: &str) -> Bson {
        Bson::Document(doc! { "$eq": [{ "$type": at.into() }, kind] })
    }

    #[test]
    fn sets_unsets_and_checks_what_was_there() {
        let change = DocChange::Update {
            key: key(),
            version: None,
            set: vec![
                (
                    FieldPath::parse("address.city"),
                    Value::String("Cusco".into()),
                ),
                (FieldPath::field("coupon"), Value::Decimal("1.50".into())),
            ],
            unset: vec![FieldPath::field("qty"), FieldPath::field("note")],
            expect: vec![
                (
                    FieldPath::parse("address.city"),
                    Some(Value::String("Lima".into())),
                ),
                (FieldPath::field("coupon"), None),
                (FieldPath::field("qty"), Some(Value::Int64(2))),
                (FieldPath::field("note"), Some(Value::Null)),
            ],
        };
        let (command, write) = command("orders", &change).unwrap();
        assert_eq!(write, Write::Update);
        let city = doc! { "$getField": { "field": "city", "input": "$address" } };
        assert_eq!(
            command,
            doc! {
                "update": "orders",
                "updates": [{
                    "q": {
                        "_id": 7,
                        "$expr": { "$and": [
                            // What the path goes through is still a document.
                            typed("$address", "object"),
                            eq(city.clone(), "Lima"),
                            typed(city, "string"),
                            // Absent, not null.
                            typed("$coupon", "missing"),
                            // NumberLong(2), not 2.
                            eq("$qty", 2_i64),
                            typed("$qty", "long"),
                            // Null, not absent.
                            eq("$note", Bson::Null),
                            typed("$note", "null"),
                        ] },
                    },
                    "u": {
                        "$set": {
                            "address.city": "Cusco",
                            "coupon": Bson::Decimal128("1.50".parse().unwrap()),
                        },
                        "$unset": { "qty": "", "note": "" },
                    },
                }],
            }
        );
    }

    #[test]
    fn checks_the_types_inside_what_it_expects() {
        let expect = |value: Value| {
            let change = DocChange::Update {
                key: key(),
                version: None,
                set: vec![(FieldPath::field("f"), Value::Int32(1))],
                unset: Vec::new(),
                expect: vec![(FieldPath::field("f"), Some(value))],
            };
            query(&command("orders", &change).unwrap().0)
        };
        // Equality alone finds [2] equal to [NumberLong(2)]: each element's
        // type is checked too, at every depth, once the array is one.
        let nested = Value::Array(vec![
            Value::Int64(2),
            Value::Object(Document(vec![("a".into(), Value::Double(1.0))])),
        ]);
        let element = doc! { "$arrayElemAt": ["$f", 1] };
        assert_eq!(
            expect(nested),
            doc! {
                "_id": 7,
                "$expr": { "$and": [
                    eq("$f", vec![Bson::Int64(2), Bson::Document(doc! { "a": 1.0 })]),
                    typed("$f", "array"),
                    { "$cond": [
                        typed("$f", "array"),
                        { "$and": [
                            { "$eq": [{ "$map": { "input": "$f", "in": { "$type": "$$this" } } }, ["long", "object"]] },
                            { "$cond": [
                                typed(element.clone(), "object"),
                                { "$eq": [
                                    { "$map": { "input": { "$objectToArray": element }, "in": { "$type": "$$this.v" } } },
                                    ["double"],
                                ] },
                                false,
                            ] },
                        ] },
                        false,
                    ] },
                ] },
            }
        );
        // Undefined cannot be compared to: its type says it on its own.
        let undefined = Value::Ext(kuery_core::document::value::Ext {
            kind: "Undefined",
            display: "undefined".into(),
            raw: serde_json::json!({ "$undefined": true }),
        });
        assert_eq!(
            expect(undefined),
            doc! { "_id": 7, "$expr": typed("$f", "undefined") }
        );
        // Text that looks like a path is a literal.
        assert_eq!(
            expect(Value::String("$other".into())),
            doc! { "_id": 7, "$expr": { "$and": [eq("$f", "$other"), typed("$f", "string")] } }
        );
        for (value, kind) in [
            (Value::Decimal("1.50".into()), "decimal"),
            (Value::Double(f64::NAN), "double"),
            (Value::DateTime(i64::MAX), "date"),
            (
                Value::Binary {
                    subtype: 4,
                    bytes: vec![1; 16],
                },
                "binData",
            ),
        ] {
            let query = expect(value);
            let checks = query
                .get_document("$expr")
                .unwrap()
                .get_array("$and")
                .unwrap();
            assert_eq!(checks[1], typed("$f", kind));
        }
        assert_eq!(
            type_alias(&Bson::RegularExpression(Regex {
                pattern: "a".to_string().try_into().unwrap(),
                options: String::new().try_into().unwrap(),
            })),
            "regex"
        );
        assert_eq!(
            type_alias(&Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes: Vec::new(),
            })),
            "binData"
        );
    }

    #[test]
    fn reaches_any_field_name_and_array_elements() {
        let change = DocChange::Update {
            key: DocKey(Value::Object(Document(vec![(
                "$gt".into(),
                Value::Int32(1),
            )]))),
            version: None,
            set: vec![(FieldPath::field("n"), Value::Int32(1))],
            unset: Vec::new(),
            expect: vec![
                (
                    FieldPath(vec![
                        PathPart::Field("$x".into()),
                        PathPart::Field("a.b".into()),
                        PathPart::Index(2),
                    ]),
                    Some(Value::Int32(1)),
                ),
                (FieldPath(vec![PathPart::Field(String::new())]), None),
            ],
        };
        let x = doc! { "$getField": { "field": { "$literal": "$x" }, "input": "$$ROOT" } };
        let ab = doc! { "$getField": { "field": "a.b", "input": x.clone() } };
        let element = doc! { "$arrayElemAt": [ab.clone(), 2] };
        let empty = doc! { "$getField": { "field": "", "input": "$$ROOT" } };
        assert_eq!(
            query(&command("orders", &change).unwrap().0),
            doc! {
                // A key that is a document is compared whole, `$gt` and all.
                "_id": { "$eq": { "$gt": 1 } },
                "$expr": { "$and": [
                    typed(x, "object"),
                    typed(ab, "array"),
                    eq(element.clone(), 1),
                    typed(element, "int"),
                    typed(empty, "missing"),
                ] },
            }
        );
    }

    #[test]
    fn inserts_replaces_and_deletes_by_key() {
        let document = Document(vec![
            ("_id".into(), Value::Int32(7)),
            ("n".into(), Value::Int64(1)),
        ]);
        let (insert, write) = command(
            "orders",
            &DocChange::Insert {
                document: document.clone(),
            },
        )
        .unwrap();
        assert_eq!(write, Write::Insert);
        assert_eq!(
            insert,
            doc! { "insert": "orders", "documents": [{ "_id": 7, "n": 1_i64 }] }
        );
        let (replace, _) = command(
            "orders",
            &DocChange::Replace {
                key: key(),
                version: None,
                document: document.clone(),
            },
        )
        .unwrap();
        assert_eq!(
            replace,
            doc! { "update": "orders", "updates": [{ "q": { "_id": 7 }, "u": { "_id": 7, "n": 1_i64 } }] }
        );
        let (delete, write) = command(
            "orders",
            &DocChange::Delete {
                key: key(),
                version: None,
                expect: vec![(FieldPath::field("n"), Some(Value::Double(1.0)))],
            },
        )
        .unwrap();
        assert_eq!(write, Write::Delete);
        assert_eq!(
            delete,
            doc! {
                "delete": "orders",
                "deletes": [{
                    "q": { "_id": 7, "$expr": { "$and": [eq("$n", 1.0), typed("$n", "double")] } },
                    "limit": 1,
                }],
            }
        );
        // The whole document: every field as it was, and no other.
        let whole = DocChange::Delete {
            key: key(),
            version: None,
            expect: vec![(FieldPath(Vec::new()), Some(Value::Object(document)))],
        };
        let (delete, _) = command("orders", &whole).unwrap();
        assert_eq!(
            query(&delete),
            doc! {
                "_id": 7,
                "$expr": { "$and": [
                    eq("$$ROOT", doc! { "_id": 7, "n": 1_i64 }),
                    typed("$$ROOT", "object"),
                    { "$cond": [
                        typed("$$ROOT", "object"),
                        { "$eq": [
                            { "$map": { "input": { "$objectToArray": "$$ROOT" }, "in": { "$type": "$$this.v" } } },
                            ["int", "long"],
                        ] },
                        false,
                    ] },
                ] },
            }
        );
    }

    #[test]
    fn prints_each_change_as_its_statement() {
        let orders = CollectionRef::new(&["shop", "orders"]);
        let update = DocChange::Update {
            key: key(),
            version: None,
            set: vec![(FieldPath::field("qty"), Value::Int64(3))],
            unset: vec![FieldPath::field("note")],
            expect: vec![
                (FieldPath::field("qty"), Some(Value::Int64(2))),
                (FieldPath::field("note"), None),
            ],
        };
        assert_eq!(
            statement(&orders, &update),
            r#"db.orders.updateOne({_id: 7, $expr: {$and: [{$eq: ["$qty", {$literal: NumberLong("2")}]}, {$eq: [{$type: "$qty"}, "long"]}, {$eq: [{$type: "$note"}, "missing"]}]}}, {$set: {qty: NumberLong("3")}, $unset: {note: ""}})"#
        );
        let insert = DocChange::Insert {
            document: Document(vec![("a".into(), Value::Int32(1))]),
        };
        assert_eq!(statement(&orders, &insert), "db.orders.insertOne({a: 1})");
        // A delete that expects the whole document shows its key alone.
        let delete = DocChange::Delete {
            key: key(),
            version: None,
            expect: vec![(
                FieldPath(Vec::new()),
                Some(Value::Object(Document(vec![
                    ("_id".into(), Value::Int32(7)),
                    ("secret".into(), Value::String("kept out".into())),
                ]))),
            )],
        };
        assert_eq!(statement(&orders, &delete), "db.orders.deleteOne({_id: 7})");
        let lines = CollectionRef::new(&["shop", "order-lines"]);
        assert_eq!(
            statement(&lines, &delete),
            r#"db.getCollection("order-lines").deleteOne({_id: 7})"#
        );
        // What MongoDB cannot hold is described.
        let bad = DocChange::Insert {
            document: Document(vec![("a".into(), Value::Decimal("x".into()))]),
        };
        assert!(statement(&orders, &bad).starts_with("insert of"));
    }

    #[test]
    fn refuses_fields_an_update_cannot_reach() {
        for name in ["a.b", "$x", ""] {
            let change = DocChange::Update {
                key: key(),
                version: None,
                set: vec![(
                    FieldPath(vec![
                        PathPart::Field("ok".into()),
                        PathPart::Field(name.into()),
                    ]),
                    Value::Int32(1),
                )],
                unset: Vec::new(),
                expect: Vec::new(),
            };
            assert!(
                matches!(
                    command("orders", &change),
                    Err(QueryError::Unsupported(message)) if message.contains("cannot edit the field")
                ),
                "{name:?}"
            );
            // Found before anything is staged.
            assert!(check(&change).is_err(), "{name:?}");
        }
        let fine = DocChange::Insert {
            document: Document(vec![("a.b".into(), Value::Int32(1))]),
        };
        assert_eq!(check(&fine), Ok(()));
    }
}
