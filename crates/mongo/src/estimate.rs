//! How many documents a console write would touch, without running it, for
//! servers without transactions to run it in and undo. The server's
//! `explain` of the write counts what its filter finds and changes nothing;
//! a user who may not explain writes gets a capped count of the filter
//! instead. Both are estimates: the documents can change before the write
//! runs, and an update counts the documents that already hold its values.

use bson::{Bson, Document as BsonDocument, doc};
use kuery_core::document::adapter::DocumentSink;
use kuery_core::document::change::Count;
use kuery_core::session::QueryError;

use crate::command::{self, Built};
use crate::{Mongo, catalog, run};

/// How far a count goes before it says "at least".
const CAP: u64 = 10_000;

/// How many of the documents a write would touch the preview reads.
const SAMPLE: i64 = 20;

/// How long an explain or a count may take.
const ESTIMATE_TIME: i64 = 5_000;

/// What a write command touches, read from the command alone.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Touches {
    /// It inserts these many documents.
    Inserts(u64),
    /// It changes or removes the documents of `collection` that match
    /// `filter`: one at most when `single`. An upsert inserts one when
    /// none match.
    Matching {
        collection: String,
        filter: BsonDocument,
        single: bool,
        upsert: bool,
    },
    /// Kuery cannot tell: a schema change, an aggregation that writes,
    /// several changes in one command, or a command it does not know.
    Unknown,
}

/// What `command` touches.
pub(crate) fn touches(command: &BsonDocument) -> Touches {
    let name = run::name(command);
    let Ok(collection) = command.get_str(name).map(str::to_string) else {
        return Touches::Unknown;
    };
    /// The one statement of an `update` or a `delete`; none when there
    /// are several.
    fn only<'a>(command: &'a BsonDocument, field: &str) -> Option<&'a BsonDocument> {
        match command.get_array(field).map(Vec::as_slice) {
            Ok([Bson::Document(statement)]) => Some(statement),
            _ => None,
        }
    }
    let filter = |document: &BsonDocument, field: &str| {
        document.get_document(field).cloned().unwrap_or_default()
    };
    let flag = |document: &BsonDocument, field: &str| document.get_bool(field).unwrap_or(false);
    match name {
        "insert" => Touches::Inserts(
            command
                .get_array("documents")
                .map_or(0, |documents| documents.len() as u64),
        ),
        "update" => match only(command, "updates") {
            Some(update) => Touches::Matching {
                collection,
                filter: filter(update, "q"),
                single: !flag(update, "multi"),
                upsert: flag(update, "upsert"),
            },
            None => Touches::Unknown,
        },
        "delete" => match only(command, "deletes") {
            Some(delete) => Touches::Matching {
                collection,
                filter: filter(delete, "q"),
                single: delete.get("limit").and_then(catalog::number) == Some(1),
                upsert: false,
            },
            None => Touches::Unknown,
        },
        "findAndModify" => Touches::Matching {
            collection,
            filter: filter(command, "query"),
            single: true,
            upsert: flag(command, "upsert"),
        },
        _ => Touches::Unknown,
    }
}

/// What an explain with `executionStats` of an update, a delete or a
/// `findAndModify` counted: the documents it matched (and the one an
/// upsert would insert), or would delete. A count the time limit cut short
/// is a minimum. `None` when the answer holds no count.
pub(crate) fn explained(answer: &BsonDocument) -> Option<Count> {
    let stats = answer.get_document("executionStats").ok()?;
    let count = |key: &str| first(stats, key).and_then(catalog::number);
    let touched = match count("nWouldDelete") {
        Some(deleted) => deleted,
        None => count("nMatched")? + count("nWouldUpsert").unwrap_or(0),
    };
    Some(match stats.get_bool("executionSuccess") {
        Ok(false) => Count::AtLeast(touched),
        _ => Count::Exact(touched),
    })
}

/// The first value of `key` anywhere in `document`, nested ones too.
fn first<'a>(document: &'a BsonDocument, key: &str) -> Option<&'a Bson> {
    fn inside<'a>(value: &'a Bson, key: &str) -> Option<&'a Bson> {
        match value {
            Bson::Document(document) => first(document, key),
            Bson::Array(items) => items.iter().find_map(|item| inside(item, key)),
            _ => None,
        }
    }
    document
        .get(key)
        .or_else(|| document.values().find_map(|value| inside(value, key)))
}

/// What a capped count of the filter says the write touches.
fn capped(counted: u64, single: bool, upsert: bool) -> Count {
    match counted {
        0 if upsert => Count::Exact(1),
        n if single => Count::Exact(n.min(1)),
        n if n > CAP => Count::AtLeast(CAP),
        n => Count::Exact(n),
    }
}

/// An error that ends the estimate, rather than one way to make it.
fn fatal(error: &QueryError) -> bool {
    matches!(error, QueryError::Cancelled | QueryError::Connection(_))
}

