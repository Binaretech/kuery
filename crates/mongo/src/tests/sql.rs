//! Kuery's SQL against the dev replica set: each query's translation runs
//! and is compared with mongosh written by hand for what the SQL means,
//! and with the documents the fixture says it finds. The fixtures cover
//! what MongoDB's own operators get wrong for SQL: null and missing,
//! arrays (intermediate ones too), mixed types, NULL in IN, LIKE across
//! newlines and past ASCII, a collation, and Decimal128 against Double.

use kuery_core::driver::TranslateContext;

use super::*;

/// A scratch database the fixture is written to, dropped when it ends.
fn fixture(name: &'static str, script: &str) -> (Scratch, Mongo) {
    let scratch = Scratch::new(target(), name);
    let at = Target {
        database: name.into(),
        ..target()
    };
    let mut writer = connect(&at, Access::ReadWrite).unwrap();
    for statement in script.split(";\n").filter(|s| !s.trim().is_empty()) {
        console(&mut writer, statement).unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    (scratch, connect(&at, Access::ReadOnly).unwrap())
}

/// The documents a SQL query's translation returns.
fn sql(mongo: &mut Mongo, query: &str) -> Vec<Document> {
    let translation = crate::sql::translate(query, &TranslateContext::default())
        .unwrap_or_else(|errors| panic!("{query}: {}", errors[0].located(query)));
    console(mongo, &translation.native)
        .unwrap_or_else(|error| panic!("{query}\n{}: {error}", translation.native))
        .0
}

fn mongosh(mongo: &mut Mongo, statement: &str) -> Vec<Document> {
    console(mongo, statement)
        .unwrap_or_else(|error| panic!("{statement}: {error}"))
        .0
}

/// One field of each document, `None` where it is missing.
fn column(documents: &[Document], field: &str) -> Vec<Option<Value>> {
    documents
        .iter()
        .map(|document| document.get(field).cloned())
        .collect()
}

fn ids(numbers: &[i32]) -> Vec<Option<Value>> {
    numbers.iter().map(|n| Some(Value::Int32(*n))).collect()
}

const ITEMS: &str = r#"db.items.insertMany([
    {_id: 1, a: 5, s: "abc"},
    {_id: 2, a: null},
    {_id: 3},
    {_id: 4, a: [5, 6]},
    {_id: 5, a: "5"},
    {_id: 6, a: 5.0},
    {_id: 7, a: NumberLong(5)},
    {_id: 8, a: NumberDecimal("5")},
    {_id: 9, a: 7},
    {_id: 10, b: {c: 1}},
    {_id: 11, b: [{c: 1}, {c: 2}]},
    {_id: 12, b: {c: null}},
    {_id: 13, b: 5},
    {_id: 14, b: {c: [1]}},
    {_id: 15, s: "line1\nline2"},
    {_id: 16, s: "ÉCOLE"},
    {_id: 17, s: "école"},
    {_id: 18, s: "a%b"},
    {_id: 19, p: NumberDecimal("9.99")},
    {_id: 20, p: 9.99},
    {_id: 21, s: "Ab"},
    {_id: 22, b: {c: 2}},
    {_id: 23, s: ["abc"]},
    {_id: 24, tags: ["x", "y"]},
    {_id: 25, tags: null},
    {_id: 26},
    {_id: 27, tags: []},
    {_id: 28, tags: "s"},
    {_id: 29, tags: [1, null]}
])"#;

