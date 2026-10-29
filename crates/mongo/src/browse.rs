//! A page of a collection as a `find` command: the filter bar, the quick
//! filters from the column headers, the projection, the sort and where the
//! page starts. Pure, so each command is tested without a server.
//!
//! Without a sort, pages follow `_id` and each starts after the last one
//! seen, in the order MongoDB sorts values of every type, so none is
//! skipped or seen twice however far the user pages. With a sort, pages
//! skip what came before, which the collection tab says can shift when
//! documents change.

use bson::spec::ElementType;
use bson::{Bson, Document as BsonDocument, doc};
use kuery_core::browse::{BrowseError, Direction, Filter, FilterOp};
use kuery_core::document::change::{BrowseSpec, Continuation};
use kuery_core::document::value::Value;
use kuery_core::read_only::Rejection;
use kuery_core::session::QueryError;

use crate::{convert, literal};

/// The `find` command for one page of `collection`; `batch` is the first
/// batch's size.
pub(crate) fn find(
    collection: &str,
    spec: &BrowseSpec,
    batch: usize,
) -> Result<BsonDocument, QueryError> {
    let mut conditions = Vec::new();
    if let Some(filter) = bar(spec.filter.as_deref())?
        && !filter.is_empty()
    {
        conditions.push(filter);
    }
    for quick in &spec.quick {
        conditions.push(quick_filter(quick)?);
    }
    let mut skip = 0;
    match &spec.after {
        None => {}
        Some(Continuation::After(last)) => {
            let last = convert::bson(last).map_err(QueryError::Unsupported)?;
            conditions.push(after(last));
        }
        Some(Continuation::Skip(n)) => skip = *n,
        Some(Continuation::Token(_)) => {
            return Err(QueryError::Unsupported(
                "MongoDB pages by _id or by skipping, not by a token".into(),
            ));
        }
    }
    let filter = match conditions.len() {
        0 => BsonDocument::new(),
        1 => conditions.remove(0),
        _ => doc! { "$and": conditions },
    };
    let mut sort = BsonDocument::new();
    for (path, direction) in &spec.sort {
        sort.insert(path.dotted(), order(*direction));
    }
    // Ties broken by `_id`, so skipping pages is stable; alone, the keyset.
    if !sort.contains_key("_id") {
        sort.insert("_id", 1);
    }
    let mut command = doc! { "find": collection, "filter": filter, "sort": sort };
    if let Some(projection) = bar(spec.projection.as_deref())?
        && !projection.is_empty()
    {
        command.insert("projection", projection);
    }
    if skip > 0 {
        command.insert("skip", i64::try_from(skip).unwrap_or(i64::MAX));
    }
    let mut batch = batch.max(1);
    if spec.limit > 0 {
        command.insert("limit", i64::try_from(spec.limit).unwrap_or(i64::MAX));
        batch = batch.min(spec.limit);
    }
    command.insert("batchSize", i64::try_from(batch).unwrap_or(i64::MAX));
    Ok(command)
}

/// Where the page after this one starts: `None` after a short page (the
/// last), and for a browse without a limit, which read everything.
pub(crate) fn next(
    spec: &BrowseSpec,
    returned: usize,
    last: Option<Value>,
) -> Option<Continuation> {
    if spec.limit == 0 || returned < spec.limit {
        return None;
    }
    let skipped = match spec.after {
        Some(Continuation::Skip(n)) => n,
        _ => 0,
    };
    let by_id = spec.sort.is_empty() && keeps_id(spec);
    match (by_id, last, &spec.after) {
        (true, Some(id), _) => Some(Continuation::After(id)),
        // Paged by `_id` until the projection left it out: there is no
        // telling where the next page starts.
        (true, None, Some(Continuation::After(_))) => None,
        _ => Some(Continuation::Skip(skipped + returned as u64)),
    }
}

/// Whether the projection hands back each document's own `_id`, so a page
/// can start after the last one: not when it leaves `_id` out, and not
/// when it computes one (`{_id: "$status"}`), which is no document's key.
fn keeps_id(spec: &BrowseSpec) -> bool {
    let Ok(Some(projection)) = bar(spec.projection.as_deref()) else {
        return true;
    };
    match projection.get("_id") {
        None => true,
        Some(Bson::Boolean(keep)) => *keep,
        Some(Bson::Int32(n)) => *n != 0,
        Some(Bson::Int64(n)) => *n != 0,
        Some(Bson::Double(n)) => *n != 0.0,
        Some(_) => false,
    }
}

