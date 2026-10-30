use kuery_core::analysis::StatementKind;
use kuery_core::document::catalog::{CollectionInfo, ContainerKind, DocIndex};
use kuery_core::document::schema::{FieldSchema, Part, Schema};
use kuery_core::document::value::{CollectionRef, Document, Value};
use kuery_core::driver::{OutputColumn, TranslateContext, Translation};
use proptest::prelude::*;

use super::lower::SELECT_TAKES;
use super::*;

fn translated(sql: &str, context: &TranslateContext) -> Translation {
    translate(sql, context).unwrap_or_else(|errors| {
        panic!("{sql}: {}", errors[0].located(sql));
    })
}

fn native(sql: &str) -> String {
    translated(sql, &TranslateContext::default()).native
}

/// The message of a refusal, and the text it points at.
fn refused(sql: &str) -> (String, String) {
    let errors = translate(sql, &TranslateContext::default())
        .err()
        .unwrap_or_else(|| panic!("{sql} translated"));
    assert_eq!(errors.len(), 1, "{sql}: {errors:?}");
    let error = &errors[0];
    (error.message.clone(), sql[error.span.clone()].to_string())
}

/// The translation table: SQL, and the mongosh it runs.
const TRANSLATIONS: &[(&str, &str)] = &[
    ("SELECT * FROM orders", "db.orders.aggregate([])"),
    (
        "SELECT name, total AS t FROM orders WHERE status = 'paid' AND total >= 100 ORDER BY total DESC LIMIT 10",
        r#"db.orders.aggregate([{$match: {status: {$not: {$type: "array"}, $eq: "paid"}, total: {$not: {$type: "array"}, $gte: 100}}}, {$sort: {total: -1}}, {$limit: 10}, {$project: {_id: 0, c0: "$name", c1: "$total"}}])"#,
    ),
    // `<>` and NOT never take in null or missing fields.
    (
        "SELECT * FROM orders WHERE a <> 5",
        r#"db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $nin: [5, null]}}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE NOT (a = 5 OR b < 2)",
        r#"db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $nin: [5, null]}, b: {$not: {$type: "array"}, $gte: 2}}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE a = 1 OR NOT b = 2",
        r#"db.orders.aggregate([{$match: {$or: [{a: {$not: {$type: "array"}, $eq: 1}}, {b: {$not: {$type: "array"}, $nin: [2, null]}}]}}])"#,
    ),
    // Every prefix of a path is guarded against arrays, but `_id`.
    (
        "SELECT * FROM orders o WHERE o.ship.city IS NULL",
        r#"db.orders.aggregate([{$match: {ship: {$not: {$type: "array"}}, "ship.city": {$not: {$type: "array"}, $eq: null}}}])"#,
    ),
    // An array is a value: IS NULL is false for it, never unknown, but
    // `{a: null}` alone would match `[1, null]`.
    (
        "SELECT * FROM orders WHERE ship.city IS NOT NULL",
        r#"db.orders.aggregate([{$match: {ship: {$not: {$type: "array"}}, $or: [{"ship.city": {$type: "array"}}, {"ship.city": {$ne: null}}]}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE tags IS NULL",
        r#"db.orders.aggregate([{$match: {tags: {$not: {$type: "array"}, $eq: null}}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE tags IS NOT NULL",
        r#"db.orders.aggregate([{$match: {$or: [{tags: {$type: "array"}}, {tags: {$ne: null}}]}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE _id IS NOT NULL",
        r#"db.orders.aggregate([{$match: {_id: {$ne: null}}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE missing(ship.city)",
        r#"db.orders.aggregate([{$match: {ship: {$not: {$type: "array"}}, "ship.city": {$exists: false}}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE NOT missing(coupon)",
        r#"db.orders.aggregate([{$match: {coupon: {$exists: true}}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE _id = ObjectId('65f0a1b2c3d4e5f601234567')",
        r#"db.orders.aggregate([{$match: {_id: {$eq: ObjectId("65f0a1b2c3d4e5f601234567")}}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE _id.region = 'eu'",
        r#"db.orders.aggregate([{$match: {"_id.region": {$not: {$type: "array"}, $eq: "eu"}}}])"#,
    ),
    // NULL in IN: dropped from IN, and NOT IN with it is never true.
    (
        "SELECT * FROM orders WHERE a IN (1, 2, NULL)",
        r#"db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $in: [1, 2]}}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE a NOT IN (1, 2)",
        r#"db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $nin: [1, 2, null]}}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE a NOT IN (1, NULL)",
        "db.orders.aggregate([{$match: {$expr: false}}])",
    ),
    (
        "SELECT * FROM orders WHERE a IN (NULL)",
        "db.orders.aggregate([{$match: {$expr: false}}])",
    ),
    (
        "SELECT * FROM orders WHERE NOT a IN (1, NULL)",
        "db.orders.aggregate([{$match: {$expr: false}}])",
    ),
    (
        "SELECT * FROM orders WHERE a BETWEEN 1 AND 5",
        r#"db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $gte: 1, $lte: 5}}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE a NOT BETWEEN 1 AND 5",
        r#"db.orders.aggregate([{$match: {a: {$not: {$type: "array"}}, $or: [{a: {$lt: 1}}, {a: {$gt: 5}}]}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE 5 < a AND -2.5 >= b",
        r#"db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $gt: 5}, b: {$not: {$type: "array"}, $lte: -2.5}}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE name LIKE 'a_%.b' ESCAPE '!'",
        r#"db.orders.aggregate([{$match: {name: {$not: {$type: "array"}, $regex: /\Aa..*\.b\z/s}}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE name LIKE '100!%/(x)' ESCAPE '!'",
        r#"db.orders.aggregate([{$match: {name: {$not: {$type: "array"}, $regex: /\A100%\/\(x\)\z/s}}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE name NOT ILIKE 'é%'",
        r#"db.orders.aggregate([{$match: {$and: [{name: {$not: {$type: "array"}, $type: "string"}}, {name: {$not: /\Aé.*\z/is}}]}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE price = DECIMAL '9.99' AND at < TIMESTAMP '2026-01-01T00:00:00Z' AND n = NumberLong(5) AND d = NumberDecimal('0.1')",
        r#"db.orders.aggregate([{$match: {price: {$not: {$type: "array"}, $eq: NumberDecimal("9.99")}, at: {$not: {$type: "array"}, $lt: ISODate("2026-01-01T00:00:00.000Z")}, n: {$not: {$type: "array"}, $eq: NumberLong("5")}, d: {$not: {$type: "array"}, $eq: NumberDecimal("0.1")}}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE a = 9223372036854775807 AND b = 1.5e3 AND c = 2.0",
        r#"db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $eq: NumberLong("9223372036854775807")}, b: {$not: {$type: "array"}, $eq: 1500.0}, c: {$not: {$type: "array"}, $eq: 2.0}}}])"#,
    ),
    // A field alone is a boolean.
    (
        "SELECT * FROM orders WHERE active AND NOT archived",
        r#"db.orders.aggregate([{$match: {active: {$not: {$type: "array"}, $eq: true}, archived: {$not: {$type: "array"}, $eq: false}}}])"#,
    ),
    ("SELECT * FROM orders WHERE TRUE", "db.orders.aggregate([])"),
    (
        "SELECT * FROM orders WHERE FALSE OR NULL",
        "db.orders.aggregate([{$match: {$expr: false}}])",
    ),
    (
        "SELECT status, COUNT(*) AS n, SUM(total), AVG(total), MIN(total), MAX(total), COUNT(coupon) FROM orders GROUP BY status HAVING COUNT(*) > 1 ORDER BY n DESC",
        r#"db.orders.aggregate([{$group: {_id: {k0: {$ifNull: ["$status", null]}}, a0: {$sum: 1}, a1: {$sum: "$total"}, n1: {$sum: {$cond: [{$isNumber: "$total"}, 1, 0]}}, a2: {$avg: "$total"}, a3: {$min: "$total"}, a4: {$max: "$total"}, a5: {$sum: {$cond: [{$in: [{$type: "$coupon"}, ["missing", "null"]]}, 0, 1]}}}}, {$project: {_id: 0, c0: "$_id.k0", c1: "$a0", c2: {$cond: [{$gt: ["$n1", 0]}, "$a1", null]}, c3: "$a2", c4: "$a3", c5: "$a4", c6: "$a5"}}, {$match: {c1: {$gt: 1}}}, {$sort: {c1: -1}}])"#,
    ),
    (
        "SELECT ship.city, status FROM orders GROUP BY status, ship.city HAVING MAX(total) >= 10 AND status <> 'void' ORDER BY 1 LIMIT 3",
        r#"db.orders.aggregate([{$group: {_id: {k0: {$ifNull: ["$status", null]}, k1: {$ifNull: ["$ship.city", null]}}, a0: {$max: "$total"}}}, {$project: {_id: 0, c0: "$_id.k1", c1: "$_id.k0", h0: "$a0"}}, {$match: {h0: {$not: {$type: "array"}, $gte: 10}, c1: {$not: {$type: "array"}, $nin: ["void", null]}}}, {$sort: {c0: 1}}, {$limit: 3}, {$project: {h0: 0}}])"#,
    ),
    // Without GROUP BY, one row even for no documents.
    (
        "SELECT COUNT(*) FROM orders",
        r#"db.orders.aggregate([{$facet: {g: [{$group: {_id: null, a0: {$sum: 1}}}]}}, {$project: {_id: 0, c0: {$ifNull: [{$first: "$g.a0"}, 0]}}}])"#,
    ),
    (
        "SELECT SUM(total), MIN(at) FROM orders WHERE status = 'paid'",
        r#"db.orders.aggregate([{$match: {status: {$not: {$type: "array"}, $eq: "paid"}}}, {$facet: {g: [{$group: {_id: null, a0: {$sum: "$total"}, n0: {$sum: {$cond: [{$isNumber: "$total"}, 1, 0]}}, a1: {$min: "$at"}}}]}}, {$project: {_id: 0, c0: {$cond: [{$gt: [{$first: "$g.n0"}, 0]}, {$first: "$g.a0"}, null]}, c1: {$ifNull: [{$first: "$g.a1"}, null]}}}])"#,
    ),
    (
        "SELECT status FROM orders GROUP BY status ORDER BY MAX(total)",
        r#"db.orders.aggregate([{$group: {_id: {k0: {$ifNull: ["$status", null]}}, a0: {$max: "$total"}}}, {$project: {_id: 0, c0: "$_id.k0", h0: "$a0"}}, {$sort: {h0: 1}}, {$project: {h0: 0}}])"#,
    ),
    (
        "SELECT 1, 'x' AS y, NULL FROM orders LIMIT 2 OFFSET 3",
        r#"db.orders.aggregate([{$skip: 3}, {$limit: 2}, {$project: {_id: 0, c0: {$literal: 1}, c1: {$literal: "x"}, c2: {$literal: null}}}])"#,
    ),
    (
        "SELECT a AS total, b FROM orders ORDER BY total, 2 DESC",
        r#"db.orders.aggregate([{$sort: {a: 1, b: -1}}, {$project: {_id: 0, c0: "$a", c1: "$b"}}])"#,
    ),
    (
        "SELECT * FROM \"order-lines\"",
        r#"db.getCollection("order-lines").aggregate([])"#,
    ),
    (
        "SELECT * FROM \"a.b\" OFFSET 0",
        r#"db.getCollection("a.b").aggregate([])"#,
    ),
    (
        "SELECT * FROM \"other\".\"orders\"",
        r#"db.getSiblingDB("other").orders.aggregate([])"#,
    ),
    // Reserved words read as fields where sqlparser lets them.
    (
        "SELECT user, \"order\", date FROM orders ORDER BY \"order\"",
        r#"db.orders.aggregate([{$sort: {order: 1}}, {$project: {_id: 0, c0: "$user", c1: "$order", c2: "$date"}}])"#,
    ),
    // Identifiers keep their case, and names need no ASCII.
    (
        "SELECT Ñame FROM orders WHERE Ñame = 'Ñ' AND ñame = 'ñ'",
        r#"db.orders.aggregate([{$match: {"Ñame": {$not: {$type: "array"}, $eq: "Ñ"}, "ñame": {$not: {$type: "array"}, $eq: "ñ"}}}, {$project: {_id: 0, c0: "$Ñame"}}])"#,
    ),
    (
        "SELECT o.* FROM orders AS o ORDER BY o.at DESC",
        r#"db.orders.aggregate([{$sort: {at: -1}}])"#,
    ),
    // Without an alias, the collection's name qualifies its fields; an
    // alias takes its place, and the name is then a field.
    (
        "SELECT c.status FROM c",
        r#"db.c.aggregate([{$project: {_id: 0, c0: "$status"}}])"#,
    ),
    (
        "SELECT orders.* FROM orders WHERE orders.status = 'paid'",
        r#"db.orders.aggregate([{$match: {status: {$not: {$type: "array"}, $eq: "paid"}}}])"#,
    ),
    (
        "SELECT orders.a FROM \"shop\".\"orders\" ORDER BY orders.a",
        r#"db.getSiblingDB("shop").orders.aggregate([{$sort: {a: 1}}, {$project: {_id: 0, c0: "$a"}}])"#,
    ),
    (
        "SELECT orders.x FROM orders o WHERE o.y = 1",
        r#"db.orders.aggregate([{$match: {y: {$not: {$type: "array"}, $eq: 1}}}, {$project: {_id: 0, c0: "$orders.x"}}])"#,
    ),
    // SQL's functions without parentheses are fields in double quotes.
    (
        "SELECT \"CURRENT_TIMESTAMP\" FROM orders",
        r#"db.orders.aggregate([{$project: {_id: 0, c0: "$CURRENT_TIMESTAMP"}}])"#,
    ),
    (
        "SELECT * FROM orders WHERE a = -9223372036854775808",
        r#"db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $eq: NumberLong("-9223372036854775808")}}}])"#,
    ),
];