/// SQL, the mongosh that finds the same documents written by hand, and the
/// `_id`s both find, in order.
const WHERE: &[(&str, &str, &[i32])] = &[
    // Numbers compare across their types; arrays and text do not match.
    ("a = 5", "{a: {$eq: 5, $not: {$type: 4}}}", &[1, 6, 7, 8]),
    // `<>` takes in neither null, missing, nor arrays.
    (
        "a <> 5",
        "{$and: [{a: {$exists: true, $ne: null}}, {a: {$not: {$type: 4}}}, {a: {$not: {$eq: 5}}}]}",
        &[5, 9],
    ),
    // Null and missing are both NULL; an array is neither.
    (
        "a IS NULL AND _id <= 9",
        "{_id: {$lte: 9}, $or: [{a: null, 'a.0': {$exists: false}}, {a: {$exists: false}}]}",
        &[2, 3],
    ),
    (
        "a IS NOT NULL AND _id <= 9",
        "{_id: {$lte: 9}, a: {$exists: true, $ne: null}}",
        &[1, 4, 5, 6, 7, 8, 9],
    ),
    // IS NULL is false for any array, empty or holding a null.
    (
        "tags IS NOT NULL AND _id >= 24",
        "{_id: {$gte: 24}, $or: [{tags: {$type: 'array'}}, {tags: {$exists: true, $not: {$type: 'null'}}}]}",
        &[24, 27, 28, 29],
    ),
    (
        "tags IS NULL AND _id >= 24",
        "{_id: {$gte: 24}, tags: {$not: {$type: 4}}, $or: [{tags: {$exists: false}}, {tags: {$type: 'null'}}]}",
        &[25, 26],
    ),
    (
        "missing(a) AND _id <= 9",
        "{_id: {$lte: 9}, a: {$exists: false}}",
        &[3],
    ),
    // NULL in IN: dropped from IN, and NOT IN with it finds nothing.
    (
        "a IN (5, NULL) AND _id <= 9",
        "{_id: {$lte: 9}, a: {$in: [5], $not: {$type: 4}}}",
        &[1, 6, 7, 8],
    ),
    ("a NOT IN (5, NULL)", "{_id: {$exists: false}}", &[]),
    (
        "a NOT IN (7) AND _id <= 9",
        "{_id: {$lte: 9}, a: {$exists: true, $ne: null, $not: {$type: 4}}, $nor: [{a: 7}]}",
        &[1, 5, 6, 7, 8],
    ),
    // Comparisons stay within a type.
    (
        "a > 4 AND _id <= 9",
        "{_id: {$lte: 9}, a: {$type: 'number', $gt: 4, $not: {$type: 4}}}",
        &[1, 6, 7, 8, 9],
    ),
    (
        "a < 'z' AND _id <= 9",
        "{_id: {$lte: 9}, a: {$type: 'string'}}",
        &[5],
    ),
    // Paths through arrays, at the end and on the way.
    (
        "b.c = 1",
        "{$and: [{'b.c': 1}, {b: {$not: {$type: 4}}}, {'b.c': {$not: {$type: 4}}}]}",
        &[10],
    ),
    (
        "b.c IS NULL AND _id BETWEEN 10 AND 14",
        "{_id: {$gte: 10, $lte: 14}, $or: [{b: {$type: 'object'}, 'b.c': null, 'b.c.0': {$exists: false}}, {b: {$not: {$type: ['object', 'array']}}}]}",
        &[12, 13],
    ),
    (
        "b.c IS NOT NULL AND _id BETWEEN 10 AND 14",
        "{_id: {$gte: 10, $lte: 14}, b: {$type: 'object', $not: {$type: 'array'}}, 'b.c': {$exists: true, $ne: null}}",
        &[10, 14],
    ),
    (
        "NOT (b.c = 1) AND _id BETWEEN 10 AND 22",
        "{_id: {$gte: 10, $lte: 22}, b: {$type: 'object'}, 'b.c': {$exists: true, $nin: [null, 1]}}",
        &[22],
    ),
    // LIKE: anchored, across newlines, one character past ASCII.
    (
        "s LIKE 'line1%'",
        "{s: {$regex: '^line1', $type: 'string'}}",
        &[15],
    ),
    ("s LIKE '%2'", "{s: 'line1\\nline2'}", &[15]),
    ("s LIKE 'line1_line2'", "{s: 'line1\\nline2'}", &[15]),
    ("s LIKE 'line_'", "{_id: {$exists: false}}", &[]),
    ("s LIKE '_cole'", "{s: 'école'}", &[17]),
    (
        "s ILIKE 'école'",
        "{s: {$in: ['ÉCOLE', 'école']}}",
        &[16, 17],
    ),
    ("s LIKE 'a!%b' ESCAPE '!'", "{s: 'a%b'}", &[18]),
    ("s LIKE 'A%'", "{s: 'Ab'}", &[21]),
    (
        "s LIKE 'a%'",
        "{s: {$in: ['abc', 'a%b'], $not: {$type: 4}}}",
        &[1, 18],
    ),
    (
        "s NOT LIKE 'a%'",
        "{s: {$type: 'string', $nin: ['abc', 'a%b'], $not: {$type: 4}}}",
        &[15, 16, 17, 21],
    ),
    // Decimal128 and Double are different numbers for 9.99.
    ("p = DECIMAL '9.99'", "{p: NumberDecimal('9.99')}", &[19]),
    ("p = 9.99", "{p: 9.99}", &[20]),
    (
        "p > 9.98 AND p < 10",
        "{p: {$gt: 9.98, $lt: 10}}",
        &[19, 20],
    ),
];