/// `$type`'s names in the order MongoDB sorts values of different types,
/// each with its place: numbers of every width share one, as do strings
/// and symbols, which `$gt` compares with each other.
const TYPE_ORDER: &[(&str, u8)] = &[
    ("minKey", 0),
    ("undefined", 1),
    ("null", 2),
    ("int", 3),
    ("long", 3),
    ("double", 3),
    ("decimal", 3),
    ("symbol", 4),
    ("string", 4),
    ("object", 5),
    ("array", 6),
    ("binData", 7),
    ("objectId", 8),
    ("bool", 9),
    ("date", 10),
    ("timestamp", 11),
    ("regex", 12),
    ("dbPointer", 13),
    ("javascript", 14),
    ("javascriptWithScope", 15),
    ("maxKey", 16),
];

/// Every `_id` sorted after `last`. `$gt` only matches values of `last`'s
/// own type (a number after a number), so the `_id`s of the types sorted
/// later are asked for by type, or a collection whose `_id`s mix types
/// would end at the last of the first.
fn after(last: Bson) -> BsonDocument {
    let place = TYPE_ORDER
        .iter()
        .find(|(name, _)| *name == type_name(last.element_type()))
        .map_or(0, |(_, place)| *place);
    let later: Vec<&str> = TYPE_ORDER
        .iter()
        .filter(|(_, other)| *other > place)
        .map(|(name, _)| *name)
        .collect();
    match later.is_empty() {
        true => doc! { "_id": { "$gt": last } },
        false => doc! { "$or": [
            { "_id": { "$gt": last } },
            { "_id": { "$type": later } },
        ] },
    }
}

/// A BSON type's name in `$type`.
fn type_name(element: ElementType) -> &'static str {
    match element {
        ElementType::MinKey => "minKey",
        ElementType::Undefined => "undefined",
        ElementType::Null => "null",
        ElementType::Int32 => "int",
        ElementType::Int64 => "long",
        ElementType::Double => "double",
        ElementType::Decimal128 => "decimal",
        ElementType::Symbol => "symbol",
        ElementType::String => "string",
        ElementType::EmbeddedDocument => "object",
        ElementType::Array => "array",
        ElementType::Binary => "binData",
        ElementType::ObjectId => "objectId",
        ElementType::Boolean => "bool",
        ElementType::DateTime => "date",
        ElementType::Timestamp => "timestamp",
        ElementType::RegularExpression => "regex",
        ElementType::DbPointer => "dbPointer",
        ElementType::JavaScriptCode => "javascript",
        ElementType::JavaScriptCodeWithScope => "javascriptWithScope",
        ElementType::MaxKey => "maxKey",
    }
}

fn order(direction: Direction) -> i32 {
    match direction {
        Direction::Ascending => 1,
        Direction::Descending => -1,
    }
}

/// A filter or projection bar's text as BSON; `None` when empty.
pub(crate) fn bar(text: Option<&str>) -> Result<Option<BsonDocument>, QueryError> {
    let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
        return Ok(None);
    };
    let document = literal::parse_document(text)
        .map_err(|error| QueryError::Rejected(Rejection::Parse(error.located(text))))?;
    convert::bson_document(&document)
        .map(Some)
        .map_err(QueryError::Unsupported)
}