#[test]
fn translates_each_query_of_the_table() {
    for (sql, expected) in TRANSLATIONS {
        assert_eq!(native(sql), *expected, "{sql}");
    }
}

#[test]
fn every_translation_reads_back_as_itself() {
    for (sql, _) in TRANSLATIONS {
        identity(&native(sql));
    }
}

/// `print(parse(native)) == native`, and the op is a read the guard lets
/// through.
fn identity(native: &str) {
    let statements = shell::statements(native).unwrap_or_else(|error| {
        panic!("{native}: {}", error.located(native));
    });
    assert_eq!(statements.len(), 1, "{native}");
    let op = &statements[0].1;
    assert_eq!(shell::print(op), native);
    let info = classify::classify(op);
    assert_eq!(info.kind, StatementKind::Read, "{native}");
    assert!(!info.heavy, "{native}");
    let built = command::build(op, "shop").unwrap().unwrap();
    assert_eq!(wire::guard(&built.command), Ok(()), "{native}");
}

#[test]
fn names_its_columns_by_alias_or_as_written() {
    let translation = translated(
        "SELECT status, COUNT(*) AS n, sum(total) FROM orders GROUP BY status",
        &TranslateContext::default(),
    );
    let column = |field: &str, label: &str| OutputColumn {
        field: field.into(),
        label: label.into(),
    };
    assert_eq!(
        translation.columns,
        [
            column("c0", "status"),
            column("c1", "n"),
            column("c2", "sum(total)"),
        ]
    );
    assert_eq!(translation.target, CollectionRef::new(&["orders"]));
    let whole = translated("SELECT * FROM orders", &TranslateContext::default());
    assert!(whole.columns.is_empty());
    let other = translated(
        "SELECT a FROM \"other\".\"orders\"",
        &TranslateContext::default(),
    );
    assert_eq!(other.target, CollectionRef::new(&["other", "orders"]));
}

