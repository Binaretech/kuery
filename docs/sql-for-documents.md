# Kuery's SQL for documents

A MongoDB console speaks SQL too: pick SQL in its language picker. Kuery's
SQL only reads. Each statement is translated into the one `aggregate` that
answers it, the Translation strip under the editor shows that mongosh
text, and Run sends exactly that text through the same checks as one
typed by hand. There is no other way for SQL to run, and no translation
back from mongosh.

Kuery's SQL is a small dialect, read with sqlparser's generic dialect. What
it cannot translate faithfully, it refuses, pointing at the text at fault;
it never approximates. Its rules are SQL's where MongoDB can keep them,
and say so where they follow MongoDB instead.

## Using it

- The strip follows the cursor: **Copy as mongosh**, **Open in mongosh**
  (a console of its own with that text) and **Explain** act on the
  statement under it.
- Rows show as a table, with the columns the `SELECT` names in its order;
  the Documents view shows what the `aggregate` returned, without running
  again. History keeps the SQL and the mongosh it ran.
- The collection's sampled schema never blocks a statement, but it can
  warn before one runs, such as when `_id` looks like an ObjectId and is
  compared with text, or a field is an array in part of the sample.
- A statement that would read a whole collection (no index serves its
  filter or its order, and it has no `LIMIT`) is marked in the strip, and
  asks first on production. `COUNT(*)` without `WHERE` offers the
  collection's estimated count instead.
- The console's time limit (`maxTimeMS`) applies as to any statement.

## What it reads

One `SELECT` from one collection:

- `FROM orders`, `FROM orders o` or `FROM orders AS o`. A name with a dot
  or a dash is quoted, `FROM "order-lines"`; another database's
  collection is `FROM "other"."orders"`.
- `SELECT *` (whole documents, flattened as in the grid), field paths
  (`ship.city`), aliases (`AS t`), `o.*`, and literals.
- `WHERE` comparing a field with a value: `=`, `<>`, `<`, `<=`, `>`, `>=`,
  `AND`, `OR`, `NOT`, `IN` and `NOT IN` of values, `BETWEEN`,
  `IS [NOT] NULL`, `missing(path)`, `LIKE` and `ILIKE` with `ESCAPE`, and a
  field alone as a boolean (`WHERE active AND NOT archived`).
- `GROUP BY` fields, with `COUNT(*)`, `COUNT(path)`, `SUM`, `AVG`, `MIN`
  and `MAX`, and `HAVING` over them.
- `ORDER BY` fields, aliases, positions (`ORDER BY 1`) and aggregates,
  `ASC` or `DESC`; `LIMIT` and `OFFSET`.

Literals: text in single quotes, numbers, `TRUE`, `FALSE` and `NULL`, and
MongoDB's types as functions: `ObjectId('…')`, `ISODate('…')`,
`NumberLong(…)` and `NumberDecimal('…')`. `DECIMAL '9.99'` is a
Decimal128 and `TIMESTAMP '2026-01-01T00:00:00Z'` a date (not BSON's
Timestamp type). `42` is a 32-bit integer, a 64-bit one when it does not
fit; `2.0` and `1.5e3` are doubles.

Names keep their case. Text in double quotes is a field, as in SQL:
`status = "paid"` compares two fields and is refused with a hint to write
`'paid'`. A reserved word used as a field (`order`, `group`, `date`…) is
written in double quotes where the parser needs it. Without an alias, the
collection's name qualifies its fields (`orders.status`); with one, the
alias does, and the collection's name is then an ordinary field.

## How it behaves