impl Mongo {
    /// How many documents `statement` would touch (see the module), with
    /// the first of them in `sample`; `None` when Kuery cannot tell.
    pub(crate) fn estimate_statement(
        &mut self,
        statement: &str,
        sample: &mut dyn DocumentSink,
    ) -> Result<Option<Count>, QueryError> {
        let op = crate::one(statement)?;
        let Some(built) = command::build(&op, &self.database)? else {
            return Ok(None);
        };
        let (collection, filter, single, upsert) = match touches(&built.command) {
            Touches::Inserts(n) => return Ok(Some(Count::Exact(n))),
            Touches::Unknown => return Ok(None),
            Touches::Matching {
                collection,
                filter,
                single,
                upsert,
            } => (collection, filter, single, upsert),
        };
        let count = match self.explain_write(&built) {
            Ok(Some(count)) => count,
            Err(error) if fatal(&error) => return Err(error),
            // Not allowed to explain a write, or no count in the answer.
            Ok(None) | Err(_) => {
                match self.count_matching(&built.database, &collection, filter.clone(), CAP) {
                    Ok(counted) => capped(counted, single, upsert),
                    Err(QueryError::Database { code, .. }) if code == "50" => Count::Unknown,
                    Err(error) => return Err(error),
                }
            }
        };
        let limit = if single { 1 } else { SAMPLE };
        let find = doc! {
            "find": &collection,
            "filter": filter,
            "limit": limit,
            "singleBatch": true,
            "maxTimeMS": ESTIMATE_TIME,
        };
        match self.cursor_into(&built.database, find, sample) {
            Err(error) if fatal(&error) => return Err(error),
            // The count stands without its documents.
            Ok(_) | Err(_) => {}
        }
        Ok(Some(count))
    }

    /// The server's count of what a write would touch, from its explain.
    pub(crate) fn explain_write(&mut self, built: &Built) -> Result<Option<Count>, QueryError> {
        let command = doc! {
            "explain": built.command.clone(),
            "verbosity": "executionStats",
            "maxTimeMS": ESTIMATE_TIME,
        };
        let answer = self.command(&built.database, command)?;
        Ok(explained(&answer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell;

    fn command_of(statement: &str) -> BsonDocument {
        let op = shell::statements(statement).unwrap().remove(0).1;
        command::build(&op, "shop").unwrap().unwrap().command
    }

    fn matching(filter: BsonDocument, single: bool, upsert: bool) -> Touches {
        Touches::Matching {
            collection: "orders".into(),
            filter,
            single,
            upsert,
        }
    }

    #[test]
    fn reads_what_a_write_touches_from_its_command() {
        let read = |statement| touches(&command_of(statement));
        assert_eq!(
            read("db.orders.insertMany([{a: 1}, {a: 2}])"),
            Touches::Inserts(2)
        );
        assert_eq!(read("db.orders.insertOne({a: 1})"), Touches::Inserts(1));
        assert_eq!(
            read("db.orders.updateMany({status: 'paid'}, {$set: {a: 1}})"),
            matching(doc! { "status": "paid" }, false, false)
        );
        assert_eq!(
            read("db.orders.updateOne({_id: 1}, {$set: {a: 1}}, {upsert: true})"),
            matching(doc! { "_id": 1 }, true, true)
        );
        assert_eq!(
            read("db.orders.replaceOne({_id: 1}, {a: 1})"),
            matching(doc! { "_id": 1 }, true, false)
        );
        assert_eq!(
            read("db.orders.deleteMany({})"),
            matching(doc! {}, false, false)
        );
        assert_eq!(
            read("db.orders.deleteOne({a: 1})"),
            matching(doc! { "a": 1 }, true, false)
        );
        assert_eq!(
            read("db.orders.findOneAndDelete({a: 1})"),
            matching(doc! { "a": 1 }, true, false)
        );
        assert_eq!(
            read("db.orders.findOneAndUpdate({a: 1}, {$set: {b: 1}}, {upsert: true})"),
            matching(doc! { "a": 1 }, true, true)
        );
        // The commands themselves, as runCommand sends them.
        assert_eq!(
            read("db.runCommand({delete: 'orders', deletes: [{q: {a: 1}, limit: 0}]})"),
            matching(doc! { "a": 1 }, false, false)
        );
        for unknown in [
            "db.runCommand({update: 'orders', updates: [{q: {}, u: {}}, {q: {}, u: {}}]})",
            "db.orders.aggregate([{$out: 'copy'}])",
            "db.orders.drop()",
            "db.orders.createIndex({a: 1})",
            "db.dropDatabase()",
        ] {
            assert_eq!(read(unknown), Touches::Unknown, "{unknown}");
        }
    }

    #[test]
    fn reads_the_count_out_of_a_write_explained() {
        let update = doc! { "executionStats": {
            "executionSuccess": true,
            "executionStages": { "stage": "UPDATE", "nMatched": 125, "nWouldModify": 125, "nWouldUpsert": 0 },
        } };
        assert_eq!(explained(&update), Some(Count::Exact(125)));
        let upsert = doc! { "executionStats": {
            "executionStages": { "stage": "UPDATE", "nMatched": 0, "nWouldUpsert": 1 },
        } };
        assert_eq!(explained(&upsert), Some(Count::Exact(1)));
        // Cut short by its time limit: what it counted is a minimum.
        let delete = doc! { "executionStats": {
            "executionSuccess": false,
            "errorCode": 50,
            "executionStages": { "stage": "BATCHED_DELETE", "nWouldDelete": 7, "inputStage": { "stage": "COLLSCAN" } },
        } };
        assert_eq!(explained(&delete), Some(Count::AtLeast(7)));
        // A sharded cluster's, deeper.
        let sharded = doc! { "executionStats": { "executionStages": { "shards": [
            { "executionStages": { "stage": "DELETE", "nWouldDelete": 3 } },
        ] } } };
        assert_eq!(explained(&sharded), Some(Count::Exact(3)));
        assert_eq!(explained(&doc! { "queryPlanner": {} }), None);
    }

    #[test]
    fn a_capped_count_says_what_the_write_touches() {
        assert_eq!(capped(125, false, false), Count::Exact(125));
        assert_eq!(capped(125, true, false), Count::Exact(1));
        assert_eq!(capped(0, true, true), Count::Exact(1));
        assert_eq!(capped(CAP + 1, false, false), Count::AtLeast(CAP));
        assert_eq!(capped(0, false, false), Count::Exact(0));
    }
}
