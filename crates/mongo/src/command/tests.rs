use bson::doc;
use kuery_core::analysis::StatementKind;
use kuery_core::session::QueryError;

use super::*;
use crate::classify::classify;
use crate::shell::statements;
use crate::wire;

fn built(text: &str) -> Built {
    let op = statements(text).unwrap().remove(0).1;
    build(&op, "shop").unwrap().expect("a command")
}

#[test]
fn builds_reads() {
    assert_eq!(
        built(r#"db.orders.find({status: "paid"}, {total: 1}).sort({_id: -1}).limit(-5).skip(2)"#),
        Built {
            database: "shop".into(),
            command: doc! {
                "find": "orders",
                "filter": { "status": "paid" },
                "projection": { "total": 1 },
                "sort": { "_id": -1 },
                "singleBatch": true,
                "limit": 5,
                "skip": 2,
            },
            reply: Reply::Cursor,
        }
    );
    assert_eq!(
        built("db.orders.findOne({_id: 1})").command,
        doc! { "find": "orders", "filter": { "_id": 1 }, "limit": 1, "singleBatch": true }
    );
    assert_eq!(
        built("db.orders.aggregate([{$match: {}}], {allowDiskUse: true, batchSize: 10})").command,
        doc! { "aggregate": "orders", "pipeline": [{ "$match": {} }], "cursor": { "batchSize": 10 }, "allowDiskUse": true }
    );
    assert_eq!(
        built("db.orders.countDocuments({status: 'paid'}, {limit: 10})"),
        Built {
            database: "shop".into(),
            command: doc! {
                "aggregate": "orders",
                "pipeline": [
                    { "$match": { "status": "paid" } },
                    { "$limit": 10 },
                    { "$group": { "_id": 1, "n": { "$sum": 1 } } },
                ],
                "cursor": {},
            },
            reply: Reply::Counted,
        }
    );
    // `$where` cannot run in `$match`.
    assert_eq!(
        built("db.orders.countDocuments({$where: 'true'})").command,
        doc! { "count": "orders", "query": { "$where": "true" } }
    );
    assert_eq!(
        built("db.orders.estimatedDocumentCount()").reply,
        Reply::Count
    );
    assert_eq!(
        built("db.orders.distinct('status', {total: {$gt: 1}})"),
        Built {
            database: "shop".into(),
            command: doc! { "distinct": "orders", "key": "status", "query": { "total": { "$gt": 1 } } },
            reply: Reply::Values("status".into()),
        }
    );
    assert_eq!(
        built("db.getSiblingDB('other').orders.getIndexes()").database,
        "other"
    );
    assert_eq!(
        built("show dbs"),
        Built {
            database: "admin".into(),
            command: doc! { "listDatabases": 1, "nameOnly": false, "authorizedDatabases": true },
            reply: Reply::Listed("databases"),
        }
    );
    assert_eq!(built("db.adminCommand({ping: 1})").database, "admin");
    assert_eq!(
        build(&statements("use other").unwrap()[0].1, "shop"),
        Ok(None)
    );
}

#[test]
fn explains_the_command_it_would_run() {
    assert_eq!(
        built("db.orders.find({a: 1}).sort({b: 1}).explain('executionStats')"),
        Built {
            database: "shop".into(),
            command: doc! {
                "explain": { "find": "orders", "filter": { "a": 1 }, "sort": { "b": 1 } },
                "verbosity": "executionStats",
            },
            reply: Reply::Answer,
        }
    );
    assert_eq!(
        built("db.orders.explain().deleteMany({a: 1})").command,
        doc! {
            "explain": { "delete": "orders", "deletes": [{ "q": { "a": 1 }, "limit": 0 }] },
            "verbosity": "queryPlanner",
        }
    );
}

#[test]
fn builds_writes() {
    assert_eq!(
        built("db.orders.updateMany({a: 1}, {$set: {b: 2}}, {upsert: true, writeConcern: {w: 1}})")
            .command,
        doc! {
            "update": "orders",
            "updates": [{ "q": { "a": 1 }, "u": { "$set": { "b": 2 } }, "multi": true, "upsert": true }],
            "writeConcern": { "w": 1 },
        }
    );
    assert_eq!(
        built("db.orders.deleteOne({a: 1})").command,
        doc! { "delete": "orders", "deletes": [{ "q": { "a": 1 }, "limit": 1 }] }
    );
    assert_eq!(
        built("db.orders.insertMany([{a: 1}, {a: 2}], {ordered: false})").command,
        doc! { "insert": "orders", "documents": [{ "a": 1 }, { "a": 2 }], "ordered": false }
    );
    assert_eq!(
        built("db.orders.findOneAndUpdate({a: 1}, {$inc: {n: 1}}, {returnDocument: 'after', projection: {n: 1}})")
            .command,
        doc! { "findAndModify": "orders", "query": { "a": 1 }, "update": { "$inc": { "n": 1 } }, "new": true, "fields": { "n": 1 } }
    );
    assert_eq!(
        built("db.orders.createIndex({status: 1, total: -1}, {unique: true})").command,
        doc! { "createIndexes": "orders", "indexes": [{ "key": { "status": 1, "total": -1 }, "name": "status_1_total_-1", "unique": true }] }
    );
    assert_eq!(
        built("db.orders.renameCollection('old', true)"),
        Built {
            database: "admin".into(),
            command: doc! { "renameCollection": "shop.orders", "to": "shop.old", "dropTarget": true },
            reply: Reply::Answer,
        }
    );
    let op = statements("db.orders.bulkWrite([])").unwrap().remove(0).1;
    assert!(matches!(
        build(&op, "shop"),
        Err(QueryError::Unsupported(_))
    ));
}

/// What the classifier calls a read the guard lets through, and what it
/// calls a write the guard stops: the two layers agree on every statement
/// the console builds.
#[test]
fn the_guard_agrees_with_the_classifier() {
    for text in [
        "show dbs",
        "show collections",
        "db.orders.find({}).sort({a: 1}).limit(1).skip(1).projection({a: 1}).hint({a: 1}).collation({locale: 'en'}).maxTimeMS(5).batchSize(2)",
        "db.orders.find().explain('executionStats')",
        "db.orders.findOne()",
        "db.orders.aggregate([{$facet: {a: [{$match: {}}]}}])",
        "db.orders.aggregate([{$match: {}}]).explain()",
        "db.orders.aggregate([], {explain: true})",
        "db.orders.countDocuments({})",
        "db.orders.countDocuments({$where: 'true'})",
        "db.orders.estimatedDocumentCount()",
        "db.orders.distinct('a')",
        "db.orders.getIndexes()",
        "db.orders.stats()",
        "db.stats()",
        "db.getCollectionNames()",
        "db.runCommand({ping: 1})",
        "db.adminCommand({hostInfo: 1})",
        "db.orders.aggregate([{$out: 'copy'}])",
        "db.orders.aggregate([{$lookup: {from: 'c', as: 'c', pipeline: [{$merge: 'x'}]}}])",
        "db.orders.aggregate([{$out: 'copy'}]).explain()",
        "db.orders.insertOne({})",
        "db.orders.insertMany([{}])",
        "db.orders.updateOne({}, {$set: {a: 1}})",
        "db.orders.updateMany({}, [{$set: {a: 1}}])",
        "db.orders.replaceOne({_id: 1}, {})",
        "db.orders.deleteOne({})",
        "db.orders.deleteMany({})",
        "db.orders.explain().updateOne({}, {$set: {a: 1}})",
        "db.orders.findOneAndUpdate({}, {$set: {a: 1}})",
        "db.orders.findOneAndReplace({}, {})",
        "db.orders.findOneAndDelete({})",
        "db.orders.createIndex({a: 1})",
        "db.orders.createIndexes([{a: 1}])",
        "db.orders.dropIndex('a_1')",
        "db.orders.dropIndexes()",
        "db.orders.drop()",
        "db.orders.renameCollection('b')",
        "db.createCollection('b')",
        "db.createView('v', 'orders', [])",
        "db.dropDatabase()",
        "db.runCommand({insert: 'orders', documents: []})",
        "db.adminCommand({shutdown: 1})",
    ] {
        let op = statements(text).unwrap().remove(0).1;
        let read = classify(&op).kind == StatementKind::Read;
        let command = build(&op, "shop").unwrap().unwrap().command;
        assert_eq!(wire::guard(&command).is_ok(), read, "{text}: {command:?}");
    }
}