/// Refusals: the message (or a part of it), and the text it points at.
const REFUSALS: &[(&str, &str, &str)] = &[
    // Writes, as statements and hidden in a query.
    (
        "UPDATE orders SET a = 1",
        READ_ONLY,
        "UPDATE orders SET a = 1",
    ),
    ("DELETE FROM orders", READ_ONLY, "DELETE FROM orders"),
    (
        "INSERT INTO orders VALUES (1)",
        READ_ONLY,
        "INSERT INTO orders VALUES (1)",
    ),
    (
        "INSERT INTO copy SELECT * FROM orders",
        READ_ONLY,
        "INSERT INTO copy SELECT * FROM orders",
    ),
    ("DROP TABLE orders", READ_ONLY, "DROP TABLE orders"),
    (
        "CREATE TABLE t (a INT)",
        READ_ONLY,
        "CREATE TABLE t (a INT)",
    ),
    (
        "SELECT a INTO copy FROM orders",
        "SELECT … INTO writes a table",
        "copy",
    ),
    (
        "SELECT * FROM orders FOR UPDATE",
        "FOR UPDATE locks rows",
        "SELECT * FROM orders FOR UPDATE",
    ),
    (
        "WITH x AS (DELETE FROM orders RETURNING *) SELECT * FROM x",
        "WITH (common table expressions) is not supported",
        "WITH x AS (DELETE FROM orders RETURNING *)",
    ),
    ("BEGIN", "there are no transactions", "BEGIN"),
    ("COMMIT", "there are no transactions", "COMMIT"),
    (
        "EXPLAIN SELECT * FROM orders",
        "Explain in the console",
        "EXPLAIN SELECT * FROM orders",
    ),
    // What v1 does not translate.
    (
        "WITH x AS (SELECT 1) SELECT * FROM x",
        "WITH (common table expressions)",
        "WITH x AS (SELECT 1)",
    ),
    (
        "SELECT * FROM a UNION SELECT * FROM b",
        "UNION, INTERSECT and EXCEPT",
        "SELECT * FROM a UNION SELECT * FROM b",
    ),
    (
        "SELECT * FROM orders o JOIN customers c ON o.c = c._id",
        "Joins are not supported",
        "customers c",
    ),
    (
        "SELECT * FROM orders, customers",
        "Joins are not supported",
        "customers",
    ),
    (
        "SELECT DISTINCT status FROM orders",
        "DISTINCT is not supported",
        "SELECT",
    ),
    (
        "SELECT COUNT(DISTINCT status) FROM orders",
        "DISTINCT is not supported",
        "COUNT(DISTINCT status)",
    ),
    (
        "SELECT * FROM (SELECT * FROM orders) x",
        "Subqueries are not supported",
        "(SELECT * FROM orders) x",
    ),
    (
        "SELECT * FROM orders WHERE a IN (SELECT b FROM c)",
        "Subqueries are not supported",
        "a IN (SELECT b FROM c)",
    ),
    (
        "SELECT * FROM orders WHERE EXISTS (SELECT 1 FROM c)",
        "Subqueries are not supported",
        "SELECT 1 FROM c",
    ),
    (
        "SELECT ROW_NUMBER() OVER () FROM orders",
        "Window functions",
        "ROW_NUMBER()",
    ),
    (
        "SELECT * FROM orders WHERE lower(name) = 'x'",
        "Functions in WHERE are not supported",
        "lower(name)",
    ),
    (
        "SELECT * FROM orders WHERE COUNT(*) > 1",
        "Aggregates go in HAVING",
        "COUNT(*)",
    ),
    (
        "SELECT * FROM orders WHERE a = b",
        "Comparing two fields is not supported",
        "a = b",
    ),
    (
        "SELECT * FROM orders WHERE a < o.b",
        "Comparing two fields is not supported",
        "a < o.b",
    ),
    (
        "SELECT * FROM orders WHERE 1 = 1",
        "Compare a field with a value",
        "1 = 1",
    ),
    (
        "SELECT * FROM orders WHERE a COLLATE \"fr\" = 'x'",
        "COLLATE is not supported",
        "a COLLATE \"fr\"",
    ),
    (
        "SELECT * FROM orders ORDER BY a NULLS LAST",
        "NULLS LAST is not supported",
        "a",
    ),
    (
        "SELECT * FROM orders ORDER BY a DESC NULLS FIRST",
        "NULLS FIRST is not supported",
        "a",
    ),
    (
        "SELECT * FROM orders ORDER BY lower(a)",
        "ORDER BY takes fields",
        "lower(a)",
    ),
    // Null is compared with IS NULL.
    (
        "SELECT * FROM orders WHERE a = NULL",
        "= NULL is never true: write IS NULL",
        "NULL",
    ),
    (
        "SELECT * FROM orders WHERE a <> NULL",
        "write IS NOT NULL",
        "NULL",
    ),
    (
        "SELECT * FROM orders WHERE a > NULL",
        "A comparison with NULL is never true",
        "NULL",
    ),
    (
        "SELECT * FROM orders WHERE a BETWEEN NULL AND 3",
        "BETWEEN with NULL is never true",
        "NULL",
    ),
    // Text in double quotes is a field.
    (
        "SELECT * FROM orders WHERE status = \"paid\"",
        "\"paid\" is a field in SQL: for text write 'paid'",
        "\"paid\"",
    ),
    // Paths reach fields by name only.
    (
        "SELECT * FROM orders WHERE a.0 = 1",
        "does not index arrays",
        ".0",
    ),
    (
        "SELECT * FROM orders WHERE \"a\".\"0\" = 1",
        "does not index arrays",
        "\"0\"",
    ),
    (
        "SELECT * FROM orders WHERE a[0] = 1",
        "does not index arrays",
        "a[0]",
    ),
    (
        "SELECT * FROM orders WHERE \"$where\" = 1",
        "starts with $",
        "\"$where\"",
    ),
    (
        "SELECT * FROM orders WHERE \"a.b\" = 1",
        "has a dot in its name",
        "\"a.b\"",
    ),
    // Collections.
    (
        "SELECT * FROM shop.orders",
        "may be the collection orders of the database shop or a collection named shop.orders",
        "shop.orders",
    ),
    ("SELECT * FROM a.b.c", "Name one collection", "a.b.c"),
    ("SELECT 1", "Name the collection", "SELECT"),
    (
        "SELECT * FROM \"x.y\".\"orders\"",
        "is not a database's name",
        "\"x.y\"",
    ),
    // Columns and groups.
    ("SELECT *, a FROM orders", "Write * alone", "a"),
    (
        "SELECT status, total FROM orders GROUP BY status",
        "total is neither in GROUP BY nor inside an aggregate",
        "total",
    ),
    (
        "SELECT * FROM orders GROUP BY status",
        "Name the fields",
        "*",
    ),
    ("SELECT a + 1 FROM orders", SELECT_TAKES, "a + 1"),
    ("SELECT upper(a) FROM orders", SELECT_TAKES, "upper(a)"),
    ("SELECT SUM(*) FROM orders", "SUM takes one field", "SUM(*)"),
    (
        "SELECT status FROM orders GROUP BY status HAVING total > 1",
        "total is not in GROUP BY",
        "total",
    ),
    (
        "SELECT status FROM orders GROUP BY lower(status)",
        "GROUP BY takes fields",
        "lower(status)",
    ),
    (
        "SELECT a FROM orders ORDER BY 2",
        "2 is not a column of the SELECT",
        "2",
    ),
    (
        "SELECT 1 AS one FROM orders ORDER BY one",
        "a value orders nothing",
        "one",
    ),
    (
        "SELECT * FROM orders ORDER BY a, a DESC",
        "ORDER BY names a twice",
        "a",
    ),
    // Limits.
    (
        "SELECT * FROM orders LIMIT 0",
        "LIMIT 0 returns no rows",
        "0",
    ),
    (
        "SELECT * FROM orders LIMIT 5, 10",
        "Write LIMIT n OFFSET m",
        "5",
    ),
    (
        "SELECT * FROM orders LIMIT 1.5",
        "LIMIT takes a whole number",
        "1.5",
    ),
    (
        "SELECT * FROM orders FETCH FIRST 5 ROWS ONLY",
        "Write LIMIT n instead of FETCH",
        "SELECT * FROM orders FETCH FIRST 5 ROWS ONLY",
    ),
    // Values.
    (
        "SELECT * FROM orders WHERE _id = ObjectId('xyz')",
        "An ObjectId is 24 hexadecimal digits",
        "ObjectId('xyz')",
    ),
    (
        "SELECT * FROM orders WHERE at = DATE '2026-01-01'",
        "Write TIMESTAMP",
        "DATE '2026-01-01'",
    ),
    (
        "SELECT * FROM orders WHERE at = TIMESTAMP 'yesterday'",
        "is not a date",
        "TIMESTAMP 'yesterday'",
    ),
    (
        "SELECT * FROM orders WHERE a = ?",
        "takes values, not parameters",
        "?",
    ),
    (
        "SELECT * FROM orders WHERE name LIKE 'a' ESCAPE 'ab'",
        "ESCAPE takes one character",
        "'ab'",
    ),
    (
        "SELECT * FROM orders WHERE name LIKE 'a!' ESCAPE '!'",
        "ends with its escape character",
        "'a!'",
    ),
    (
        "SELECT * FROM orders WHERE name LIKE a",
        "LIKE takes a pattern in quotes",
        "a",
    ),
    (
        "SELECT * FROM orders WHERE a IN (1, b)",
        "IN takes a list of values",
        "b",
    ),
    (
        "SELECT * FROM orders WHERE a = 99999999999999999999",
        "99999999999999999999 does not fit in a 64-bit integer, and as a Double it would lose digits: write DECIMAL '99999999999999999999'",
        "99999999999999999999",
    ),
    (
        "SELECT * FROM orders WHERE a IN (1, -9223372036854775809)",
        "does not fit in a 64-bit integer",
        "-9223372036854775809",
    ),
    // SQL's functions without parentheses, which read as fields would find
    // nothing.
    (
        "SELECT CURRENT_TIMESTAMP FROM orders",
        "CURRENT_TIMESTAMP is a function of SQL, which Kuery's SQL does not run: for a field of that name write \"CURRENT_TIMESTAMP\"",
        "CURRENT_TIMESTAMP",
    ),
    (
        "SELECT * FROM orders WHERE current_date > 1",
        "write \"current_date\"",
        "current_date",
    ),
    (
        "SELECT a FROM orders ORDER BY LOCALTIMESTAMP",
        "LOCALTIMESTAMP is a function of SQL",
        "LOCALTIMESTAMP",
    ),
    (
        "SELECT COUNT(SESSION_USER) FROM orders",
        "SESSION_USER is a function of SQL",
        "SESSION_USER",
    ),
    (
        "SELECT a FROM orders GROUP BY CURRENT_USER",
        "CURRENT_USER is a function of SQL",
        "CURRENT_USER",
    ),
    (
        "SELECT * FROM orders WHERE CURRENT_TIME IS NULL OR LOCALTIME = 1",
        "CURRENT_TIME is a function of SQL",
        "CURRENT_TIME",
    ),
    (
        "SELECT CURRENT_CATALOG FROM orders",
        "CURRENT_CATALOG is a function of SQL",
        "CURRENT_CATALOG",
    ),
    // Text sqlparser cannot read, and more than one statement.
    (
        "SELECT * FROM orders WHERE a = 'open",
        "Unterminated string literal",
        "'",
    ),
    ("SELECT * FROM orders WHERE", "Expected: an expression", "E"),
    (
        "SELECT * FROM orders; SELECT * FROM b",
        "Run one statement at a time",
        "SELECT * FROM b",
    ),
];

