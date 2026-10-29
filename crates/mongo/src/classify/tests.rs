use kuery_core::analysis::StatementKind::{self, *};

use super::*;

fn info(text: &str) -> OpInfo {
    let statements = analyze(text).unwrap_or_else(|error| panic!("{text}: {error}"));
    assert_eq!(statements.len(), 1, "{text}");
    statements.into_iter().next().unwrap().info
}

fn kind(text: &str) -> StatementKind {
    info(text).kind
}

#[test]
fn classifies_every_collection_method() {
    for (text, expected) in [
        ("db.orders.find({})", Read),
        ("db.orders.findOne({_id: 1})", Read),
        (
            "db.orders.aggregate([{$match: {}}, {$group: {_id: '$status'}}])",
            Read,
        ),
        ("db.orders.countDocuments({})", Read),
        ("db.orders.estimatedDocumentCount()", Read),
        ("db.orders.distinct('status')", Read),
        ("db.orders.getIndexes()", Read),
        ("db.orders.stats()", Read),
        ("db.orders.insertOne({a: 1})", Data),
        ("db.orders.insertMany([{a: 1}])", Data),
        ("db.orders.updateOne({_id: 1}, {$set: {a: 1}})", Data),
        ("db.orders.updateMany({a: 1}, {$set: {a: 2}})", Data),
        ("db.orders.replaceOne({_id: 1}, {a: 1})", Data),
        ("db.orders.deleteOne({_id: 1})", Data),
        ("db.orders.deleteMany({a: 1})", Data),
        ("db.orders.findOneAndUpdate({_id: 1}, {$set: {a: 1}})", Data),
        ("db.orders.findOneAndReplace({_id: 1}, {a: 1})", Data),
        ("db.orders.findOneAndDelete({_id: 1})", Data),
        ("db.orders.bulkWrite([{insertOne: {document: {}}}])", Data),
        ("db.orders.createIndex({a: 1})", Schema),
        ("db.orders.createIndexes([{a: 1}])", Schema),
        ("db.orders.dropIndex('a_1')", Schema),
        ("db.orders.dropIndexes()", Schema),
        ("db.orders.drop()", Schema),
        ("db.orders.renameCollection('old_orders')", Schema),
        ("db.createCollection('logs')", Schema),
        ("db.createView('paid', 'orders', [])", Schema),
        ("db.dropDatabase()", Schema),
        ("db.stats()", Read),
        ("db.getCollectionNames()", Read),
        ("use shop", Read),
        ("show dbs", Read),
        ("show collections", Read),
    ] {
        assert_eq!(kind(text), expected, "{text}");
    }
}

#[test]
fn finds_writing_stages_at_any_depth() {
    for text in [
        "db.orders.aggregate([{$out: 'copy'}])",
        "db.orders.aggregate([{$merge: {into: 'copy'}}])",
        "db.orders.aggregate([{$facet: {a: [{$match: {}}], b: [{$out: 'copy'}]}}])",
        "db.orders.aggregate([{$lookup: {from: 'c', as: 'c', pipeline: [{$merge: 'copy'}]}}])",
        "db.orders.aggregate([{$unionWith: {coll: 'c', pipeline: [{$out: 'copy'}]}}])",
        // Explained, it is still the write it explains.
        "db.orders.explain().aggregate([{$out: 'copy'}])",
        "db.orders.aggregate([{$out: 'copy'}]).explain('executionStats')",
    ] {
        assert_eq!(kind(text), Data, "{text}");
    }
    assert_eq!(kind("db.orders.find().explain()"), Read);
    assert_eq!(kind("db.orders.explain().deleteMany({a: 1})"), Data);
}

#[test]
fn reads_commands_only_from_the_allowlist() {
    for name in READ_COMMANDS {
        assert_eq!(
            kind(&format!("db.runCommand({{{name}: 1}})")),
            Read,
            "{name}"
        );
    }
    assert_eq!(kind("db.adminCommand({ping: 1})"), Read);
    for (text, expected) in [
        ("db.runCommand({insert: 'orders', documents: [{}]})", Data),
        (
            "db.runCommand({findAndModify: 'orders', query: {}, remove: true})",
            Data,
        ),
        ("db.runCommand({drop: 'orders'})", Schema),
        ("db.runCommand({collMod: 'orders'})", Schema),
        // Not in the list: a write, even when it would only read.
        ("db.runCommand({find: 'orders'})", Other),
        ("db.adminCommand({shutdown: 1})", Other),
        ("db.runCommand({someFutureCommand: 1})", Other),
        ("db.runCommand({Ping: 1})", Other),
    ] {
        assert_eq!(kind(text), expected, "{text}");
    }
}

