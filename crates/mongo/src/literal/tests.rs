use bson::oid::ObjectId;
use bson::spec::BinarySubtype;
use bson::{Binary, Bson, JavaScriptCodeWithScope, Regex, Timestamp, doc};
use kuery_core::document::export::{Ejson, to_ejson};
use proptest::prelude::*;

use super::*;

fn read(text: &str) -> Value {
    parse_value(text).unwrap_or_else(|error| panic!("{text}: {}", error.located(text)))
}

fn ext(bson: Bson) -> Value {
    convert::value(&bson)
}

fn fields(pairs: &[(&str, Value)]) -> Value {
    Value::Object(Document(
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect(),
    ))
}

fn refused(text: &str) -> ParseError {
    parse_value(text).expect_err(text)
}

#[test]
fn numbers_keep_their_kind() {
    assert_eq!(read("42"), Value::Int32(42));
    assert_eq!(read("-2147483648"), Value::Int32(i32::MIN));
    assert_eq!(read("2147483648"), Value::Int64(2_147_483_648));
    assert_eq!(
        read("9223372036854775808"),
        Value::Double(9.223_372_036_854_776e18)
    );
    for double in ["42.0", "4.2e1", "42.", "+42.0"] {
        assert_eq!(read(double), Value::Double(42.0), "{double}");
    }
    assert_eq!(read(".5"), Value::Double(0.5));
    assert_eq!(read("0x1F"), Value::Int32(31));
    assert!(matches!(read("NaN"), Value::Double(x) if x.is_nan()));
    assert_eq!(read("-Infinity"), Value::Double(f64::NEG_INFINITY));
    assert_eq!(render(&Value::Double(42.0), Style::Compact), "42.0");
    assert_eq!(render(&Value::Double(1e300), Style::Compact), "1e300");
    assert_eq!(render(&Value::Int32(-5), Style::Compact), "-5");
}