#[test]
fn refuses_what_it_cannot_translate_and_says_where() {
    for (sql, message, at) in REFUSALS {
        let (said, pointed) = refused(sql);
        assert!(said.contains(message), "{sql}: {said}");
        assert_eq!(pointed, *at, "{sql}: {said}");
    }
}

#[test]
fn a_reserved_word_where_sqlparser_stops_gets_a_hint() {
    let (message, at) = refused("SELECT a FROM orders WHERE a = 1 AND interval = 2");
    assert!(
        message.contains("write it in double quotes: \"interval\""),
        "{message}"
    );
    assert_eq!(at, "interval");
    let (message, at) = refused("SELECT * FROM orders WHERE exists = 1");
    assert!(
        message.contains("write it in double quotes: \"exists\""),
        "{message}"
    );
    assert_eq!(at, "exists");
}

#[test]
fn refuses_a_write_hidden_in_a_query_whatever_its_shape() {
    // A query whose body is a write: sqlparser reads one inside a common
    // table expression. The allowlist refuses it on its own, not because
    // WITH is refused first.
    for sql in [
        "WITH x AS (INSERT INTO t VALUES (1) RETURNING *) SELECT * FROM x",
        "WITH x AS (UPDATE t SET a = 1 RETURNING *) SELECT * FROM x",
        "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x",
    ] {
        let statements = Parser::parse_sql(&GenericDialect {}, sql).unwrap();
        let Statement::Query(outer) = &statements[0] else {
            panic!("{sql}");
        };
        let inner = &outer.with.as_ref().unwrap().cte_tables[0].query;
        let text = Text::new(sql);
        let error = select(&text, inner).expect_err(sql);
        assert!(
            error.message.starts_with(READ_ONLY),
            "{sql}: {}",
            error.message
        );
    }
}