| SQL | In Kuery's SQL for documents |
| --- | --- |
| Three-valued logic | A comparison with a null or missing field is unknown, and unknown is neither true nor false: `a <> 5` and `NOT a = 5` never match documents where `a` is null or missing. |
| `IS NULL` | True for null and for a missing field. `missing(a)` is true only when `a` is missing. |
| `= NULL`, `<> NULL`, `< NULL` | Refused: they are never true. Write `IS NULL` or `IS NOT NULL`. |
| `IN` and `NOT IN` | `NULL` in an `IN` list is dropped; `NOT IN` with a `NULL` in its list is never true. |
| Arrays | A comparison with a value never matches an array, nor goes through one on the way down a path (`ship.city` when `ship` is an array). `IS NOT NULL` is true for an array. Paths reach fields by name only: `a.0` and `a[0]` are refused. |
| Types | Values of different types are never equal, and `<`, `>` and `BETWEEN` between different types are unknown, as in MongoDB's comparisons. |
| `ORDER BY` | MongoDB's order: null and missing first ascending, last descending, and the collection's collation for text. `NULLS FIRST` or `NULLS LAST` against that order is refused. |
| `LIKE` | The whole value must match, across lines; `ILIKE` ignores case. A collection's collation applies to `=` but not to `LIKE`, which the strip notes. |
| `GROUP BY` | Null and missing values are one group. |
| `COUNT(path)` | Counts values that are neither null nor missing. |
| `SUM` | `NULL` when there were no numbers (MongoDB's `$sum` would give 0). `AVG`, `MIN` and `MAX` are MongoDB's. |
| Aggregates without `GROUP BY` | One row, even when no document matched. |

## What it refuses

Writes and schema changes (`INSERT`, `UPDATE`, `DELETE`, `CREATE`, `DROP`,
`SELECT … INTO`, `FOR UPDATE`, a write inside a `WITH`) with "Kuery's SQL
over MongoDB only reads: switch to mongosh or edit in the grid";
transactions; `EXPLAIN` (Explain is the console's own); joins,
subqueries, `WITH`, `UNION`, `INTERSECT` and `EXCEPT`; `DISTINCT`; window
functions; functions in `WHERE` and `ORDER BY`; comparing two fields;
`COLLATE`; and more than one statement at a time. Each refusal points at
the text at fault.

A translation is checked again before it runs: it must read as a read to
the console's own reading, and its command must pass the check every
read-only command passes. One that does not is refused as a fault of
Kuery's, and never runs.

## Examples

Every example below comes from the translator's tests, which check that
each SQL statement still becomes exactly this mongosh, and that this page
still shows them as they are.

<!-- examples: generated by `KUERY_WRITE_DOCS=1 cargo test -p kuery-mongo documented`; do not edit -->

### Translations

```js
// SELECT * FROM orders
db.orders.aggregate([])
```

```js
// SELECT name, total AS t FROM orders WHERE status = 'paid' AND total >= 100 ORDER BY total DESC LIMIT 10
db.orders.aggregate([{$match: {status: {$not: {$type: "array"}, $eq: "paid"}, total: {$not: {$type: "array"}, $gte: 100}}}, {$sort: {total: -1}}, {$limit: 10}, {$project: {_id: 0, c0: "$name", c1: "$total"}}])
```

```js
// SELECT * FROM orders WHERE a <> 5
db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $nin: [5, null]}}}])
```

```js
// SELECT * FROM orders WHERE NOT (a = 5 OR b < 2)
db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $nin: [5, null]}, b: {$not: {$type: "array"}, $gte: 2}}}])
```

```js
// SELECT * FROM orders WHERE a = 1 OR NOT b = 2
db.orders.aggregate([{$match: {$or: [{a: {$not: {$type: "array"}, $eq: 1}}, {b: {$not: {$type: "array"}, $nin: [2, null]}}]}}])
```

```js
// SELECT * FROM orders o WHERE o.ship.city IS NULL
db.orders.aggregate([{$match: {ship: {$not: {$type: "array"}}, "ship.city": {$not: {$type: "array"}, $eq: null}}}])
```

```js
// SELECT * FROM orders WHERE ship.city IS NOT NULL
db.orders.aggregate([{$match: {ship: {$not: {$type: "array"}}, $or: [{"ship.city": {$type: "array"}}, {"ship.city": {$ne: null}}]}}])
```

```js
// SELECT * FROM orders WHERE tags IS NULL
db.orders.aggregate([{$match: {tags: {$not: {$type: "array"}, $eq: null}}}])
```

```js
// SELECT * FROM orders WHERE tags IS NOT NULL
db.orders.aggregate([{$match: {$or: [{tags: {$type: "array"}}, {tags: {$ne: null}}]}}])
```

```js
// SELECT * FROM orders WHERE _id IS NOT NULL
db.orders.aggregate([{$match: {_id: {$ne: null}}}])
```

```js
// SELECT * FROM orders WHERE missing(ship.city)
db.orders.aggregate([{$match: {ship: {$not: {$type: "array"}}, "ship.city": {$exists: false}}}])
```

```js
// SELECT * FROM orders WHERE NOT missing(coupon)
db.orders.aggregate([{$match: {coupon: {$exists: true}}}])
```

```js
// SELECT * FROM orders WHERE _id = ObjectId('65f0a1b2c3d4e5f601234567')
db.orders.aggregate([{$match: {_id: {$eq: ObjectId("65f0a1b2c3d4e5f601234567")}}}])
```

```js
// SELECT * FROM orders WHERE _id.region = 'eu'
db.orders.aggregate([{$match: {"_id.region": {$not: {$type: "array"}, $eq: "eu"}}}])
```

```js
// SELECT * FROM orders WHERE a IN (1, 2, NULL)
db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $in: [1, 2]}}}])
```

```js
// SELECT * FROM orders WHERE a NOT IN (1, 2)
db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $nin: [1, 2, null]}}}])
```

```js
// SELECT * FROM orders WHERE a NOT IN (1, NULL)
db.orders.aggregate([{$match: {$expr: false}}])
```

```js
// SELECT * FROM orders WHERE a IN (NULL)
db.orders.aggregate([{$match: {$expr: false}}])
```

```js
// SELECT * FROM orders WHERE NOT a IN (1, NULL)
db.orders.aggregate([{$match: {$expr: false}}])
```

```js
// SELECT * FROM orders WHERE a BETWEEN 1 AND 5
db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $gte: 1, $lte: 5}}}])
```

```js
// SELECT * FROM orders WHERE a NOT BETWEEN 1 AND 5
db.orders.aggregate([{$match: {a: {$not: {$type: "array"}}, $or: [{a: {$lt: 1}}, {a: {$gt: 5}}]}}])
```

```js
// SELECT * FROM orders WHERE 5 < a AND -2.5 >= b
db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $gt: 5}, b: {$not: {$type: "array"}, $lte: -2.5}}}])
```

```js
// SELECT * FROM orders WHERE name LIKE 'a_%.b' ESCAPE '!'
db.orders.aggregate([{$match: {name: {$not: {$type: "array"}, $regex: /\Aa..*\.b\z/s}}}])
```

```js
// SELECT * FROM orders WHERE name LIKE '100!%/(x)' ESCAPE '!'
db.orders.aggregate([{$match: {name: {$not: {$type: "array"}, $regex: /\A100%\/\(x\)\z/s}}}])
```

```js
// SELECT * FROM orders WHERE name NOT ILIKE 'é%'
db.orders.aggregate([{$match: {$and: [{name: {$not: {$type: "array"}, $type: "string"}}, {name: {$not: /\Aé.*\z/is}}]}}])
```

```js
// SELECT * FROM orders WHERE price = DECIMAL '9.99' AND at < TIMESTAMP '2026-01-01T00:00:00Z' AND n = NumberLong(5) AND d = NumberDecimal('0.1')
db.orders.aggregate([{$match: {price: {$not: {$type: "array"}, $eq: NumberDecimal("9.99")}, at: {$not: {$type: "array"}, $lt: ISODate("2026-01-01T00:00:00.000Z")}, n: {$not: {$type: "array"}, $eq: NumberLong("5")}, d: {$not: {$type: "array"}, $eq: NumberDecimal("0.1")}}}])
```

```js
// SELECT * FROM orders WHERE a = 9223372036854775807 AND b = 1.5e3 AND c = 2.0
db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $eq: NumberLong("9223372036854775807")}, b: {$not: {$type: "array"}, $eq: 1500.0}, c: {$not: {$type: "array"}, $eq: 2.0}}}])
```

```js
// SELECT * FROM orders WHERE active AND NOT archived
db.orders.aggregate([{$match: {active: {$not: {$type: "array"}, $eq: true}, archived: {$not: {$type: "array"}, $eq: false}}}])
```

```js
// SELECT * FROM orders WHERE TRUE
db.orders.aggregate([])
```

```js
// SELECT * FROM orders WHERE FALSE OR NULL
db.orders.aggregate([{$match: {$expr: false}}])
```

```js
// SELECT status, COUNT(*) AS n, SUM(total), AVG(total), MIN(total), MAX(total), COUNT(coupon) FROM orders GROUP BY status HAVING COUNT(*) > 1 ORDER BY n DESC
db.orders.aggregate([{$group: {_id: {k0: {$ifNull: ["$status", null]}}, a0: {$sum: 1}, a1: {$sum: "$total"}, n1: {$sum: {$cond: [{$isNumber: "$total"}, 1, 0]}}, a2: {$avg: "$total"}, a3: {$min: "$total"}, a4: {$max: "$total"}, a5: {$sum: {$cond: [{$in: [{$type: "$coupon"}, ["missing", "null"]]}, 0, 1]}}}}, {$project: {_id: 0, c0: "$_id.k0", c1: "$a0", c2: {$cond: [{$gt: ["$n1", 0]}, "$a1", null]}, c3: "$a2", c4: "$a3", c5: "$a4", c6: "$a5"}}, {$match: {c1: {$gt: 1}}}, {$sort: {c1: -1}}])
```

```js
// SELECT ship.city, status FROM orders GROUP BY status, ship.city HAVING MAX(total) >= 10 AND status <> 'void' ORDER BY 1 LIMIT 3
db.orders.aggregate([{$group: {_id: {k0: {$ifNull: ["$status", null]}, k1: {$ifNull: ["$ship.city", null]}}, a0: {$max: "$total"}}}, {$project: {_id: 0, c0: "$_id.k1", c1: "$_id.k0", h0: "$a0"}}, {$match: {h0: {$not: {$type: "array"}, $gte: 10}, c1: {$not: {$type: "array"}, $nin: ["void", null]}}}, {$sort: {c0: 1}}, {$limit: 3}, {$project: {h0: 0}}])
```

```js
// SELECT COUNT(*) FROM orders
db.orders.aggregate([{$facet: {g: [{$group: {_id: null, a0: {$sum: 1}}}]}}, {$project: {_id: 0, c0: {$ifNull: [{$first: "$g.a0"}, 0]}}}])
```

```js
// SELECT SUM(total), MIN(at) FROM orders WHERE status = 'paid'
db.orders.aggregate([{$match: {status: {$not: {$type: "array"}, $eq: "paid"}}}, {$facet: {g: [{$group: {_id: null, a0: {$sum: "$total"}, n0: {$sum: {$cond: [{$isNumber: "$total"}, 1, 0]}}, a1: {$min: "$at"}}}]}}, {$project: {_id: 0, c0: {$cond: [{$gt: [{$first: "$g.n0"}, 0]}, {$first: "$g.a0"}, null]}, c1: {$ifNull: [{$first: "$g.a1"}, null]}}}])
```

```js
// SELECT status FROM orders GROUP BY status ORDER BY MAX(total)
db.orders.aggregate([{$group: {_id: {k0: {$ifNull: ["$status", null]}}, a0: {$max: "$total"}}}, {$project: {_id: 0, c0: "$_id.k0", h0: "$a0"}}, {$sort: {h0: 1}}, {$project: {h0: 0}}])
```

```js
// SELECT 1, 'x' AS y, NULL FROM orders LIMIT 2 OFFSET 3
db.orders.aggregate([{$skip: 3}, {$limit: 2}, {$project: {_id: 0, c0: {$literal: 1}, c1: {$literal: "x"}, c2: {$literal: null}}}])
```

```js
// SELECT a AS total, b FROM orders ORDER BY total, 2 DESC
db.orders.aggregate([{$sort: {a: 1, b: -1}}, {$project: {_id: 0, c0: "$a", c1: "$b"}}])
```

```js
// SELECT * FROM "order-lines"
db.getCollection("order-lines").aggregate([])
```

```js
// SELECT * FROM "a.b" OFFSET 0
db.getCollection("a.b").aggregate([])
```

```js
// SELECT * FROM "other"."orders"
db.getSiblingDB("other").orders.aggregate([])
```

```js
// SELECT user, "order", date FROM orders ORDER BY "order"
db.orders.aggregate([{$sort: {order: 1}}, {$project: {_id: 0, c0: "$user", c1: "$order", c2: "$date"}}])
```

```js
// SELECT Ñame FROM orders WHERE Ñame = 'Ñ' AND ñame = 'ñ'
db.orders.aggregate([{$match: {"Ñame": {$not: {$type: "array"}, $eq: "Ñ"}, "ñame": {$not: {$type: "array"}, $eq: "ñ"}}}, {$project: {_id: 0, c0: "$Ñame"}}])
```

```js
// SELECT o.* FROM orders AS o ORDER BY o.at DESC
db.orders.aggregate([{$sort: {at: -1}}])
```

```js
// SELECT c.status FROM c
db.c.aggregate([{$project: {_id: 0, c0: "$status"}}])
```

```js
// SELECT orders.* FROM orders WHERE orders.status = 'paid'
db.orders.aggregate([{$match: {status: {$not: {$type: "array"}, $eq: "paid"}}}])
```

```js
// SELECT orders.a FROM "shop"."orders" ORDER BY orders.a
db.getSiblingDB("shop").orders.aggregate([{$sort: {a: 1}}, {$project: {_id: 0, c0: "$a"}}])
```

```js
// SELECT orders.x FROM orders o WHERE o.y = 1
db.orders.aggregate([{$match: {y: {$not: {$type: "array"}, $eq: 1}}}, {$project: {_id: 0, c0: "$orders.x"}}])
```

```js
// SELECT "CURRENT_TIMESTAMP" FROM orders
db.orders.aggregate([{$project: {_id: 0, c0: "$CURRENT_TIMESTAMP"}}])
```

```js
// SELECT * FROM orders WHERE a = -9223372036854775808
db.orders.aggregate([{$match: {a: {$not: {$type: "array"}, $eq: NumberLong("-9223372036854775808")}}}])
```

### Refusals

| SQL | Refused with | At |
| --- | --- | --- |
| `UPDATE orders SET a = 1` | Kuery's SQL over MongoDB only reads: switch to mongosh or edit in the grid | `UPDATE orders SET a = 1` |
| `DELETE FROM orders` | Kuery's SQL over MongoDB only reads: switch to mongosh or edit in the grid | `DELETE FROM orders` |
| `INSERT INTO orders VALUES (1)` | Kuery's SQL over MongoDB only reads: switch to mongosh or edit in the grid | `INSERT INTO orders VALUES (1)` |
| `INSERT INTO copy SELECT * FROM orders` | Kuery's SQL over MongoDB only reads: switch to mongosh or edit in the grid | `INSERT INTO copy SELECT * FROM orders` |
| `DROP TABLE orders` | Kuery's SQL over MongoDB only reads: switch to mongosh or edit in the grid | `DROP TABLE orders` |
| `CREATE TABLE t (a INT)` | Kuery's SQL over MongoDB only reads: switch to mongosh or edit in the grid | `CREATE TABLE t (a INT)` |
| `SELECT a INTO copy FROM orders` | Kuery's SQL over MongoDB only reads: switch to mongosh or edit in the grid: SELECT … INTO writes a table | `copy` |
| `SELECT * FROM orders FOR UPDATE` | Kuery's SQL over MongoDB only reads: switch to mongosh or edit in the grid: FOR UPDATE locks rows | `SELECT * FROM orders FOR UPDATE` |
| `WITH x AS (DELETE FROM orders RETURNING *) SELECT * FROM x` | WITH (common table expressions) is not supported | `WITH x AS (DELETE FROM orders RETURNING *)` |
| `BEGIN` | Kuery's SQL over MongoDB only reads: switch to mongosh or edit in the grid; there are no transactions to begin or end | `BEGIN` |
| `COMMIT` | Kuery's SQL over MongoDB only reads: switch to mongosh or edit in the grid; there are no transactions to begin or end | `COMMIT` |
| `EXPLAIN SELECT * FROM orders` | Write the SELECT alone: Explain in the console shows the plan of what it runs | `EXPLAIN SELECT * FROM orders` |
| `WITH x AS (SELECT 1) SELECT * FROM x` | WITH (common table expressions) is not supported | `WITH x AS (SELECT 1)` |
| `SELECT * FROM a UNION SELECT * FROM b` | UNION, INTERSECT and EXCEPT are not supported | `SELECT * FROM a UNION SELECT * FROM b` |
| `SELECT * FROM orders o JOIN customers c ON o.c = c._id` | Joins are not supported: Kuery's SQL reads one collection | `customers c` |
| `SELECT * FROM orders, customers` | Joins are not supported: Kuery's SQL reads one collection | `customers` |
| `SELECT DISTINCT status FROM orders` | DISTINCT is not supported yet: write GROUP BY | `SELECT` |
| `SELECT COUNT(DISTINCT status) FROM orders` | DISTINCT is not supported yet: write GROUP BY | `COUNT(DISTINCT status)` |
| `SELECT * FROM (SELECT * FROM orders) x` | Subqueries are not supported | `(SELECT * FROM orders) x` |
| `SELECT * FROM orders WHERE a IN (SELECT b FROM c)` | Subqueries are not supported | `a IN (SELECT b FROM c)` |
| `SELECT * FROM orders WHERE EXISTS (SELECT 1 FROM c)` | Subqueries are not supported | `SELECT 1 FROM c` |
| `SELECT ROW_NUMBER() OVER () FROM orders` | Window functions are not supported | `ROW_NUMBER()` |
| `SELECT * FROM orders WHERE lower(name) = 'x'` | Functions in WHERE are not supported | `lower(name)` |
| `SELECT * FROM orders WHERE COUNT(*) > 1` | Aggregates go in HAVING, not WHERE | `COUNT(*)` |
| `SELECT * FROM orders WHERE a = b` | Comparing two fields is not supported | `a = b` |
| `SELECT * FROM orders WHERE a < o.b` | Comparing two fields is not supported | `a < o.b` |
| `SELECT * FROM orders WHERE 1 = 1` | Compare a field with a value | `1 = 1` |
| `SELECT * FROM orders WHERE a COLLATE "fr" = 'x'` | COLLATE is not supported: MongoDB compares by the collection's collation | `a COLLATE "fr"` |
| `SELECT * FROM orders ORDER BY a NULLS LAST` | MongoDB puts null and missing first when ascending: NULLS LAST is not supported | `a` |
| `SELECT * FROM orders ORDER BY a DESC NULLS FIRST` | MongoDB puts null and missing last when descending: NULLS FIRST is not supported | `a` |
| `SELECT * FROM orders ORDER BY lower(a)` | ORDER BY takes fields | `lower(a)` |
| `SELECT * FROM orders WHERE a = NULL` | = NULL is never true: write IS NULL | `NULL` |
| `SELECT * FROM orders WHERE a <> NULL` | <> NULL is never true: write IS NOT NULL | `NULL` |
| `SELECT * FROM orders WHERE a > NULL` | A comparison with NULL is never true: write IS NULL or IS NOT NULL | `NULL` |
| `SELECT * FROM orders WHERE a BETWEEN NULL AND 3` | BETWEEN with NULL is never true | `NULL` |
| `SELECT * FROM orders WHERE status = "paid"` | "paid" is a field in SQL: for text write 'paid' | `"paid"` |
| `SELECT * FROM orders WHERE a.0 = 1` | Kuery's SQL does not index arrays: a path takes field names only (Expected: end of statement, found: .0) | `.0` |
| `SELECT * FROM orders WHERE "a"."0" = 1` | 0 reads as an array index, and Kuery's SQL does not index arrays | `"0"` |
| `SELECT * FROM orders WHERE a[0] = 1` | Kuery's SQL does not index arrays: a path takes field names only | `a[0]` |
| `SELECT * FROM orders WHERE "$where" = 1` | $where starts with $, which MongoDB reads as an operator, not a field | `"$where"` |
| `SELECT * FROM orders WHERE "a.b" = 1` | "a.b" has a dot in its name, which a path cannot reach: read it in mongosh | `"a.b"` |
| `SELECT * FROM shop.orders` | shop.orders may be the collection orders of the database shop or a collection named shop.orders: write "shop"."orders" for the first, "shop.orders" for the second | `shop.orders` |
| `SELECT * FROM a.b.c` | Name one collection: orders, "shop"."orders", or "name.with.dots" | `a.b.c` |
| `SELECT 1` | Name the collection: SELECT … FROM orders | `SELECT` |
| `SELECT * FROM "x.y"."orders"` | x.y is not a database's name | `"x.y"` |
| `SELECT *, a FROM orders` | Write * alone, or name the fields | `a` |
| `SELECT status, total FROM orders GROUP BY status` | total is neither in GROUP BY nor inside an aggregate | `total` |
| `SELECT * FROM orders GROUP BY status` | Name the fields: * cannot go with GROUP BY or an aggregate | `*` |
| `SELECT a + 1 FROM orders` | SELECT takes fields, values, and COUNT, SUM, AVG, MIN and MAX | `a + 1` |
| `SELECT upper(a) FROM orders` | SELECT takes fields, values, and COUNT, SUM, AVG, MIN and MAX | `upper(a)` |
| `SELECT SUM(*) FROM orders` | SUM takes one field: SUM(total) | `SUM(*)` |
| `SELECT status FROM orders GROUP BY status HAVING total > 1` | total is not in GROUP BY: group by it, or name it inside an aggregate | `total` |
| `SELECT status FROM orders GROUP BY lower(status)` | GROUP BY takes fields | `lower(status)` |
| `SELECT a FROM orders ORDER BY 2` | 2 is not a column of the SELECT: they go from 1 to 1 | `2` |
| `SELECT 1 AS one FROM orders ORDER BY one` | Order by a field: a value orders nothing | `one` |
| `SELECT * FROM orders ORDER BY a, a DESC` | ORDER BY names a twice | `a` |
| `SELECT * FROM orders LIMIT 0` | LIMIT 0 returns no rows; MongoDB takes a limit from 1 | `0` |
| `SELECT * FROM orders LIMIT 5, 10` | Write LIMIT n OFFSET m | `5` |
| `SELECT * FROM orders LIMIT 1.5` | LIMIT takes a whole number | `1.5` |
| `SELECT * FROM orders FETCH FIRST 5 ROWS ONLY` | Write LIMIT n instead of FETCH | `SELECT * FROM orders FETCH FIRST 5 ROWS ONLY` |
| `SELECT * FROM orders WHERE _id = ObjectId('xyz')` | An ObjectId is 24 hexadecimal digits | `ObjectId('xyz')` |
| `SELECT * FROM orders WHERE at = DATE '2026-01-01'` | Write TIMESTAMP '2026-10-28T12:00:00Z' for a date, DECIMAL '9.99' for a decimal | `DATE '2026-01-01'` |
| `SELECT * FROM orders WHERE at = TIMESTAMP 'yesterday'` | yesterday is not a date: write it as 2026-10-28T12:00:00Z | `TIMESTAMP 'yesterday'` |
| `SELECT * FROM orders WHERE a = ?` | Kuery's SQL takes values, not parameters | `?` |
| `SELECT * FROM orders WHERE name LIKE 'a' ESCAPE 'ab'` | ESCAPE takes one character in quotes | `'ab'` |
| `SELECT * FROM orders WHERE name LIKE 'a!' ESCAPE '!'` | The pattern ends with its escape character | `'a!'` |
| `SELECT * FROM orders WHERE name LIKE a` | LIKE takes a pattern in quotes | `a` |
| `SELECT * FROM orders WHERE a IN (1, b)` | IN takes a list of values | `b` |
| `SELECT * FROM orders WHERE a = 99999999999999999999` | 99999999999999999999 does not fit in a 64-bit integer, and as a Double it would lose digits: write DECIMAL '99999999999999999999' | `99999999999999999999` |
| `SELECT * FROM orders WHERE a IN (1, -9223372036854775809)` | -9223372036854775809 does not fit in a 64-bit integer, and as a Double it would lose digits: write DECIMAL '-9223372036854775809' | `-9223372036854775809` |
| `SELECT CURRENT_TIMESTAMP FROM orders` | CURRENT_TIMESTAMP is a function of SQL, which Kuery's SQL does not run: for a field of that name write "CURRENT_TIMESTAMP" | `CURRENT_TIMESTAMP` |
| `SELECT * FROM orders WHERE current_date > 1` | current_date is a function of SQL, which Kuery's SQL does not run: for a field of that name write "current_date" | `current_date` |
| `SELECT a FROM orders ORDER BY LOCALTIMESTAMP` | LOCALTIMESTAMP is a function of SQL, which Kuery's SQL does not run: for a field of that name write "LOCALTIMESTAMP" | `LOCALTIMESTAMP` |
| `SELECT COUNT(SESSION_USER) FROM orders` | SESSION_USER is a function of SQL, which Kuery's SQL does not run: for a field of that name write "SESSION_USER" | `SESSION_USER` |
| `SELECT a FROM orders GROUP BY CURRENT_USER` | CURRENT_USER is a function of SQL, which Kuery's SQL does not run: for a field of that name write "CURRENT_USER" | `CURRENT_USER` |
| `SELECT * FROM orders WHERE CURRENT_TIME IS NULL OR LOCALTIME = 1` | CURRENT_TIME is a function of SQL, which Kuery's SQL does not run: for a field of that name write "CURRENT_TIME" | `CURRENT_TIME` |
| `SELECT CURRENT_CATALOG FROM orders` | CURRENT_CATALOG is a function of SQL, which Kuery's SQL does not run: for a field of that name write "CURRENT_CATALOG" | `CURRENT_CATALOG` |
| `SELECT * FROM orders WHERE a = 'open` | Unterminated string literal | `'` |
| `SELECT * FROM orders WHERE` | Expected: an expression, found: EOF | `E` |
| `SELECT * FROM orders; SELECT * FROM b` | Run one statement at a time | `SELECT * FROM b` |
| `SELECT * FROM orders WHERE a = 1e400` | 1e400 is too large for a Double: write DECIMAL '1e400' | `1e400` |
<!-- examples end -->