#[test]
fn reads_every_constructor() {
    let id = ObjectId::parse_str("65f0c0ffee0000000000beef").unwrap();
    assert_eq!(
        read(r#"ObjectId("65f0c0ffee0000000000beef")"#),
        ext(Bson::ObjectId(id))
    );
    assert_eq!(
        read("new ObjectId('65f0c0ffee0000000000beef')"),
        ext(Bson::ObjectId(id))
    );
    assert_eq!(
        read(r#"ISODate("2026-10-28T12:34:56.789Z")"#),
        Value::DateTime(1_793_190_896_789)
    );
    assert_eq!(
        read(r#"new Date("2026-10-28T14:34:56.789+02:00")"#),
        Value::DateTime(1_793_190_896_789)
    );
    assert_eq!(
        read(r#"ISODate("1969-07-20")"#),
        Value::DateTime(-14_256_000_000)
    );
    assert_eq!(read("new Date(0)"), Value::DateTime(0));
    assert_eq!(read("new Date(-1)"), Value::DateTime(-1));
    assert!(matches!(read("ISODate()"), Value::DateTime(_)));
    assert_eq!(read("NumberInt(42)"), Value::Int32(42));
    assert_eq!(read("NumberInt('42')"), Value::Int32(42));
    assert_eq!(read("NumberLong(5)"), Value::Int64(5));
    assert_eq!(
        read(r#"NumberLong("9007199254740993")"#),
        Value::Int64(9_007_199_254_740_993)
    );
    assert_eq!(read("Long('-1')"), Value::Int64(-1));
    assert_eq!(
        read(r#"NumberDecimal("9.99")"#),
        Value::Decimal("9.99".into())
    );
    assert_eq!(read("NumberDecimal(-1.50)"), Value::Decimal("-1.50".into()));
    assert_eq!(read("Decimal128('1E+3')"), Value::Decimal("1E+3".into()));
    assert_eq!(read("Double(42)"), Value::Double(42.0));
    assert_eq!(
        read(r#"UUID("3b241101-e2bb-4255-8caf-4136c566a962")"#),
        Value::Binary {
            subtype: 4,
            bytes: vec![
                0x3b, 0x24, 0x11, 0x01, 0xe2, 0xbb, 0x42, 0x55, 0x8c, 0xaf, 0x41, 0x36, 0xc5, 0x66,
                0xa9, 0x62
            ]
        }
    );
    assert!(matches!(read("UUID()"), Value::Binary { subtype: 4, ref bytes } if bytes.len() == 16));
    assert_eq!(
        read(r#"BinData(0, "S3VlcnkgZGV2")"#),
        Value::Binary {
            subtype: 0,
            bytes: b"Kuery dev".to_vec()
        }
    );
    let timestamp = ext(Bson::Timestamp(Timestamp {
        time: 1_761_654_896,
        increment: 3,
    }));
    assert_eq!(read("Timestamp({t: 1761654896, i: 3})"), timestamp);
    assert_eq!(read("Timestamp(1761654896, 3)"), timestamp);
    assert_eq!(read("MinKey()"), ext(Bson::MinKey));
    assert_eq!(read("MaxKey()"), ext(Bson::MaxKey));
    assert_eq!(
        read("Code('x + 1')"),
        ext(Bson::JavaScriptCode("x + 1".into()))
    );
    assert_eq!(read("undefined"), ext(Bson::Undefined));
}

#[test]
fn reads_regular_expressions_with_sorted_flags() {
    let regex = |pattern: &str, options: &str| {
        ext(Bson::RegularExpression(Regex {
            pattern: pattern.to_string().try_into().unwrap(),
            options: options.to_string().try_into().unwrap(),
        }))
    };
    assert_eq!(
        read(r"/^cust(omer)?\d+$/i"),
        regex(r"^cust(omer)?\d+$", "i")
    );
    assert_eq!(read("/a/mi"), regex("a", "im"));
    // A slash in a class or escaped stays in the pattern.
    assert_eq!(read(r"/[/]\//"), regex(r"[/]\/", ""));
    assert_eq!(read("RegExp('a/b', 'i')"), regex("a/b", "i"));
    assert_eq!(
        render(&regex("a/b", "i"), Style::Compact),
        r#"RegExp("a/b", "i")"#
    );
    assert_eq!(render(&regex("^a", ""), Style::Compact), "/^a/");
    assert_eq!(render(&regex("*", ""), Style::Compact), r#"RegExp("*")"#);
}

#[test]
fn reads_json5() {
    let expected = fields(&[
        ("status", Value::String("paid".into())),
        ("total", fields(&[("$gt", Value::Int32(10))])),
        ("tags", Value::Array(vec![Value::String("a".into())])),
        ("say \"hi\"", Value::Bool(true)),
        ("1", Value::Null),
    ]);
    let text = r#"
        // the paid ones
        {
            status: 'paid',
            total: {$gt: 10,},  /* more than ten */
            "tags": ["a",],
            'say "hi"': true,
            1: null,
        }"#;
    assert_eq!(read(text), expected);
    assert_eq!(
        read(r#""é\x41\u{1F600}😀\n\t\\""#),
        Value::String("éA😀😀\n\t\\".into())
    );
    assert_eq!(read("'it\\'s'"), Value::String("it's".into()));
    assert_eq!(parse_document("{}").unwrap(), Document::default());
}

#[test]
fn a_document_that_looks_like_a_wrapper_reads_back_as_itself() {
    for lookalike in [
        fields(&[("$numberLong", Value::String("5".into()))]),
        fields(&[("$oid", Value::String("65f0c0ffee0000000000beef".into()))]),
        // Not even valid as the wrapper it looks like.
        fields(&[("$date", Value::String("x".into()))]),
        fields(&[
            ("$type", Value::String("00".into())),
            ("$binary", Value::Int32(1)),
        ]),
    ] {
        for style in [Style::Compact, Style::Pretty] {
            let text = render(&lookalike, style);
            assert_eq!(read(&text), lookalike, "{text}");
        }
    }
    assert_eq!(
        render(
            &fields(&[("$numberLong", Value::String("5".into()))]),
            Style::Compact
        ),
        r#"{"\u0024numberLong": "5"}"#
    );
    // Any escape of the `$` makes it a field; written plainly, a wrapper.
    assert_eq!(
        read(r#"{"\$numberLong": "5"}"#),
        fields(&[("$numberLong", Value::String("5".into()))])
    );
    assert_eq!(read(r#"{$numberLong: "5"}"#), Value::Int64(5));
    // Other keys that start with `$` print as they are.
    assert_eq!(
        render(&fields(&[("$gt", Value::Int32(1))]), Style::Compact),
        "{$gt: 1}"
    );
}

#[test]
fn reads_extended_json() {
    assert_eq!(read(r#"{"$numberLong": "5"}"#), Value::Int64(5));
    assert_eq!(read(r#"{$numberInt: "5"}"#), Value::Int32(5));
    assert_eq!(read(r#"{"$numberDouble": "42.0"}"#), Value::Double(42.0));
    assert_eq!(
        read(r#"{"$numberDecimal": "1.10"}"#),
        Value::Decimal("1.10".into())
    );
    assert_eq!(
        read(r#"{"$date": "1970-01-01T00:00:00.000Z"}"#),
        Value::DateTime(0)
    );
    assert_eq!(
        read(r#"{"$date": {"$numberLong": "-1"}}"#),
        Value::DateTime(-1)
    );
    assert_eq!(
        read(r#"{"$oid": "65f0c0ffee0000000000beef"}"#),
        read(r#"ObjectId("65f0c0ffee0000000000beef")"#)
    );
    assert_eq!(
        read(r#"{"$timestamp": {"t": 1, "i": 2}}"#),
        read("Timestamp({t: 1, i: 2})")
    );
    assert_eq!(read(r#"{"$symbol": "s"}"#), ext(Bson::Symbol("s".into())));
    // A query operator is not a wrapper.
    assert_eq!(
        read("{$regex: '^a', $options: 'i'}"),
        fields(&[
            ("$regex", Value::String("^a".into())),
            ("$options", Value::String("i".into()))
        ])
    );
    assert_eq!(
        read("{$type: 'string'}"),
        fields(&[("$type", Value::String("string".into()))])
    );
    let broken = refused(r#"{"$oid": "nope"}"#);
    assert!(
        broken.message.starts_with("Not valid Extended JSON"),
        "{broken:?}"
    );
}

#[test]
fn errors_say_where() {
    let text = "{status: 'paid',\n total: }";
    let error = parse_document(text).unwrap_err();
    assert_eq!(error.span, 25..26);
    assert_eq!(error.located(text), "line 2, column 9: Expected a value");
    let error = refused("{a: paid}");
    assert_eq!(error.span, 4..8);
    assert!(error.message.contains("\"paid\""), "{}", error.message);
    assert_eq!(refused("{a: 1").message, "This document never ends: add }");
    assert_eq!(refused("'abc").message, "This string never ends");
    assert_eq!(refused("[1 2]").message, "Expected , or ] after the value");
    assert_eq!(refused("{a: 1, a: 2}").message, "The field a appears twice");
    assert_eq!(refused("{a: 1} x").span, 7..8);
    assert_eq!(refused("/* never").message, "This comment never ends");
    assert_eq!(refused("Foo(1)").message, "Foo() is not a type Kuery reads");
    assert_eq!(
        refused("NumberInt(1.5)").message,
        "NumberInt() takes a whole number that fits in 32 bits"
    );
    assert!(
        refused("ObjectId('xyz')")
            .message
            .contains("24 hexadecimal")
    );
    assert!(
        refused("ISODate('2026-02-30')")
            .message
            .contains("is not a date")
    );
    assert!(refused("Date()").message.contains("Date() is not a type"));
    let error = parse_document("[1]").unwrap_err();
    assert_eq!(
        (error.span, error.message.starts_with("Expected a document")),
        (0..3, true)
    );
}

#[test]
fn prints_typed_literals() {
    let order = fields(&[
        ("_id", read("ObjectId('65f0c0ffee0000000000beef')")),
        ("n", Value::Int64(5)),
        ("total", Value::Decimal("9.99".into())),
        ("at", Value::DateTime(1_793_188_800_000)),
        ("old", Value::DateTime(-62_167_219_200_001)),
        ("id", read("UUID('3b241101-e2bb-4255-8caf-4136c566a962')")),
        (
            "raw",
            Value::Binary {
                subtype: 0,
                bytes: b"Kuery dev".to_vec(),
            },
        ),
        ("gift.note", Value::String("a \"b\"\n".into())),
        ("items", Value::Array(vec![Value::Int32(1), fields(&[])])),
        ("empty", Value::Array(Vec::new())),
    ]);
    assert_eq!(
        render(&order, Style::Compact),
        concat!(
            r#"{_id: ObjectId("65f0c0ffee0000000000beef"), n: NumberLong("5"), "#,
            r#"total: NumberDecimal("9.99"), at: ISODate("2026-10-28T12:00:00.000Z"), "#,
            r#"old: new Date(-62167219200001), id: UUID("3b241101-e2bb-4255-8caf-4136c566a962"), "#,
            r#"raw: BinData(0, "S3VlcnkgZGV2"), "gift.note": "a \"b\"\n", items: [1, {}], empty: []}"#,
        )
    );
    let nested = fields(&[(
        "a",
        Value::Array(vec![Value::Int32(1), fields(&[("b", Value::Null)])]),
    )]);
    assert_eq!(
        render(&nested, Style::Pretty),
        "{\n  a: [\n    1,\n    {\n      b: null\n    }\n  ]\n}"
    );
    assert_eq!(
        render(&read("{$undefined: true}"), Style::Compact),
        "{$undefined: true}"
    );
}

#[test]
fn filters_that_run_javascript_are_heavy() {
    assert!(!check_filter("{status: 'paid'}").unwrap().heavy);
    assert!(!check_filter("  ").unwrap().heavy);
    assert!(
        check_filter("{$where: 'sleep(100) || true'}")
            .unwrap()
            .heavy
    );
    assert!(
        check_filter("{$expr: {$function: {body: 'x', args: [], lang: 'js'}}}")
            .unwrap()
            .heavy
    );
    assert!(
        check_filter("{$or: [{a: 1}, {$where: 'true'}]}")
            .unwrap()
            .heavy
    );
    assert!(check_filter("{a: ").is_err());
}

#[test]
fn every_engine_type_prints_and_reads_back() {
    let id = ObjectId::parse_str("65f0c0ffee0000000000beef").unwrap();
    let pointer = Bson::try_from(serde_json::json!({
        "$dbPointer": {"$ref": "shop.orders", "$id": {"$oid": "65f0c0ffee0000000000beef"}}
    }))
    .unwrap();
    let values = [
        Bson::ObjectId(id),
        Bson::Timestamp(Timestamp {
            time: 0,
            increment: u32::MAX,
        }),
        Bson::MinKey,
        Bson::MaxKey,
        Bson::Symbol("s".into()),
        Bson::JavaScriptCode("function () { return 1 }".into()),
        Bson::JavaScriptCodeWithScope(JavaScriptCodeWithScope {
            code: "x".into(),
            scope: doc! { "x": 1_i64 },
        }),
        pointer,
        Bson::Undefined,
        Bson::Binary(Binary {
            subtype: BinarySubtype::UserDefined(0x80),
            bytes: vec![1, 2],
        }),
    ];
    for bson in values {
        let value = convert::value(&bson);
        for style in [Style::Compact, Style::Pretty] {
            let text = render(&value, style);
            assert_eq!(read(&text), value, "{text}");
        }
        assert_eq!(convert::bson(&value).unwrap(), bson);
    }
}

// The property: printing and reading back, and BSON and back, give the
// same value, for every type.

/// Values equal down to the bits of a double (`NaN` is itself, `-0.0` is
/// not `0.0`), through their canonical Extended JSON.
fn same(a: &Value, b: &Value) -> bool {
    to_ejson(a, Ejson::Canonical) == to_ejson(b, Ejson::Canonical) && kinds(a) == kinds(b)
}

/// The types, which canonical Extended JSON already tells apart, and the
/// display of engine types, which it leaves out.
fn kinds(value: &Value) -> String {
    match value {
        Value::Array(items) => items.iter().map(kinds).collect::<Vec<_>>().join(","),
        Value::Object(document) => document
            .0
            .iter()
            .map(|(name, value)| format!("{name}:{}", kinds(value)))
            .collect::<Vec<_>>()
            .join(","),
        Value::Ext(ext) => format!("{}{}", ext.kind, ext.display),
        other => other.type_name().into(),
    }
}

fn double() -> impl Strategy<Value = f64> {
    prop_oneof![
        any::<f64>(),
        Just(f64::NAN),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
        Just(-0.0),
        Just(42.0),
        (-1_000_000i64..1_000_000).prop_map(|n| n as f64),
    ]
}

fn decimal() -> impl Strategy<Value = String> {
    prop_oneof![
        (any::<i64>(), -40i32..40).prop_map(|(digits, exponent)| format!("{digits}E{exponent}")),
        Just("NaN".to_string()),
        Just("-Infinity".to_string()),
        Just("-0.00".to_string()),
        Just("1234567890.123456789".to_string()),
    ]
    .prop_map(|text| text.parse::<Decimal128>().unwrap().to_string())
}

/// Text BSON can hold anywhere a C string goes (field names, patterns).
fn c_text() -> impl Strategy<Value = String> {
    any::<String>().prop_map(|text| text.replace('\0', ""))
}

fn engine_type() -> impl Strategy<Value = Value> {
    let bson = prop_oneof![
        any::<[u8; 12]>().prop_map(|bytes| Bson::ObjectId(ObjectId::from_bytes(bytes))),
        (any::<u32>(), any::<u32>())
            .prop_map(|(time, increment)| Bson::Timestamp(Timestamp { time, increment })),
        (c_text(), proptest::sample::subsequence(vec!['i', 'l', 'm', 's', 'u', 'x'], 0..=6))
            .prop_map(|(pattern, flags)| Bson::RegularExpression(Regex {
                pattern: pattern.try_into().unwrap(),
                options: flags.into_iter().collect::<String>().try_into().unwrap(),
            })),
        prop_oneof![
            Just("/".to_string()),
            Just(r"\".to_string()),
            Just("a\nb".to_string()),
            Just("[/]".to_string()),
            Just("*a".to_string()),
            Just(r"^cust(omer)?\d+$".to_string()),
        ]
        .prop_map(|pattern| Bson::RegularExpression(Regex {
            pattern: pattern.try_into().unwrap(),
            options: "i".to_string().try_into().unwrap(),
        })),
        Just(Bson::MinKey),
        Just(Bson::MaxKey),
        any::<String>().prop_map(Bson::Symbol),
        any::<String>().prop_map(Bson::JavaScriptCode),
        (any::<String>(), any::<i64>()).prop_map(|(code, x)| Bson::JavaScriptCodeWithScope(
            JavaScriptCodeWithScope { code, scope: doc! { "x": x, "y": [1.5, "z"] } }
        )),
        (any::<[u8; 12]>(), "[a-z.]{1,12}").prop_map(|(id, namespace)| Bson::try_from(
            serde_json::json!({"$dbPointer": {"$ref": namespace, "$id": {"$oid": ObjectId::from_bytes(id).to_hex()}}})
        )
        .unwrap()),
        Just(Bson::Undefined),
    ];
    bson.prop_map(|bson| convert::value(&bson))
}

fn leaf() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i32>().prop_map(Value::Int32),
        any::<i64>().prop_map(Value::Int64),
        prop_oneof![Just(i64::MAX), Just(i64::MIN), Just((1 << 53) + 1)].prop_map(Value::Int64),
        double().prop_map(Value::Double),
        decimal().prop_map(Value::Decimal),
        any::<String>().prop_map(Value::String),
        (any::<u8>(), proptest::collection::vec(any::<u8>(), 0..24))
            .prop_map(|(subtype, bytes)| Value::Binary { subtype, bytes }),
        any::<[u8; 16]>().prop_map(|bytes| Value::Binary {
            subtype: 4,
            bytes: bytes.to_vec()
        }),
        any::<i64>().prop_map(Value::DateTime),
        (-62_167_219_200_000i64..=253_402_300_799_999).prop_map(Value::DateTime),
        engine_type(),
    ]
}

/// Field names: plain, any text, and the keys of Extended JSON wrappers.
fn name() -> impl Strategy<Value = String> {
    prop_oneof![
        prop_oneof![Just("_id"), Just("status"), Just("total"), Just("a")].prop_map(String::from),
        "[a-zA-Z_$][a-zA-Z0-9_$]{0,8}",
        c_text(),
        wrapper_key(),
    ]
}

fn wrapper_key() -> impl Strategy<Value = String> {
    let keys: Vec<&str> = WRAPPERS
        .iter()
        .flat_map(|keys| keys.iter().copied())
        .collect();
    proptest::sample::select(keys).prop_map(String::from)
}

/// A document that only looks like an Extended JSON wrapper: the keys of
/// one, holding any values.
fn lookalike(inner: impl Strategy<Value = Value> + Clone) -> impl Strategy<Value = Value> {
    (
        proptest::sample::select(WRAPPERS),
        proptest::collection::vec(inner, 2),
    )
        .prop_map(|(keys, values)| {
            Value::Object(Document(
                keys.iter()
                    .zip(values)
                    .map(|(key, value)| (key.to_string(), value))
                    .collect(),
            ))
        })
}

fn value() -> impl Strategy<Value = Value> {
    leaf().prop_recursive(4, 48, 6, |inner| {
        prop_oneof![
            lookalike(inner.clone()),
            proptest::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            proptest::collection::vec((name(), inner), 0..6).prop_map(|fields| {
                let mut unique: Vec<(String, Value)> = Vec::new();
                for (name, value) in fields {
                    if !unique.iter().any(|(seen, _)| *seen == name) {
                        unique.push((name, value));
                    }
                }
                Value::Object(Document(unique))
            }),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn printing_and_reading_back_gives_the_same_value(value in value()) {
        for style in [Style::Compact, Style::Pretty] {
            let text = render(&value, style);
            let read = parse_value(&text)
                .map_err(|error| TestCaseError::fail(format!("{text}: {}", error.located(&text))))?;
            prop_assert!(same(&read, &value), "{text}\nread back as {read:?}");
        }
    }

    #[test]
    fn bson_and_back_gives_the_same_value(value in value()) {
        let bson = convert::bson(&value).map_err(TestCaseError::fail)?;
        let back = convert::value(&bson);
        prop_assert!(same(&back, &value), "{value:?}\ncame back as {back:?}");
    }
}

#[test]
fn filters_by_a_field_with_its_type() {
    let filter =
        |path: &str, value: Option<&Value>| field_filter(&FieldPath::parse(path), value).unwrap();
    assert_eq!(
        filter("items.0.sku", Some(&Value::String("sku-001".into()))),
        r#"{"items.0.sku": "sku-001"}"#
    );
    assert_eq!(
        filter("total", Some(&Value::Decimal("9.99".into()))),
        r#"{total: NumberDecimal("9.99")}"#
    );
    assert_eq!(
        filter("n", Some(&Value::Int64(5))),
        r#"{n: NumberLong("5")}"#
    );
    // A null finds only nulls; a missing field, only documents without it.
    assert_eq!(
        filter("coupon", Some(&Value::Null)),
        r#"{coupon: {$type: "null"}}"#
    );
    assert_eq!(filter("coupon", None), r#"{coupon: {$exists: false}}"#);
}

#[test]
fn nesting_too_deep_is_an_error_not_a_crash() {
    let error = refused(&"[".repeat(100_000));
    assert_eq!(
        (error.message.as_str(), error.span),
        ("Values nest deeper than 200 levels here", 200..201)
    );
    let document = format!("{{a: {}", "{a: ".repeat(100_000));
    let error = parse_document(&document).expect_err("too deep");
    assert!(error.message.contains("200 levels"), "{error}");
    assert!(check_filter(&document).is_err());
    // As deep as the limit still reads.
    let deepest = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
    assert!(parse_value(&deepest).is_ok());
    let deeper = format!("[{deepest}]");
    assert!(parse_value(&deeper).is_err());
}

#[test]
fn finds_keys_at_any_depth_without_recursing() {
    let mut deep = fields(&[("$where", Value::String("true".into()))]);
    for _ in 0..100_000 {
        deep = Value::Array(vec![deep]);
    }
    assert!(runs_javascript(&deep));
    // Dropping a value this deep recurses: the test leaks it instead.
    std::mem::forget(deep);
}