#[test]
fn spans_count_bytes_past_text_that_is_not_ascii() {
    let sql = "SELECT * FROM órdenes WHERE ñame = \"é\"";
    let (_, at) = refused(sql);
    assert_eq!(at, "\"é\"");
    let sql = "-- pedidos\n  SELECT * FROM órdenes\n  WHERE ñ = NULL";
    let errors = translate(sql, &TranslateContext::default()).unwrap_err();
    assert_eq!(&sql[errors[0].span.clone()], "NULL");
    assert_eq!(
        errors[0].located(sql),
        "line 3, column 13: = NULL is never true: write IS NULL"
    );
}

#[test]
fn splits_a_script_at_its_semicolons() {
    let script = "SELECT * FROM a WHERE b = ';';\n-- next\nSELECT * FROM c\n";
    let texts: Vec<&str> = split(script).into_iter().map(|r| &script[r]).collect();
    assert_eq!(
        texts,
        ["SELECT * FROM a WHERE b = ';'", "-- next\nSELECT * FROM c"]
    );
    let broken = "SELECT * FROM a WHERE b = 'open";
    // One statement, the whole text: the translation says where it breaks.
    let whole = 0..broken.len();
    assert_eq!(split(broken), [whole]);
    assert!(split("  \n").is_empty());
}

