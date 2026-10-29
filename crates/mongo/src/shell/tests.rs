use kuery_core::document::language::CompletionContext;
use kuery_core::document::value::{CollectionRef, Document, Value};
use proptest::prelude::*;

use super::*;
use crate::literal::parse_value;

fn one(text: &str) -> Op {
    let mut statements = statements(text).unwrap_or_else(|error| {
        panic!("{text}: {}", error.located(text));
    });
    assert_eq!(statements.len(), 1, "{text}: {statements:?}");
    statements.remove(0).1
}

fn refused(text: &str) -> (String, std::ops::Range<usize>) {
    let error = statements(text).expect_err(text);
    (error.message, error.span)
}

fn value(text: &str) -> Value {
    parse_value(text).unwrap()
}

fn on(collection: &str) -> CollectionRef {
    CollectionRef {
        path: vec![collection.into()],
    }
}

#[test]
fn reads_a_find_with_its_cursor() {
    assert_eq!(
        one(r#"db.orders.find({status: "paid"}).sort({_id: -1}).limit(5).pretty()"#),
        Op::Collection {
            target: on("orders"),
            method: Method::Find,
            args: vec![value(r#"{status: "paid"}"#)],
            chain: vec![
                (Cursor::Sort, vec![value("{_id: -1}")]),
                (Cursor::Limit, vec![Value::Int32(5)]),
            ],
        }
    );
}

#[test]
fn names_collections_every_way_mongosh_does() {
    let find = |target: CollectionRef| Op::Collection {
        target,
        method: Method::Find,
        args: vec![],
        chain: vec![],
    };
    assert_eq!(one("db.orders.find()"), find(on("orders")));
    assert_eq!(one("db.system.views.find()"), find(on("system.views")));
    assert_eq!(one(r#"db.getCollection("a-b").find()"#), find(on("a-b")));
    assert_eq!(one(r#"db["a b"].find()"#), find(on("a b")));
    assert_eq!(
        one(r#"db.getSiblingDB("other").orders.find()"#),
        find(CollectionRef {
            path: vec!["other".into(), "orders".into()]
        })
    );
    // A collection named like a database method, when it is not called.
    assert_eq!(one("db.stats.find()"), find(on("stats")));
}

#[test]
fn reads_use_show_database_methods_and_commands() {
    assert_eq!(one("use shop"), Op::Use("shop".into()));
    assert_eq!(one("use my-shop_2"), Op::Use("my-shop_2".into()));
    assert_eq!(one("use 'shop'"), Op::Use("shop".into()));
    assert_eq!(one("show dbs"), Op::Show(Show::Databases));
    assert_eq!(one("show databases"), Op::Show(Show::Databases));
    assert_eq!(one("show collections"), Op::Show(Show::Collections));
    assert_eq!(
        one(r#"db.createCollection("logs", {capped: true, size: 4096})"#),
        Op::Database {
            database: None,
            method: DbMethod::CreateCollection,
            args: vec![
                Value::String("logs".into()),
                value("{capped: true, size: 4096}")
            ],
        }
    );
    assert_eq!(
        one("db.runCommand({ping: 1})"),
        Op::RunCommand {
            database: None,
            admin: false,
            command: Document(vec![("ping".into(), Value::Int32(1))]),
        }
    );
    // The short form names the command.
    assert_eq!(
        one(r#"db.adminCommand("ping")"#),
        one("db.adminCommand({ping: 1})")
    );
    assert_eq!(
        one(r#"db.getSiblingDB("admin").runCommand({buildInfo: 1})"#),
        Op::RunCommand {
            database: Some("admin".into()),
            admin: false,
            command: Document(vec![("buildInfo".into(), Value::Int32(1))]),
        }
    );
}

#[test]
fn reads_explain_before_and_after_the_method() {
    let explained = Op::Collection {
        target: on("orders"),
        method: Method::Aggregate,
        args: vec![value("[{$match: {}}]")],
        chain: vec![(
            Cursor::Explain,
            vec![Value::String("executionStats".into())],
        )],
    };
    assert_eq!(
        one(r#"db.orders.explain("executionStats").aggregate([{$match: {}}])"#),
        explained
    );
    assert_eq!(
        one(r#"db.orders.aggregate([{$match: {}}]).explain("executionStats")"#),
        explained
    );
}

#[test]
fn splits_statements_at_semicolons_and_line_ends() {
    let script = "use shop; show collections\n// a comment\ndb.orders.find()\n  .sort({_id: 1})\n\ndb.orders.find({\n  status: 'paid'\n})\n;;";
    let read = statements(script).unwrap();
    let texts: Vec<&str> = read.iter().map(|(span, _)| &script[span.clone()]).collect();
    assert_eq!(
        texts,
        [
            "use shop",
            "show collections",
            "db.orders.find()\n  .sort({_id: 1})",
            "db.orders.find({\n  status: 'paid'\n})",
        ]
    );
    assert_eq!(statements("  // nothing\n ; ").unwrap(), vec![]);
}

#[test]
fn refuses_javascript_with_one_message() {
    for script in [
        "const x = db.orders.find()",
        "let n = 1",
        "var a",
        "x = 5",
        "function f() { return 1 }",
        "db.orders.find({$where: function () { return true }})",
        "db.orders.find().forEach(d => print(d))",
        "for (let i = 0; i < 3; i++) {}",
        "while (true) {}",
        r#"load("script.js")"#,
        r#"db.eval("1")"#,
        "db.orders.mapReduce(m, r)",
        r#"Mongo("localhost")"#,
        r#"new Mongo("localhost")"#,
        "print(1)",
        "db.orders.find() db.orders.find()",
        "{a: 1}",
    ] {
        assert_eq!(refused(script).0, JAVASCRIPT, "{script}");
    }
    // Where it starts.
    assert_eq!(refused("db.a.find(); let x = 1").1, 13..16);
}

#[test]
fn names_what_it_does_not_run() {
    assert_eq!(
        refused("db.orders.frobnicate()").0,
        "frobnicate is not a collection method Kuery runs"
    );
    assert_eq!(
        refused("db.orders.find().itcount()"),
        (
            ".itcount() is not a cursor method Kuery runs".into(),
            17..24
        )
    );
    assert_eq!(
        refused("db.frobnicate()").0,
        "db.frobnicate() is not a command Kuery runs"
    );
    assert_eq!(
        refused("show users").0,
        "Kuery shows dbs and collections, not users"
    );
    assert_eq!(
        refused("db.orders.countDocuments().sort({a: 1})").0,
        "countDocuments takes no .sort(): pass it as an option"
    );
    assert_eq!(
        refused("db.orders.insertOne({}).explain()").0,
        "insertOne cannot be explained"
    );
    assert_eq!(
        refused("db.orders.find().explain().limit(1)").0,
        "explain() comes last"
    );
}

#[test]
fn checks_arguments_with_their_spans() {
    let text = "db.orders.find(5)";
    assert_eq!(
        refused(text),
        (
            "find()'s first argument is a document, such as {status: \"paid\"}".into(),
            15..16
        )
    );
    assert_eq!(
        refused("db.orders.updateOne({})"),
        ("updateOne() needs 2 arguments".into(), 10..23)
    );
    assert_eq!(
        refused("db.orders.getIndexes(1)"),
        ("getIndexes() takes no arguments".into(), 21..22)
    );
    assert_eq!(
        refused("db.orders.find({a: })"),
        ("Expected a value".into(), 19..20)
    );
    let unfinished = "db.orders.find({a: 1}";
    let error = statements(unfinished).unwrap_err();
    assert_eq!(
        error.located(unfinished),
        "line 1, column 15: Expected ) to close the call"
    );
    assert_eq!(
        refused("db.orders").0,
        "Call a method of the collection, such as db.orders.find()"
    );
    assert_eq!(refused("use").0, "Name the database: use shop");
    assert_eq!(refused("use a b").0, "a b is not a database name");
}

#[test]
fn enters_every_database_the_server_allows() {
    for name in [
        "shop",
        "my-shop_2",
        "kr(x)",
        "kr;x",
        "kr,x",
        "kr'x",
        "kr:x",
        "kr*x",
        "kr{x}",
        "kr[x]",
        "kr<x>|?",
        "kr\tx",
        "café",
    ] {
        let entered = (crate::LANGUAGE.enter)(&[name.to_string()]).unwrap();
        assert_eq!(one(&entered), Op::Use(name.into()), "{entered}");
    }
    // What the server refuses in a database's name, even in quotes.
    let long = "x".repeat(64);
    for name in [
        "a/b",
        "a\\b",
        "a.b",
        "a b",
        "a\"b",
        "a$b",
        "a\0b",
        long.as_str(),
    ] {
        let text = print(&Op::Use(name.into()));
        assert!(statements(&text).is_err(), "{text}");
    }
    // Unquoted, the name is the text as written: code-like punctuation
    // needs the quotes.
    assert_eq!(refused("use kr(x)").0, "kr(x) is not a database name");
}

#[test]
fn prints_what_reads_back() {
    for text in [
        r#"db.orders.find({status: "paid"}, {total: 1}).sort({_id: -1}).limit(5).explain("queryPlanner")"#,
        r#"db.getCollection("a.b").countDocuments({})"#,
        r#"db.getSiblingDB("admin").adminCommand({ping: 1})"#,
        r#"db.createView("paid", "orders", [{$match: {status: "paid"}}])"#,
        "use shop",
        "show dbs",
    ] {
        assert_eq!(one(&print(&one(text))), one(text), "{text}");
    }
    assert_eq!(
        print(&one("db.orders.find({a: 1.0}).pretty()")),
        "db.orders.find({a: 1.0})"
    );
}

#[test]
fn completes_by_position() {
    let at = |text: &str| context(text, text.len());
    assert_eq!(
        at("db.or"),
        CompletionContext::Collection {
            prefix: "or".into()
        }
    );
    assert_eq!(
        at("db.find({}); db."),
        CompletionContext::Collection {
            prefix: String::new()
        }
    );
    assert_eq!(
        at(r#"db.getSiblingDB("x")."#),
        CompletionContext::Collection {
            prefix: String::new()
        }
    );
    assert_eq!(
        at("db.orders.fi"),
        CompletionContext::Method {
            collection: "orders".into(),
            prefix: "fi".into()
        }
    );
    assert_eq!(
        at(r#"db.getCollection("a.b")."#),
        CompletionContext::Method {
            collection: "a.b".into(),
            prefix: String::new()
        }
    );
    assert_eq!(
        at("db.orders.find({status: {$i"),
        CompletionContext::Operator {
            prefix: "$i".into()
        }
    );
    assert_eq!(
        at("db.orders.aggregate([{$ma"),
        CompletionContext::Stage {
            prefix: "$ma".into()
        }
    );
    assert_eq!(
        at("db.orders.aggregate([{$match: {}}, {$lookup: {from: 'c', as: 'c', pipeline: [{$pro"),
        CompletionContext::Stage {
            prefix: "$pro".into()
        }
    );
    assert_eq!(
        at("db.orders.aggregate([{$match: {$an"),
        CompletionContext::Operator {
            prefix: "$an".into()
        }
    );
    assert_eq!(
        at("db.orders.find({status: 'paid', to"),
        CompletionContext::Field {
            collection: Some("orders".into()),
            prefix: "to".into()
        }
    );
    assert_eq!(
        at("db.getSiblingDB('x').getCollection('a.b').find({"),
        CompletionContext::Field {
            collection: Some("a.b".into()),
            prefix: String::new()
        }
    );
    // In a value, a string or a comment: nothing.
    for text in [
        "db.orders.find({status: ",
        "db.orders.find({status: 'pa",
        "// db.",
        "db.orders.find()",
        "show ",
    ] {
        assert_eq!(at(text), CompletionContext::Nothing, "{text}");
    }
    // The cursor inside the text, and inside a character.
    let text = "db.orders.find({}) // é";
    assert_eq!(
        context(text, 5),
        CompletionContext::Collection {
            prefix: "or".into()
        }
    );
    assert_eq!(context(text, text.len() - 1), CompletionContext::Nothing);
}

// The property: every operation prints as a statement that reads back as
// it.

fn name() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z][a-zA-Z0-9_]{0,10}",
        "[a-z]{1,4}\\.[a-z]{1,4}",
        "[a-z][a-z -]{0,8}[a-z]",
    ]
}

/// Values that compare equal to themselves (no `NaN`).
fn plain_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i32>().prop_map(Value::Int32),
        any::<i64>().prop_map(Value::Int64),
        (-1e12f64..1e12).prop_map(Value::Double),
        any::<String>().prop_map(Value::String),
        any::<i64>().prop_map(Value::DateTime),
        any::<[u8; 12]>().prop_map(|bytes| crate::convert::value(&bson::Bson::ObjectId(
            bson::oid::ObjectId::from_bytes(bytes)
        ))),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            document(inner).prop_map(Value::Object),
        ]
    })
}

fn document(inner: impl Strategy<Value = Value>) -> impl Strategy<Value = Document> {
    proptest::collection::vec(("[a-z_][a-zA-Z0-9_]{0,6}|\\$[a-z]{1,6}", inner), 0..4).prop_map(
        |fields| {
            let mut unique: Vec<(String, Value)> = Vec::new();
            for (name, value) in fields {
                if !unique.iter().any(|(seen, _)| *seen == name) {
                    unique.push((name, value));
                }
            }
            // A lone `$`-field could read as an Extended JSON wrapper.
            if unique.len() == 1 && unique[0].0.starts_with('$') {
                unique.push(("z".into(), Value::Null));
            }
            Document(unique)
        },
    )
}

fn argument(arg: Arg) -> BoxedStrategy<Value> {
    match arg {
        Arg::Document => document(plain_value()).prop_map(Value::Object).boxed(),
        Arg::Array => proptest::collection::vec(document(plain_value()), 0..3)
            .prop_map(|items| Value::Array(items.into_iter().map(Value::Object).collect()))
            .boxed(),
        Arg::String => name().prop_map(Value::String).boxed(),
        Arg::Number => prop_oneof![
            any::<i32>().prop_map(Value::Int32),
            any::<i64>().prop_map(Value::Int64)
        ]
        .boxed(),
        Arg::Any => plain_value().boxed(),
    }
}

fn arguments((needed, shape): (usize, &'static [Arg])) -> BoxedStrategy<Vec<Value>> {
    (needed..=shape.len())
        .prop_flat_map(move |n| {
            shape[..n]
                .iter()
                .map(|arg| argument(*arg))
                .collect::<Vec<_>>()
        })
        .boxed()
}

fn collection_op() -> impl Strategy<Value = Op> {
    let target =
        (proptest::option::of(name()), name()).prop_map(|(database, collection)| CollectionRef {
            path: database.into_iter().chain([collection]).collect(),
        });
    let method = proptest::sample::select(Method::ALL);
    (target, method).prop_flat_map(|(target, method)| {
        let cursors: Vec<Cursor> = match method.has_cursor() {
            true => Cursor::ALL
                .iter()
                .copied()
                .filter(|cursor| *cursor != Cursor::Explain)
                .collect(),
            false => Vec::new(),
        };
        let chain = proptest::collection::vec(
            proptest::sample::select(match cursors.is_empty() {
                true => vec![Cursor::Sort],
                false => cursors,
            })
            .prop_flat_map(|cursor| arguments(cursor.args()).prop_map(move |args| (cursor, args))),
            match method.has_cursor() {
                true => 0..4,
                false => 0..1,
            },
        );
        let explain = match method.explainable() {
            true => proptest::option::of(arguments(Cursor::Explain.args())).boxed(),
            false => Just(None).boxed(),
        };
        (arguments(method.args()), chain, explain).prop_map(move |(args, mut chain, explain)| {
            chain.extend(explain.map(|args| (Cursor::Explain, args)));
            Op::Collection {
                target: target.clone(),
                method,
                args,
                chain,
            }
        })
    })
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        "[a-zA-Z0-9_-]{1,12}|[a-z][a-z!#%&(){}\\[\\];,':*<>|?]{0,6}".prop_map(Op::Use),
        Just(Op::Show(Show::Databases)),
        Just(Op::Show(Show::Collections)),
        collection_op(),
        (
            proptest::option::of(name()),
            proptest::sample::select(DbMethod::ALL)
        )
            .prop_flat_map(|(database, method)| arguments(method.args()).prop_map(
                move |args| {
                    Op::Database {
                        database: database.clone(),
                        method,
                        args,
                    }
                }
            )),
        (
            proptest::option::of(name()),
            any::<bool>(),
            document(plain_value())
        )
            .prop_map(|(database, admin, mut command)| {
                command.0.retain(|(name, _)| name != "ping");
                command.0.insert(0, ("ping".into(), Value::Int32(1)));
                Op::RunCommand {
                    database,
                    admin,
                    command,
                }
            }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    #[test]
    fn printing_and_reading_back_gives_the_same_operation(op in op()) {
        let text = print(&op);
        let read = statements(&text)
            .map_err(|error| TestCaseError::fail(format!("{text}: {}", error.located(&text))))?;
        prop_assert_eq!(read, vec![(0..text.len(), op)], "{}", text);
    }
}