#[test]
fn names_the_dangers() {
    let dangers = |text: &str| -> Vec<String> {
        info(text)
            .dangers
            .into_iter()
            .map(|danger| danger.0)
            .collect()
    };
    assert_eq!(
        dangers("db.orders.deleteMany({})"),
        ["deleteMany with an empty filter removes every document in orders"]
    );
    assert_eq!(
        dangers("db.orders.updateMany({}, {$set: {a: 1}})"),
        ["updateMany with an empty filter changes every document in orders"]
    );
    assert!(dangers("db.orders.deleteMany({a: 1})").is_empty());
    assert!(dangers("db.orders.updateMany({a: 1}, {$set: {a: 1}})").is_empty());
    assert_eq!(dangers("db.orders.drop()").len(), 1);
    assert_eq!(dangers("db.dropDatabase()").len(), 1);
    assert_eq!(dangers("db.orders.dropIndexes()").len(), 1);
    assert_eq!(dangers("db.orders.dropIndexes('*')").len(), 1);
    assert!(dangers("db.orders.dropIndexes('a_1')").is_empty());
    assert_eq!(
        dangers("db.orders.renameCollection('old', true)"),
        ["renameCollection with dropTarget first drops old"]
    );
    assert!(dangers("db.orders.renameCollection('old')").is_empty());
    assert_eq!(dangers("db.orders.replaceOne({sku: 1}, {a: 1})").len(), 1);
    assert!(dangers("db.orders.replaceOne({_id: 1}, {a: 1})").is_empty());
    assert_eq!(
        dangers("db.runCommand({createUser: 'x', pwd: 'y', roles: []})"),
        ["createUser changes who may do what"]
    );
}

#[test]
fn marks_heavy_work() {
    for text in [
        "db.orders.find({$where: 'sleep(10) || true'})",
        "db.orders.aggregate([{$group: {_id: 1, x: {$accumulator: {}}}}])",
        "db.orders.find({$expr: {$function: {body: 'x', args: [], lang: 'js'}}})",
        "db.orders.find().sort({a: 1}).hint({$where: 1})",
        "db.orders.createIndex({a: 1})",
        "db.runCommand({validate: 'orders'})",
        "db.runCommand({compact: 'orders'})",
    ] {
        assert!(info(text).heavy, "{text}");
    }
    // A $lookup is not heavy on its own.
    assert!(!info("db.orders.aggregate([{$lookup: {from: 'c', localField: 'a', foreignField: 'b', as: 'c'}}])").heavy);
}

#[test]
fn says_what_it_works_on() {
    let statements =
        analyze("db.orders.find()\nuse other\ndb.orders.find()\ndb.getSiblingDB('third').x.find()")
            .unwrap();
    let targets: Vec<Option<Vec<String>>> = statements
        .iter()
        .map(|statement| statement.info.target.clone().map(|target| target.path))
        .collect();
    assert_eq!(
        targets,
        [
            Some(vec!["orders".to_string()]),
            None,
            Some(vec!["other".to_string(), "orders".to_string()]),
            Some(vec!["third".to_string(), "x".to_string()]),
        ]
    );
    let summaries: Vec<&str> = statements
        .iter()
        .map(|statement| statement.info.summary.as_str())
        .collect();
    assert_eq!(
        summaries,
        ["find orders", "use other", "find orders", "find third.x"]
    );
    assert_eq!(
        info("db.orders.find().explain()").summary,
        "explain find orders"
    );
    assert!(info("db.orders.find()").explainable);
    assert!(info("db.orders.deleteMany({a: 1})").explainable);
    assert!(!info("db.orders.insertOne({})").explainable);
    assert!(!info("show dbs").explainable);
}

#[test]
fn nesting_too_deep_is_an_error_not_a_crash() {
    let deep = format!("db.orders.find({}", "[".repeat(100_000));
    let error = analyze(&deep).expect_err("too deep");
    assert!(error.message.contains("200 levels"), "{error}");
    let start = "db.orders.find(".len() + 200;
    assert_eq!(error.span, start..start + 1);
    let pipeline = format!(
        "db.orders.aggregate([{}{{$out: 'x'}}{}])",
        "{$facet: {a: [".repeat(60),
        "]}}".repeat(60)
    );
    assert_eq!(kind(&pipeline), Data);
}