fn field(path: &str, types: &[(&'static str, usize)]) -> FieldSchema {
    FieldSchema {
        path: path.into(),
        parts: path
            .split('.')
            .map(|part| Part::Field(part.into()))
            .collect(),
        present: types.iter().map(|(_, n)| n).sum(),
        types: types.to_vec(),
    }
}

fn sampled(fields: Vec<FieldSchema>) -> TranslateContext {
    TranslateContext {
        schema: Some(Schema {
            documents: 100,
            fields,
        }),
        collection: None,
    }
}

#[test]
fn warns_of_what_the_sample_says_without_refusing() {
    let context = sampled(vec![
        field("_id", &[("ObjectId", 100)]),
        field("tags", &[("String", 62), ("Array", 38)]),
        field("ship", &[("Object", 90), ("Array", 10)]),
        field("ship.city", &[("String", 90)]),
        field("status", &[("String", 100)]),
    ]);
    let sql = "SELECT * FROM orders WHERE _id = '65f0a1b2c3d4e5f601234567' AND tags = 'x' AND ship.city = 'Lyon' AND status = 'paid'";
    let translation = translated(sql, &context);
    let warnings: Vec<(&str, &str)> = translation
        .warnings
        .iter()
        .map(|warning| (warning.message.as_str(), &sql[warning.span.clone()]))
        .collect();
    assert_eq!(
        warnings,
        [
            (
                "_id looks like an ObjectId: did you mean ObjectId('65f0a1b2c3d4e5f601234567')?",
                "'65f0a1b2c3d4e5f601234567'"
            ),
            (
                "tags is an array in 38% of the sample: where it is one, this condition is neither true nor false",
                "tags"
            ),
            (
                "ship is an array in 10% of the sample: where it is one, this condition is neither true nor false",
                "ship.city"
            ),
        ]
    );
    // The sample changes no guard.
    assert_eq!(
        translation.native,
        translated(sql, &TranslateContext::default()).native
    );
    // A number with a point is a Double, which no Decimal128 equals.
    let decimals = sampled(vec![
        field("price", &[("Decimal128", 70), ("Int32", 30)]),
        field("qty", &[("Int32", 100)]),
    ]);
    let sql = "SELECT * FROM orders WHERE price = 19.99 OR price IN (1.5, 2) OR price BETWEEN 1.0 AND 3 OR price > -2.5 OR price = 20 OR qty = 1.5 OR price = DECIMAL '9.99'";
    let translation = translated(sql, &decimals);
    let warnings: Vec<(&str, &str)> = translation
        .warnings
        .iter()
        .map(|warning| (warning.message.as_str(), &sql[warning.span.clone()]))
        .collect();
    let decimal = |written: &str| {
        format!(
            "price holds Decimal128 in the sample, and {written} is a Double, which may not equal it: did you mean DECIMAL '{written}'?"
        )
    };
    assert_eq!(
        warnings,
        [
            (decimal("19.99").as_str(), "19.99"),
            (decimal("1.5").as_str(), "1.5"),
            (decimal("1.0").as_str(), "1.0"),
            (decimal("-2.5").as_str(), "-2.5"),
        ]
    );
    assert_eq!(
        translation.native,
        translated(sql, &TranslateContext::default()).native
    );
    // A sampled field in double quotes is a comparison of two fields.
    let (message, _) = match translate("SELECT * FROM orders WHERE tags = \"status\"", &context) {
        Err(errors) => (errors[0].message.clone(), ()),
        Ok(_) => panic!("translated"),
    };
    assert_eq!(message, "Comparing two fields is not supported");
}

fn collection(indexes: &[&str], options: Document) -> TranslateContext {
    let index = |field: &str| DocIndex {
        name: format!("{field}_1"),
        keys: Document(vec![(field.into(), Value::Int32(1))]),
        unique: false,
        sparse: false,
        partial: None,
        ttl: None,
        hidden: false,
        size: None,
        usage: None,
    };
    TranslateContext {
        schema: None,
        collection: Some(CollectionInfo {
            kind: ContainerKind::Collection,
            indexes: indexes.iter().map(|field| index(field)).collect(),
            validator: None,
            options,
        }),
    }
}

#[test]
fn says_when_it_reads_the_whole_collection() {
    let context = collection(&["_id", "status"], Document::default());
    let scans = |sql: &str| translated(sql, &context).full_scan;
    assert!(scans("SELECT * FROM orders"));
    assert!(scans("SELECT * FROM orders WHERE total > 5"));
    assert!(!scans("SELECT * FROM orders WHERE status = 'paid'"));
    assert!(!scans("SELECT * FROM orders WHERE _id = 5"));
    assert!(!scans("SELECT * FROM orders ORDER BY status"));
    assert!(!scans("SELECT * FROM orders LIMIT 10"));
    assert!(scans("SELECT COUNT(*) FROM orders"));
    // An OR reads through an index only when every branch does.
    assert!(scans(
        "SELECT * FROM orders WHERE status = 'paid' OR total > 5"
    ));
    assert!(!scans(
        "SELECT * FROM orders WHERE status = 'paid' OR _id = 5"
    ));
    assert!(!scans(
        "SELECT * FROM orders WHERE total > 5 AND (status = 'a' OR (_id > 1 AND total < 9))"
    ));
    assert!(scans(
        "SELECT * FROM orders WHERE total > 5 AND (status = 'a' OR NOT _id = 1)"
    ));
    // Negations never narrow an index.
    for sql in [
        "SELECT * FROM orders WHERE NOT (status = 'paid')",
        "SELECT * FROM orders WHERE NOT (total > 5 OR status = 'paid')",
        "SELECT * FROM orders WHERE status <> 'paid'",
        "SELECT * FROM orders WHERE status NOT IN ('a', 'b')",
        "SELECT * FROM orders WHERE status NOT BETWEEN 'a' AND 'b'",
        "SELECT * FROM orders WHERE status IS NOT NULL",
        "SELECT * FROM orders WHERE NOT missing(status)",
        "SELECT * FROM orders WHERE status NOT LIKE 'pa%'",
    ] {
        assert!(scans(sql), "{sql}");
    }
    for sql in [
        "SELECT * FROM orders WHERE status IN ('a', 'b')",
        "SELECT * FROM orders WHERE status BETWEEN 'a' AND 'b'",
        "SELECT * FROM orders WHERE status IS NULL",
        "SELECT * FROM orders WHERE missing(status)",
        "SELECT * FROM orders WHERE 'a' < status",
        "SELECT * FROM orders o WHERE o.status = 'a'",
    ] {
        assert!(!scans(sql), "{sql}");
    }
    // LIKE only with a fixed start, and ILIKE never.
    assert!(!scans("SELECT * FROM orders WHERE status LIKE 'pa%'"));
    assert!(!scans(
        "SELECT * FROM orders WHERE status LIKE '!%x%' ESCAPE '!'"
    ));
    assert!(scans("SELECT * FROM orders WHERE status LIKE '%id'"));
    assert!(scans("SELECT * FROM orders WHERE status LIKE '_aid'"));
    assert!(scans("SELECT * FROM orders WHERE status ILIKE 'pa%'"));
    // A LIMIT after `$group` (or `$facet`) comes once every document was
    // read.
    assert!(scans(
        "SELECT status, COUNT(*) FROM orders GROUP BY status LIMIT 5"
    ));
    assert!(scans("SELECT COUNT(*) FROM orders LIMIT 1"));
    assert!(scans(
        "SELECT MAX(total) FROM orders WHERE total > 1 LIMIT 1"
    ));
    assert!(!scans(
        "SELECT status, COUNT(*) FROM orders WHERE status = 'a' GROUP BY status LIMIT 5"
    ));
    assert!(!scans("SELECT * FROM orders WHERE total > 5 LIMIT 5"));
    // Unknown indexes say nothing.
    assert!(!translated("SELECT * FROM orders", &TranslateContext::default()).full_scan);
}

#[test]
fn offers_an_estimate_for_a_count_of_everything() {
    let translation = translated(
        "SELECT COUNT(*) AS n FROM orders",
        &TranslateContext::default(),
    );
    let estimate = translation.estimate.expect("an estimate");
    assert_eq!(estimate.native, "db.orders.estimatedDocumentCount()");
    assert_eq!(
        estimate.columns,
        [OutputColumn {
            field: "count".into(),
            label: "n".into()
        }]
    );
    identity(&estimate.native);
    for sql in [
        "SELECT COUNT(*) FROM orders WHERE a = 1",
        "SELECT COUNT(a) FROM orders",
        "SELECT COUNT(*), SUM(a) FROM orders",
        "SELECT status, COUNT(*) FROM orders GROUP BY status",
        "SELECT * FROM orders",
    ] {
        assert!(
            translated(sql, &TranslateContext::default())
                .estimate
                .is_none(),
            "{sql}"
        );
    }
}

#[test]
fn notes_the_collation_it_follows() {
    let collation = Document(vec![(
        "collation".into(),
        Value::Object(Document(vec![
            ("locale".into(), Value::String("fr".into())),
            ("strength".into(), Value::Int32(1)),
        ])),
    )]);
    let context = collection(&[], collation);
    assert_eq!(
        translated("SELECT * FROM orders WHERE a = 'é'", &context).notes,
        ["Compares and orders by the collection's collation (fr)"]
    );
    assert_eq!(
        translated("SELECT * FROM orders WHERE a LIKE 'é%'", &context).notes,
        ["= and ORDER BY follow the collection's collation (fr); LIKE does not"]
    );
    assert!(
        translated(
            "SELECT * FROM orders",
            &collection(&[], Document::default())
        )
        .notes
        .is_empty()
    );
}

/// A small SQL grammar: paths, values and predicates in every
/// combination the translator takes, and some it refuses.
fn path() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("a".to_string()),
        Just("b.c".to_string()),
        Just("_id".to_string()),
        Just("\"order\"".to_string()),
        Just("o.d.e".to_string()),
        Just("ñ".to_string()),
    ]
}