/// A quick filter from a column header. Its value reads as a literal when
/// it is one (`42`, `ObjectId("…")`, `true`), and as text otherwise.
fn quick_filter(filter: &Filter) -> Result<BsonDocument, QueryError> {
    let value = filter.value.trim();
    if filter.op.needs_value() && value.is_empty() {
        return Err(QueryError::Unsupported(
            BrowseError::MissingValue(filter.column.clone()).to_string(),
        ));
    }
    let typed = |value: &str| -> Result<Bson, QueryError> {
        let read = literal::parse_value(value).unwrap_or_else(|_| Value::String(value.into()));
        convert::bson(&read).map_err(QueryError::Unsupported)
    };
    let pattern = |pattern: String| doc! { "$regex": pattern, "$options": "i" };
    let condition = match filter.op {
        FilterOp::Equals => doc! { "$eq": typed(value)? },
        FilterOp::NotEquals => doc! { "$ne": typed(value)? },
        FilterOp::Greater => doc! { "$gt": typed(value)? },
        FilterOp::GreaterOrEqual => doc! { "$gte": typed(value)? },
        FilterOp::Less => doc! { "$lt": typed(value)? },
        FilterOp::LessOrEqual => doc! { "$lte": typed(value)? },
        FilterOp::Contains => pattern(escape(value)),
        FilterOp::StartsWith => pattern(format!("^{}", escape(value))),
        FilterOp::OneOf => {
            let values = value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(typed)
                .collect::<Result<Vec<_>, _>>()?;
            doc! { "$in": values }
        }
        // Null or absent: the grid shows both as empty.
        FilterOp::IsNull => doc! { "$eq": Bson::Null },
        FilterOp::IsNotNull => doc! { "$exists": true, "$ne": Bson::Null },
    };
    Ok(doc! { filter.column.clone(): condition })
}