#[test]
#[ignore = "needs the dev MongoDB"]
fn sql_finds_what_hand_written_mongosh_finds() {
    let (_scratch, mut mongo) = fixture("kuery_sql_where", ITEMS);
    for (condition, filter, expected) in WHERE {
        let query = format!("SELECT _id FROM items WHERE {condition} ORDER BY _id");
        let found = column(&sql(&mut mongo, &query), "c0");
        let by_hand = column(
            &mongosh(
                &mut mongo,
                &format!("db.items.find({filter}, {{_id: 1}}).sort({{_id: 1}})"),
            ),
            "_id",
        );
        assert_eq!(found, ids(expected), "{query}");
        assert_eq!(by_hand, ids(expected), "{filter}");
    }
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn sql_orders_and_groups_as_mongodb_does() {
    let (_scratch, mut mongo) = fixture("kuery_sql_groups", ITEMS);
    // Mixed types order as MongoDB orders them, null and missing first.
    let ordered = column(
        &sql(
            &mut mongo,
            "SELECT _id FROM items WHERE _id <= 9 ORDER BY a, _id",
        ),
        "c0",
    );
    let by_hand = column(
        &mongosh(
            &mut mongo,
            "db.items.find({_id: {$lte: 9}}, {_id: 1}).sort({a: 1, _id: 1})",
        ),
        "_id",
    );
    assert_eq!(ordered, by_hand);
    assert_eq!(ordered[..2], ids(&[2, 3]));

    // Null and missing make one group.
    let groups = sql(
        &mut mongo,
        "SELECT a, COUNT(*) AS n FROM items WHERE _id IN (2, 3, 9) GROUP BY a ORDER BY n",
    );
    assert_eq!(
        column(&groups, "c0"),
        [Some(Value::Int32(7)), Some(Value::Null)]
    );
    assert_eq!(column(&groups, "c1"), ids(&[1, 2]));

    // COUNT(a) skips null and missing; SUM without a number is NULL.
    let totals = sql(
        &mut mongo,
        "SELECT COUNT(a), SUM(a), COUNT(*), MIN(a) FROM items WHERE _id IN (2, 3, 5)",
    );
    assert_eq!(totals.len(), 1);
    assert_eq!(
        totals[0],
        Document(vec![
            ("c0".into(), Value::Int32(1)),
            ("c1".into(), Value::Null),
            ("c2".into(), Value::Int32(3)),
            ("c3".into(), Value::String("5".into())),
        ])
    );
    // No documents still make one row.
    let none = sql(
        &mut mongo,
        "SELECT COUNT(*), SUM(a), AVG(a) FROM items WHERE _id > 1000",
    );
    assert_eq!(
        none,
        [Document(vec![
            ("c0".into(), Value::Int32(0)),
            ("c1".into(), Value::Null),
            ("c2".into(), Value::Null),
        ])]
    );

    // HAVING over groups, against the pipeline written by hand.
    let having = sql(
        &mut mongo,
        "SELECT a, COUNT(*) FROM items WHERE _id <= 9 GROUP BY a HAVING COUNT(*) >= 2 ORDER BY 2",
    );
    let by_hand = mongosh(
        &mut mongo,
        "db.items.aggregate([{$match: {_id: {$lte: 9}}}, {$group: {_id: {$ifNull: ['$a', null]}, n: {$sum: 1}}}, {$match: {n: {$gte: 2}}}, {$sort: {n: 1}}])",
    );
    assert_eq!(column(&having, "c1"), column(&by_hand, "n"));
    assert_eq!(column(&having, "c1"), ids(&[2, 4]));
    let sums = sql(&mut mongo, "SELECT SUM(a), AVG(p), MAX(p) FROM items");
    let by_hand = mongosh(
        &mut mongo,
        "db.items.aggregate([{$group: {_id: null, s: {$sum: '$a'}, avg: {$avg: '$p'}, max: {$max: '$p'}}}])",
    );
    assert_eq!(column(&sums, "c0"), column(&by_hand, "s"));
    assert_eq!(column(&sums, "c1"), column(&by_hand, "avg"));
    assert_eq!(column(&sums, "c2"), column(&by_hand, "max"));
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn sql_compares_and_orders_by_the_collections_collation() {
    let (_scratch, mut mongo) = fixture(
        "kuery_sql_collation",
        r#"db.createCollection("people", {collation: {locale: "fr", strength: 1}});
db.people.insertMany([{_id: 1, name: "école"}, {_id: 2, name: "ECOLE"}, {_id: 3, name: "Ecole"}, {_id: 4, name: "zèbre"}, {_id: 5, name: "abeille"}])"#,
    );
    let equal = column(
        &sql(
            &mut mongo,
            "SELECT _id FROM people WHERE name = 'ecole' ORDER BY _id",
        ),
        "c0",
    );
    let by_hand = column(
        &mongosh(&mut mongo, "db.people.find({name: 'ecole'}).sort({_id: 1})"),
        "_id",
    );
    assert_eq!(equal, ids(&[1, 2, 3]));
    assert_eq!(equal, by_hand);
    // LIKE does not follow the collation.
    let like = column(
        &sql(&mut mongo, "SELECT _id FROM people WHERE name LIKE 'ecole'"),
        "c0",
    );
    assert!(like.is_empty(), "{like:?}");
    let ordered = column(
        &sql(&mut mongo, "SELECT _id FROM people ORDER BY name, _id"),
        "c0",
    );
    let by_hand = column(
        &mongosh(&mut mongo, "db.people.find().sort({name: 1, _id: 1})"),
        "_id",
    );
    assert_eq!(ordered, by_hand);
    assert_eq!(ordered, ids(&[5, 1, 2, 3, 4]));
}

#[test]
#[ignore = "needs the dev MongoDB"]
fn sql_reads_the_sample_shop_on_a_read_only_connection() {
    let mut mongo = connect(&target(), Access::ReadOnly).unwrap();
    for query in [
        "SELECT * FROM orders LIMIT 3",
        "SELECT status, COUNT(*) AS n FROM orders GROUP BY status ORDER BY n DESC",
        "SELECT COUNT(*) FROM orders",
    ] {
        assert!(!sql(&mut mongo, query).is_empty(), "{query}");
    }
    let translation =
        crate::sql::translate("SELECT COUNT(*) FROM orders", &TranslateContext::default()).unwrap();
    let estimate = translation.estimate.unwrap();
    let (documents, _) = console(&mut mongo, &estimate.native).unwrap();
    assert!(count_of(&documents) > 0);
}