fn value() -> impl Strategy<Value = String> {
    prop_oneof![
        any::<i32>().prop_map(|n| n.to_string()),
        any::<i64>().prop_map(|n| n.to_string()),
        (-1e6f64..1e6).prop_map(|x| format!("{x:.3}")),
        "[a-zA-Z0-9 %_'é\\n]{0,8}".prop_map(|s| format!("'{}'", s.replace('\'', "''"))),
        Just("TRUE".to_string()),
        Just("NULL".to_string()),
        Just("ObjectId('65f0a1b2c3d4e5f601234567')".to_string()),
        Just("DECIMAL '9.99'".to_string()),
        Just("TIMESTAMP '2026-10-28T12:00:00Z'".to_string()),
        Just("NumberLong(7)".to_string()),
    ]
}

fn predicate() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        (
            path(),
            prop_oneof![
                Just("="),
                Just("<>"),
                Just("<"),
                Just("<="),
                Just(">"),
                Just(">=")
            ],
            value()
        )
            .prop_map(|(p, op, v)| format!("{p} {op} {v}")),
        (value(), path()).prop_map(|(v, p)| format!("{v} < {p}")),
        path().prop_map(|p| format!("{p} IS NULL")),
        path().prop_map(|p| format!("{p} IS NOT NULL")),
        path().prop_map(|p| format!("missing({p})")),
        path().prop_map(|p| p.to_string()),
        (
            path(),
            proptest::collection::vec(value(), 1..4),
            any::<bool>()
        )
            .prop_map(|(p, vs, not)| {
                format!(
                    "{p} {}IN ({})",
                    if not { "NOT " } else { "" },
                    vs.join(", ")
                )
            }),
        (path(), value(), value()).prop_map(|(p, a, b)| format!("{p} BETWEEN {a} AND {b}")),
        (path(), "[a-z%_!.()é]{0,6}", any::<bool>()).prop_map(|(p, pattern, i)| {
            format!(
                "{p} {} '{pattern}' ESCAPE '!'",
                if i { "ILIKE" } else { "LIKE" }
            )
        }),
        Just("TRUE".to_string()),
    ];
    leaf.prop_recursive(3, 12, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a} AND {b})")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a} OR {b})")),
            inner.prop_map(|a| format!("NOT ({a})")),
        ]
    })
}

