// Sample MongoDB database for Kuery: a small shop in the `shop` database,
// with what documents do that rows do not (absent fields next to null ones,
// mixed types, nested arrays, a key holding a dot), a view, a time series,
// a capped collection, every BSON type the literal grammar reads, and a
// collection big enough to page through and to cancel. Run with mongosh
// against `shop`; it starts the database over each time.

db = db.getSiblingDB("shop");
db.dropDatabase();

const statuses = ["pending", "paid", "shipped", "cancelled"];
const countries = ["ES", "PT", "FR", "DE", "US", "MX"];

db.createCollection("customers", {
  validator: {
    $jsonSchema: {
      bsonType: "object",
      required: ["name", "email", "country"],
      properties: {
        name: { bsonType: "string" },
        email: { bsonType: "string", pattern: "@" },
        country: { bsonType: "string", minLength: 2, maxLength: 2 },
        since: { bsonType: "date" },
      },
    },
  },
});
const customers = [];
for (let i = 1; i <= 60; i++) {
  customers.push({
    _id: new ObjectId(),
    name: `Customer ${i}`,
    email: `customer${i}@example.com`,
    country: countries[i % countries.length],
    since: new Date(Date.UTC(2024, i % 12, (i % 28) + 1)),
    ...(i % 4 === 0 ? { tags: ["vip", "newsletter"] } : {}),
  });
}
db.customers.insertMany(customers);
db.customers.createIndex({ email: 1 }, { unique: true });

// String keys, and a price that is a String in about one product in ten.
const products = [];
for (let i = 1; i <= 40; i++) {
  const price = Math.round((4.5 + ((i * 37) % 200) + (i % 7) / 10 + 0.05) * 100) / 100;
  products.push({
    _id: `sku-${String(i).padStart(3, "0")}`,
    name: `Product ${i}`,
    price: i % 10 === 0 ? price.toFixed(2) : Double(price),
    stock: NumberInt((i * 13) % 250),
    category: `cat-${i % 6}`,
  });
}
db.products.insertMany(products);

const orders = [];
for (let i = 1; i <= 500; i++) {
  const items = [];
  let total = 0;
  for (let j = 0; j < (i % 4) + 1; j++) {
    const product = products[(i * 7 + j * 3) % products.length];
    const price = Number(product.price);
    const qty = (i + j) % 3 + 1;
    total += price * qty;
    items.push({ sku: product._id, qty: NumberInt(qty), price: Double(price) });
  }
  const order = {
    number: NumberInt(i),
    customer: customers[i % customers.length]._id,
    status: statuses[i % statuses.length],
    total: NumberDecimal(total.toFixed(2)),
    date: new Date(Date.UTC(2026, (i * 5) % 12, (i % 28) + 1, i % 24, (i * 7) % 60)),
    items,
  };
  // Absent in most, null in some, a code in others.
  if (i % 10 === 3) order.coupon = null;
  if (i % 10 === 7) order.coupon = `SAVE${i % 30}`;
  orders.push(order);
}
db.orders.insertMany(orders);
db.orders.createIndex({ status: 1, date: -1 });
db.orders.createIndex({ customer: 1 });
// A field named like a SQL keyword, and a key holding a dot.
db.orders.updateOne({ number: 1 }, { $set: { order: "first" } });
db.orders.updateOne({ number: 2 }, [
  { $replaceWith: { $setField: { field: "gift.note", input: "$$ROOT", value: "Happy birthday" } } },
]);

db.createView("paid_orders", "orders", [
  { $match: { status: "paid" } },
  { $project: { number: 1, customer: 1, total: 1, date: 1 } },
]);

db.createCollection("readings", {
  timeseries: { timeField: "at", metaField: "sensor", granularity: "minutes" },
});
const readings = [];
for (let i = 0; i < 300; i++) {
  readings.push({
    at: new Date(Date.UTC(2026, 9, 1) + i * 60_000),
    sensor: { id: `s-${i % 3}`, room: ["lab", "office", "store"][i % 3] },
    celsius: Double(18 + (i % 50) / 10),
  });
}
db.readings.insertMany(readings);

db.createCollection("events", { capped: true, size: 1_048_576, max: 1000 });
const events = [];
for (let i = 1; i <= 200; i++) {
  events.push({ at: new Date(Date.UTC(2026, 9, 28, 8) + i * 1000), kind: i % 5 === 0 ? "error" : "info", message: `event ${i}` });
}
db.events.insertMany(events);

// One document per BSON type the grid, the editor and the console must keep.
db.types.insertMany([
  { _id: "long", value: NumberLong("9007199254740993"), note: "above 2^53" },
  { _id: "int", value: NumberInt(42) },
  { _id: "double", value: Double(42) },
  { _id: "decimal", value: NumberDecimal("1234567890.123456789") },
  { _id: "date", value: ISODate("2026-10-28T12:34:56.789Z") },
  { _id: "before 1970", value: ISODate("1969-07-20T20:17:40Z") },
  { _id: "object id", value: ObjectId("65f0c0ffee0000000000beef") },
  { _id: "uuid", value: UUID("3b241101-e2bb-4255-8caf-4136c566a962") },
  { _id: "binary", value: BinData(0, "S3VlcnkgZGV2") },
  { _id: "timestamp", value: Timestamp({ t: 1761654896, i: 3 }) },
  { _id: "regex", value: /^cust(omer)?\d+$/i },
  { _id: "min key", value: MinKey() },
  { _id: "max key", value: MaxKey() },
  { _id: "null", value: null },
  { _id: "absent" },
  { _id: "nested", value: { a: { b: [1, { c: true }] } } },
]);

// Small documents, enough to page through and to keep a slow read busy.
for (let start = 0; start < 20_000; start += 5_000) {
  const batch = [];
  for (let i = start; i < start + 5_000; i++) {
    batch.push({ _id: NumberInt(i), n: NumberInt(i % 100), label: `row ${i}` });
  }
  db.big.insertMany(batch);
}