/// Text matched as it is inside a regular expression.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\^$.|?*+()[]{}/".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use kuery_core::document::value::FieldPath;

    use super::*;

    fn quick(column: &str, op: FilterOp, value: &str) -> Filter {
        Filter {
            column: column.into(),
            op,
            value: value.into(),
        }
    }

    fn page(spec: BrowseSpec) -> BsonDocument {
        find("orders", &spec, 500).unwrap()
    }

    #[test]
    fn pages_by_id_without_a_sort() {
        let first = page(BrowseSpec {
            limit: 100,
            ..BrowseSpec::default()
        });
        assert_eq!(
            first,
            doc! {
                "find": "orders",
                "filter": {},
                "sort": { "_id": 1 },
                "limit": 100_i64,
                "batchSize": 100_i64,
            }
        );
        let spec = BrowseSpec {
            limit: 100,
            after: Some(Continuation::After(Value::Int32(99))),
            ..BrowseSpec::default()
        };
        assert_eq!(
            page(spec.clone()).get_document("filter").unwrap(),
            &after(Bson::Int32(99))
        );
        assert_eq!(
            next(&spec, 100, Some(Value::Int32(199))),
            Some(Continuation::After(Value::Int32(199)))
        );
        // A short page is the last.
        assert_eq!(next(&spec, 40, Some(Value::Int32(239))), None);
    }

    #[test]
    fn the_next_page_takes_the_ids_of_types_sorted_later() {
        // After a number: a larger number, or any type that sorts later.
        assert_eq!(
            after(Bson::Int32(2)),
            doc! { "$or": [
                { "_id": { "$gt": 2 } },
                { "_id": { "$type": [
                    "symbol", "string", "object", "array", "binData", "objectId",
                    "bool", "date", "timestamp", "regex", "dbPointer", "javascript",
                    "javascriptWithScope", "maxKey",
                ] } },
            ] }
        );
        // After a string: no other number.
        let types = |last: Bson| {
            after(last).get_array("$or").unwrap()[1]
                .as_document()
                .and_then(|id| id.get_document("_id").ok())
                .and_then(|id| id.get_array("$type").ok())
                .map(|types| {
                    types
                        .iter()
                        .filter_map(Bson::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap()
        };
        assert!(types(Bson::String("b".into())).starts_with("object array"));
        // A null `_id` sorts before every number.
        assert!(types(Bson::Null).starts_with("int long double decimal symbol"));
        // Nothing sorts after MaxKey.
        assert_eq!(after(Bson::MaxKey), doc! { "_id": { "$gt": Bson::MaxKey } });
    }

    #[test]
    fn skips_pages_with_a_sort() {
        let spec = BrowseSpec {
            sort: vec![(FieldPath::parse("total"), Direction::Descending)],
            limit: 50,
            after: Some(Continuation::Skip(100)),
            ..BrowseSpec::default()
        };
        let command = page(spec.clone());
        assert_eq!(
            command.get_document("sort").unwrap(),
            &doc! { "total": -1, "_id": 1 }
        );
        assert_eq!(command.get_i64("skip").ok(), Some(100));
        assert_eq!(
            next(&spec, 50, Some(Value::Int32(1))),
            Some(Continuation::Skip(150))
        );
    }

    #[test]
    fn joins_the_filter_bar_and_the_quick_filters() {
        let command = page(BrowseSpec {
            filter: Some("{status: 'paid'}".into()),
            quick: vec![
                quick("number", FilterOp::Greater, "10"),
                quick(
                    "customer",
                    FilterOp::Equals,
                    "ObjectId('65f0c0ffee0000000000beef')",
                ),
                quick("status", FilterOp::Contains, "pa.d"),
                quick("coupon", FilterOp::StartsWith, "SAVE"),
                quick("number", FilterOp::OneOf, "1, 2,x"),
                quick("coupon", FilterOp::IsNull, ""),
                quick("note", FilterOp::IsNotNull, ""),
            ],
            after: Some(Continuation::After(Value::Int32(7))),
            limit: 10,
            ..BrowseSpec::default()
        });
        let id = bson::oid::ObjectId::parse_str("65f0c0ffee0000000000beef").unwrap();
        assert_eq!(
            command.get_document("filter").unwrap(),
            &doc! { "$and": [
                { "status": "paid" },
                { "number": { "$gt": 10 } },
                { "customer": { "$eq": id } },
                { "status": { "$regex": "pa\\.d", "$options": "i" } },
                { "coupon": { "$regex": "^SAVE", "$options": "i" } },
                { "number": { "$in": [1, 2, "x"] } },
                { "coupon": { "$eq": Bson::Null } },
                { "note": { "$exists": true, "$ne": Bson::Null } },
                after(Bson::Int32(7)),
            ] }
        );
        let missing = find(
            "orders",
            &BrowseSpec {
                quick: vec![quick("status", FilterOp::Equals, " ")],
                ..BrowseSpec::default()
            },
            500,
        );
        assert_eq!(
            missing,
            Err(QueryError::Unsupported(
                "The filter on \"status\" needs a value".into()
            ))
        );
    }

    #[test]
    fn projects_and_reads_everything_without_a_limit() {
        let spec = BrowseSpec {
            projection: Some("{number: 1, total: 1}".into()),
            filter: Some("  ".into()),
            ..BrowseSpec::default()
        };
        let command = page(spec.clone());
        assert_eq!(
            command,
            doc! {
                "find": "orders",
                "filter": {},
                "sort": { "_id": 1 },
                "projection": { "number": 1, "total": 1 },
                "batchSize": 500_i64,
            }
        );
        assert_eq!(next(&spec, 20_000, Some(Value::Int32(1))), None);
        let broken = find(
            "orders",
            &BrowseSpec {
                filter: Some("{status: }".into()),
                ..BrowseSpec::default()
            },
            500,
        );
        assert!(
            matches!(&broken, Err(QueryError::Rejected(Rejection::Parse(message))) if message.starts_with("line 1, column 10")),
            "{broken:?}"
        );
    }

    #[test]
    fn a_projection_without_id_pages_by_skipping() {
        let spec = BrowseSpec {
            projection: Some("{_id: 0}".into()),
            limit: 10,
            ..BrowseSpec::default()
        };
        assert_eq!(next(&spec, 10, None), Some(Continuation::Skip(10)));
        let second = BrowseSpec {
            after: Some(Continuation::Skip(10)),
            ..spec
        };
        assert_eq!(page(second.clone()).get_i64("skip").ok(), Some(10));
        assert_eq!(next(&second, 10, None), Some(Continuation::Skip(20)));
    }

    #[test]
    fn a_computed_id_pages_by_skipping() {
        let spec = |projection: &str| BrowseSpec {
            projection: Some(projection.into()),
            limit: 100,
            ..BrowseSpec::default()
        };
        // `_id` here is the status, not the document's key.
        let computed = spec("{_id: \"$status\", number: 1}");
        let last = Some(Value::String("pending".into()));
        assert_eq!(
            next(&computed, 100, last.clone()),
            Some(Continuation::Skip(100))
        );
        let expression = spec("{_id: {$toString: \"$_id\"}}");
        assert_eq!(next(&expression, 100, last), Some(Continuation::Skip(100)));
        // Kept as it is, `_id` still pages by key.
        for kept in [
            "{number: 1}",
            "{_id: 1, number: 1}",
            "{_id: true}",
            "{note: 0}",
        ] {
            assert_eq!(
                next(&spec(kept), 100, Some(Value::Int32(7))),
                Some(Continuation::After(Value::Int32(7))),
                "{kept}"
            );
        }
    }
}