fn query() -> impl Strategy<Value = String> {
    let plain = (
        prop_oneof![
            Just("*".to_string()),
            proptest::collection::vec(prop_oneof![path(), value()], 1..4)
                .prop_map(|items| items.join(", ")),
        ],
        proptest::option::of(predicate()),
        proptest::option::of((path(), any::<bool>())),
        proptest::option::of(1u32..50),
        proptest::option::of(0u32..50),
    )
        .prop_map(|(items, filter, order, limit, offset)| {
            let mut sql = format!("SELECT {items} FROM orders o");
            if let Some(filter) = filter {
                sql += &format!(" WHERE {filter}");
            }
            if let Some((path, descending)) = order {
                sql += &format!(" ORDER BY {path}{}", if descending { " DESC" } else { "" });
            }
            if let Some(limit) = limit {
                sql += &format!(" LIMIT {limit}");
            }
            if let Some(offset) = offset {
                sql += &format!(" OFFSET {offset}");
            }
            sql
        });
    let grouped = (
        proptest::collection::vec(path(), 0..3),
        proptest::sample::subsequence(
            vec![
                "COUNT(*)",
                "COUNT(a)",
                "SUM(b.c)",
                "AVG(a)",
                "MIN(\"order\")",
                "MAX(_id)",
            ],
            1..4,
        ),
        proptest::option::of(predicate()),
        proptest::option::of(prop_oneof![
            value().prop_map(|v| format!("COUNT(*) > {v}")),
            Just("SUM(b.c) IS NULL".to_string()),
            Just("MAX(_id) <> 3 OR MIN(a) = 'x'".to_string()),
        ]),
    )
        .prop_map(|(keys, aggregates, filter, having)| {
            let mut items: Vec<String> = keys.clone();
            items.extend(aggregates.iter().map(|a| a.to_string()));
            let mut sql = format!("SELECT {} FROM orders o", items.join(", "));
            if let Some(filter) = filter {
                sql += &format!(" WHERE {filter}");
            }
            if !keys.is_empty() {
                sql += &format!(" GROUP BY {}", keys.join(", "));
            }
            if let Some(having) = having {
                sql += &format!(" HAVING {having}");
            }
            sql += " ORDER BY 1 DESC";
            sql
        });
    prop_oneof![plain, grouped]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1500))]

    /// Every translation reads back as itself, is a read, and passes the
    /// protocol guard; every refusal points inside the text.
    #[test]
    fn every_translation_is_a_read_the_guard_lets_through(sql in query()) {
        match translate(&sql, &TranslateContext::default()) {
            Ok(translation) => {
                identity(&translation.native);
                if let Some(estimate) = &translation.estimate {
                    identity(&estimate.native);
                }
            }
            Err(errors) => {
                for error in errors {
                    prop_assert!(!error.message.contains("cannot vouch for"), "{sql}: {}", error.message);
                    prop_assert!(error.span.end <= sql.len(), "{sql}: {error:?}");
                    prop_assert!(sql.is_char_boundary(error.span.start) && sql.is_char_boundary(error.span.end));
                }
            }
        }
    }

    /// Arbitrary text never panics, and refusals point inside it.
    #[test]
    fn any_text_is_translated_or_refused(sql in "\\PC{0,60}") {
        if let Err(errors) = translate(&sql, &TranslateContext::default()) {
            for error in errors {
                prop_assert!(error.span.start <= error.span.end && error.span.end <= sql.len());
                prop_assert!(sql.is_char_boundary(error.span.start) && sql.is_char_boundary(error.span.end));
            }
        }
    }
}
