//! The last check before a read-only connection sends a command: the
//! final BSON itself, whatever produced it. The session already refused
//! whatever its language did not read as a read; this check does not
//! trust that reading, so a parser that gets a command wrong still cannot
//! write through a read-only connection.

use bson::{Bson, Document};
use kuery_core::session::QueryError;

use crate::run;

/// The commands a read-only connection sends: reads, and what reads need.
const READS: &[&str] = &[
    "find",
    "aggregate",
    "count",
    "distinct",
    "listCollections",
    "listIndexes",
    "listDatabases",
    "collStats",
    "dbStats",
    "explain",
    "getMore",
    "killCursors",
    "ping",
    "hello",
    "buildInfo",
    "serverStatus",
    "connectionStatus",
    "hostInfo",
];

/// Pipeline stages that write their results to a collection.
const WRITING_STAGES: &[&str] = &["$out", "$merge"];

/// Refuses a command a read-only connection must not send: one outside
/// [`READS`], an aggregation that writes (`$out` or `$merge` at any depth,
/// in `$facet`, `$lookup` and `$unionWith` pipelines too), or an `explain`
/// of any of those.
pub(crate) fn guard(command: &Document) -> Result<(), QueryError> {
    let name = run::name(command);
    if !READS.contains(&name) {
        return Err(QueryError::WritesDisabled);
    }
    match name {
        "explain" => match command.get("explain") {
            Some(Bson::Document(inner)) => guard(inner),
            _ => Err(QueryError::WritesDisabled),
        },
        "aggregate" if command.get("pipeline").is_some_and(writes) => {
            Err(QueryError::WritesDisabled)
        }
        _ => Ok(()),
    }
}

/// Whether a value holds a writing stage anywhere in it. Searching every
/// document, not only where stages go, keeps nested pipelines (and any a
/// future server adds) covered.
fn writes(value: &Bson) -> bool {
    match value {
        Bson::Document(document) => document
            .iter()
            .any(|(name, value)| WRITING_STAGES.contains(&name.as_str()) || writes(value)),
        Bson::Array(items) => items.iter().any(writes),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use bson::doc;

    use super::*;

    fn refused(command: Document) {
        assert_eq!(
            guard(&command),
            Err(QueryError::WritesDisabled),
            "{command:?}"
        );
    }

    #[test]
    fn lets_reads_through() {
        for command in [
            doc! { "find": "orders", "filter": { "status": "paid" }, "comment": "kuery:1:1" },
            doc! { "aggregate": "orders", "pipeline": [{ "$match": {} }, { "$group": { "_id": "$status" } }], "cursor": {} },
            doc! { "aggregate": 1, "pipeline": [{ "$currentOp": {} }], "cursor": {} },
            doc! { "count": "orders" },
            doc! { "listDatabases": 1, "nameOnly": false },
            doc! { "explain": { "find": "orders" }, "verbosity": "executionStats" },
            doc! { "getMore": 7_i64, "collection": "orders" },
            doc! { "aggregate": "orders", "pipeline": [{ "$project": { "x": { "$mergeObjects": ["$a", "$b"] } } }], "cursor": {} },
        ] {
            assert_eq!(guard(&command), Ok(()), "{command:?}");
        }
    }

    #[test]
    fn refuses_writes_by_name() {
        refused(doc! { "insert": "orders", "documents": [{}] });
        refused(doc! { "update": "orders", "updates": [] });
        refused(doc! { "delete": "orders", "deletes": [] });
        refused(doc! { "findAndModify": "orders", "query": {}, "remove": true });
        refused(doc! { "drop": "orders" });
        refused(doc! { "createIndexes": "orders", "indexes": [] });
        refused(doc! { "mapReduce": "orders" });
        refused(doc! { "eval": "db.orders.drop()" });
        // Unknown counts as a write, whatever its case.
        refused(doc! { "Find": "orders" });
        refused(doc! { "someFutureCommand": 1 });
        refused(doc! {});
    }

    #[test]
    fn refuses_pipelines_that_write_at_any_depth() {
        refused(doc! { "aggregate": "orders", "pipeline": [{ "$match": {} }, { "$out": "copy" }] });
        refused(doc! { "aggregate": "orders", "pipeline": [{ "$merge": { "into": "copy" } }] });
        refused(doc! { "aggregate": "orders", "pipeline": [
            { "$facet": { "a": [{ "$match": {} }], "b": [{ "$out": "copy" }] } },
        ] });
        refused(doc! { "aggregate": "orders", "pipeline": [
            { "$lookup": { "from": "customers", "as": "c", "pipeline": [{ "$merge": "copy" }] } },
        ] });
        refused(doc! { "aggregate": "orders", "pipeline": [
            { "$unionWith": { "coll": "customers", "pipeline": [{ "$out": "copy" }] } },
        ] });
    }

    #[test]
    fn checks_what_explain_would_run() {
        refused(
            doc! { "explain": { "update": "orders", "updates": [{ "q": {}, "u": { "$set": { "a": 1 } } }] } },
        );
        refused(
            doc! { "explain": { "aggregate": "orders", "pipeline": [{ "$out": "copy" }], "cursor": {} } },
        );
        refused(doc! { "explain": { "explain": { "delete": "orders", "deletes": [] } } });
        refused(doc! { "explain": "insert" });
    }
}
