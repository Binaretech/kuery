use std::fs::{self, File};
use std::path::Path;

use kuery_core::compare::{self, CompareOptions, Comparison, Outcome};
use kuery_core::export::{self, Format};
use kuery_core::result_set::{BatchBuilder, ColumnKind, ResultSet};
use kuery_core::stats::{self, Mode, Options, Summary};
use kuery_files::{
    Error, FileDataset, FileFormat, FileSource, LimitReason, MAX_JSON_BYTES, MAX_PREVIEW_ROWS,
    PreviewRequest, PreviewResult, PreviewScope,
};
use polars::prelude::*;
use tempfile::TempDir;

fn text_file(dir: &TempDir, name: &str, text: &str) -> FileSource {
    let path = dir.path().join(name);
    fs::write(&path, text).unwrap();
    FileSource::new(path).unwrap()
}

fn preview(source: FileSource) -> PreviewResult {
    FileDataset::open(source)
        .unwrap()
        .preview(PreviewRequest::default())
        .unwrap()
}

fn parquet(dir: &TempDir, name: &str, mut frame: DataFrame) -> FileSource {
    let path = dir.path().join(name);
    ParquetWriter::new(File::create(&path).unwrap())
        .finish(&mut frame)
        .unwrap();
    FileSource::new(path).unwrap()
}

#[test]
fn all_formats_produce_the_same_result_and_feed_existing_statistics() {
    let dir = TempDir::new().unwrap();
    let sources = [
        text_file(&dir, "data.csv", "amount,group\n1,a\n3,a\n5,b\n,b\n"),
        text_file(
            &dir,
            "data.json",
            r#"[{"amount":1,"group":"a"},{"amount":3,"group":"a"},{"amount":5,"group":"b"},{"amount":null,"group":"b"}]"#,
        ),
        text_file(
            &dir,
            "data.jsonl",
            "{\"amount\":1,\"group\":\"a\"}\n{\"amount\":3,\"group\":\"a\"}\n{\"amount\":5,\"group\":\"b\"}\n{\"amount\":null,\"group\":\"b\"}\n",
        ),
        parquet(
            &dir,
            "data.parquet",
            df!("amount" => [Some(1_i64), Some(3), Some(5), None], "group" => ["a", "a", "b", "b"])
                .unwrap(),
        ),
    ];
    for source in sources {
        let output = preview(source);
        let set = &output.results;
        assert_eq!(output.scope, PreviewScope::Complete);
        assert_eq!(set.columns(), ["amount", "group"]);
        assert_eq!(set.kinds(), [ColumnKind::Integer, ColumnKind::Text]);
        assert_eq!(set.rows(), 4);
        assert_eq!(set.cell(0, 0), Some(Some("1")));
        assert_eq!(set.cell(3, 0), Some(None));
        let overview = stats::overview(set);
        assert_eq!(
            (overview[0].rows, overview[0].nulls, overview[0].distinct),
            (4, 1, 3)
        );
        let options = Options {
            mode: Mode::Numeric,
            ..Default::default()
        };
        let Summary::Numeric(summary) = stats::analyze(set, 0, &options).summary else {
            panic!("numeric summary");
        };
        assert_eq!(summary.mean, 3.0);
        let Outcome::Groups(groups) = compare::compare(
            set,
            Comparison::Groups {
                measure: 0,
                group: 1,
            },
            &CompareOptions::default(),
        ) else {
            panic!("groups");
        };
        assert_eq!(groups.overall.n, 3);
        assert_eq!(groups.overall.mean, 3.0);
        assert!(groups.groups.iter().any(|g| g.n == 2 && g.mean == 2.0));
    }
}

#[test]
fn csv_handles_bom_unicode_quotes_and_embedded_newlines() {
    let dir = TempDir::new().unwrap();
    let output = preview(text_file(
        &dir,
        "data.csv",
        "\u{feff}id,name\n1,\"Mérida, VE\"\n2,\"line one\nline two\"\n",
    ));
    assert_eq!(output.results.columns(), ["id", "name"]);
    assert_eq!(output.results.cell(0, 1), Some(Some("Mérida, VE")));
    assert_eq!(output.results.cell(1, 1), Some(Some("line one\nline two")));
}

#[test]
fn csv_options_and_empty_values_are_explicit() {
    let dir = TempDir::new().unwrap();
    let mut source = text_file(&dir, "data.csv", "id;value\n1;\n2;\"\"\n3;NULL\n4;text\n");
    source.csv.separator = b';';
    source.csv.null_value = Some("NULL".into());
    let output = preview(source.clone());
    assert_eq!(output.results.cell(0, 1), Some(None));
    assert_eq!(output.results.cell(1, 1), Some(Some("")));
    assert_eq!(output.results.cell(2, 1), Some(None));
    source.csv.empty_is_null = false;
    assert_eq!(preview(source).results.cell(0, 1), Some(Some("")));
}

#[test]
fn an_exported_single_column_reads_back_every_row() {
    let dir = TempDir::new().unwrap();
    let mut set = ResultSet::new(vec!["note".into()]);
    let rows = [Some("a"), None, Some("")];
    let mut builder = BatchBuilder::new(1, rows.len());
    for row in rows {
        builder.push_row(|_| row);
    }
    set.push(builder.finish());
    for name in ["notes.csv", "notes.tsv"] {
        let mut out = Vec::new();
        export::write(&set, Format::from_path(Path::new(name)), &mut out).unwrap();
        let read = preview(text_file(&dir, name, &String::from_utf8(out).unwrap())).results;
        assert_eq!(read.rows(), 3, "{name}");
        assert_eq!(read.cell(1, 0), Some(Some("")), "{name}");
    }
}

#[test]
fn csv_without_header_and_duplicate_headers_have_unique_stable_names() {
    let dir = TempDir::new().unwrap();
    let mut source = text_file(&dir, "no-header.tsv", "1\tx\n2\ty\n");
    assert_eq!(source.csv.separator, b'\t');
    source.csv.has_header = false;
    assert_eq!(preview(source).results.columns(), ["column_1", "column_2"]);
    let source = text_file(&dir, "duplicate.csv", "id,id\n1,2\n");
    let a = preview(source.clone());
    let b = preview(source);
    assert_eq!(a.results.columns(), b.results.columns());
    assert_ne!(a.results.columns()[0], a.results.columns()[1]);
}

#[test]
fn csv_rejects_malformed_rows_and_late_type_conflicts() {
    let dir = TempDir::new().unwrap();
    let source = text_file(&dir, "bad.csv", "id,value\n1,a,extra\n");
    assert!(
        FileDataset::open(source)
            .unwrap()
            .preview(PreviewRequest::default())
            .is_err()
    );
    let mut text = String::from("id\n");
    for _ in 0..1_000 {
        text.push_str("1\n");
    }
    text.push_str("oops\n");
    let source = text_file(&dir, "late.csv", &text);
    assert!(
        FileDataset::open(source)
            .unwrap()
            .preview(PreviewRequest::default())
            .is_err()
    );
}

#[test]
fn row_limit_distinguishes_exact_end_from_a_prefix() {
    let dir = TempDir::new().unwrap();
    for (name, data) in [
        ("rows.csv", "id\n1\n2\n3\n"),
        ("rows.json", "[{\"id\":1},{\"id\":2},{\"id\":3}]"),
        ("rows.ndjson", "{\"id\":1}\n{\"id\":2}\n{\"id\":3}\n"),
    ] {
        let dataset = FileDataset::open(text_file(&dir, name, data)).unwrap();
        for limit in [2, 3, 4] {
            let output = dataset
                .preview(PreviewRequest {
                    row_limit: limit,
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(output.results.rows(), limit.min(3));
            assert_eq!(
                output.scope,
                if limit == 2 {
                    PreviewScope::PrefixLimited {
                        loaded_rows: 2,
                        row_limit: 2,
                        reason: LimitReason::Rows,
                    }
                } else {
                    PreviewScope::Complete
                }
            );
        }
    }
    let dataset = FileDataset::open(parquet(
        &dir,
        "rows.parquet",
        df!("id" => [1, 2, 3]).unwrap(),
    ))
    .unwrap();
    assert!(matches!(
        dataset
            .preview(PreviewRequest {
                row_limit: 2,
                ..Default::default()
            })
            .unwrap()
            .scope,
        PreviewScope::PrefixLimited {
            reason: LimitReason::Rows,
            ..
        }
    ));
}

#[test]
fn byte_budget_measures_actual_arrow_allocations() {
    let dir = TempDir::new().unwrap();
    let data = format!("value\n{}", "abcdefghijklmnop\n".repeat(600));
    let dataset = FileDataset::open(text_file(&dir, "budget.csv", &data)).unwrap();
    let output = dataset
        .preview(PreviewRequest {
            byte_limit: 1_024,
            ..Default::default()
        })
        .unwrap();
    assert!(output.results.rows() > 0 && output.results.rows() < 600);
    assert!(output.results.memory_size() <= 1_024);
    assert!(matches!(
        output.scope,
        PreviewScope::PrefixLimited {
            reason: LimitReason::Bytes,
            ..
        }
    ));
    assert!(matches!(
        dataset.preview(PreviewRequest {
            byte_limit: 1,
            ..Default::default()
        }),
        Err(Error::RowTooLarge(1))
    ));
    let source = text_file(
        &dir,
        "large-cell.csv",
        &format!("value\n{}\n", "x".repeat(2_000)),
    );
    assert!(matches!(
        FileDataset::open(source).unwrap().preview(PreviewRequest {
            byte_limit: 1_024,
            ..Default::default()
        }),
        Err(Error::RowTooLarge(1_024))
    ));
}

#[test]
fn json_missing_fields_and_nested_values_keep_their_meaning() {
    let dir = TempDir::new().unwrap();
    let output = preview(text_file(
        &dir,
        "nested.json",
        r#"[{"id":1,"nested":{"tag":"x","items":[1,null,3]}},{"id":2}]"#,
    ));
    assert_eq!(output.results.cell(1, 1), Some(None));
    let cell = output.results.cell(0, 1).unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(cell).unwrap(),
        serde_json::json!({"tag":"x","items":[1,null,3]})
    );
    assert_eq!(output.schema[1].kind, ColumnKind::Text);
    assert!(output.schema[1].native_type.contains("struct"));
}

#[test]
fn invalid_json_shapes_and_syntax_are_errors() {
    let dir = TempDir::new().unwrap();
    for (i, data) in ["{}", "null", "42", "[1,2]", "[{\"x\":1},null]", "[{"]
        .iter()
        .enumerate()
    {
        let source = text_file(&dir, &format!("bad{i}.json"), data);
        assert!(
            FileDataset::open(source)
                .unwrap()
                .preview(PreviewRequest::default())
                .is_err(),
            "{data}"
        );
    }
    for (i, data) in ["1\n", "{}\nnot json\n", "[]\n"].iter().enumerate() {
        let source = text_file(&dir, &format!("bad{i}.jsonl"), data);
        assert!(
            FileDataset::open(source)
                .unwrap()
                .preview(PreviewRequest::default())
                .is_err()
        );
    }
}

#[test]
fn json_late_conflicts_and_new_fields_are_not_silently_dropped() {
    let dir = TempDir::new().unwrap();
    for suffix in [r#"{"x":"oops"}"#, r#"{"x":1,"new":2}"#, r#"{"x":true}"#] {
        let mut rows = vec![r#"{"x":1}"#; 1_000];
        rows.push(suffix);
        for (name, data) in [
            ("late.json", format!("[{}]", rows.join(","))),
            ("late.jsonl", rows.join("\n")),
        ] {
            let source = text_file(&dir, name, &data);
            assert!(
                FileDataset::open(source)
                    .unwrap()
                    .preview(PreviewRequest::default())
                    .is_err(),
                "{name}: {suffix}"
            );
        }
    }
}

#[test]
fn large_integers_and_float_precision_survive_conversion() {
    let dir = TempDir::new().unwrap();
    for (name, data) in [
        (
            "precise.csv",
            "id,value\n9007199254740993,1.2345678901234567\n",
        ),
        (
            "precise.json",
            "[{\"id\":9007199254740993,\"value\":1.2345678901234567}]",
        ),
    ] {
        let output = preview(text_file(&dir, name, data));
        assert_eq!(output.results.cell(0, 0), Some(Some("9007199254740993")));
        assert_eq!(output.results.cell(0, 1), Some(Some("1.2345678901234567")));
    }
    let source = text_file(
        &dir,
        "rounded.json",
        "[{\"id\":9007199254740993},{\"id\":1.5}]",
    );
    assert!(
        FileDataset::open(source)
            .unwrap()
            .preview(PreviewRequest::default())
            .is_err()
    );
    let output = preview(text_file(&dir, "mixed.csv", "id\n9007199254740993\n1.5\n"));
    assert_eq!(output.results.cell(0, 0), Some(Some("9007199254740993")));
    assert_eq!(output.results.cell(1, 0), Some(Some("1.5")));
}

#[test]
fn parquet_preserves_decimal_temporal_boolean_and_binary_types() {
    let dir = TempDir::new().unwrap();
    let decimal = Series::new("decimal".into(), [12345_i64])
        .cast(&DataType::Decimal(18, 2))
        .unwrap();
    let date = Series::new("date".into(), [0_i32])
        .cast(&DataType::Date)
        .unwrap();
    let timestamp = Series::new("timestamp".into(), [0_i64])
        .cast(&DataType::Datetime(
            TimeUnit::Microseconds,
            Some(TimeZone::UTC),
        ))
        .unwrap();
    let binary = Series::new("binary".into(), [b"\x00\xff".as_slice()]);
    let frame = DataFrame::new(
        1,
        vec![
            decimal.into(),
            date.into(),
            timestamp.into(),
            binary.into(),
            Series::new("bool".into(), [true]).into(),
        ],
    )
    .unwrap();
    let output = preview(parquet(&dir, "types.parquet", frame));
    assert_eq!(output.results.cell(0, 0), Some(Some("12345.00")));
    assert_eq!(output.results.cell(0, 1), Some(Some("1970-01-01")));
    assert_eq!(
        output.results.cell(0, 2),
        Some(Some("1970-01-01T00:00:00+00:00"))
    );
    assert_eq!(
        stats::parse_timestamp(output.results.cell(0, 2).unwrap().unwrap()),
        Some(0.0)
    );
    assert_eq!(output.results.cell(0, 3), Some(Some("0x00ff")));
    assert_eq!(output.results.cell(0, 4), Some(Some("true")));
    assert_eq!(
        output.results.kinds(),
        [
            ColumnKind::Number,
            ColumnKind::Date,
            ColumnKind::Timestamp,
            ColumnKind::Text,
            ColumnKind::Boolean
        ]
    );
    assert!(output.schema.iter().all(|c| !c.inferred));
}

#[test]
fn json_rejects_out_of_range_integer_literals_without_touching_quoted_numbers() {
    let dir = TempDir::new().unwrap();
    for literal in ["18446744073709551617", "-9223372036854775809"] {
        for (name, data) in [
            ("overflow.json", format!("[{{\"id\":{literal}}}]")),
            ("overflow.jsonl", format!("{{\"id\":{literal}}}\n")),
        ] {
            let source = text_file(&dir, name, &data);
            let error = FileDataset::open(source)
                .unwrap()
                .preview(PreviewRequest::default())
                .unwrap_err();
            assert!(error.to_string().contains("64-bit range"), "{error}");
        }
    }
    let output = preview(text_file(
        &dir,
        "quoted.json",
        r#"[{"text":"\"18446744073709551617","id":18446744073709551615}]"#,
    ));
    assert_eq!(
        output.results.cell(0, 0),
        Some(Some("\"18446744073709551617"))
    );
    assert_eq!(
        output.results.cell(0, 1),
        Some(Some("18446744073709551615"))
    );
}

#[test]
fn empty_inputs_have_complete_empty_previews() {
    let dir = TempDir::new().unwrap();
    for (name, data) in [
        ("empty.csv", ""),
        ("header.csv", "id,name\n"),
        ("empty.json", "[]"),
        ("empty.jsonl", ""),
        ("blank.jsonl", "\n \n"),
    ] {
        let output = preview(text_file(&dir, name, data));
        assert_eq!(output.results.rows(), 0);
        assert_eq!(output.scope, PreviewScope::Complete);
    }
    let output = preview(parquet(
        &dir,
        "empty.parquet",
        df!("id" => Vec::<i64>::new()).unwrap(),
    ));
    assert_eq!(output.results.rows(), 0);
    assert_eq!(output.results.columns(), ["id"]);
}

#[test]
fn parquet_non_utc_timestamps_keep_local_time_and_original_zone() {
    let dir = TempDir::new().unwrap();
    let zone = TimeZone::opt_try_new(Some("America/New_York")).unwrap();
    let timestamp = Series::new("timestamp".into(), [0_i64])
        .cast(&DataType::Datetime(TimeUnit::Microseconds, zone))
        .unwrap();
    let frame = DataFrame::new(1, vec![timestamp.into()]).unwrap();
    let output = preview(parquet(&dir, "timezone.parquet", frame));
    assert!(output.schema[0].native_type.contains("America/New_York"));
    let value = output.results.cell(0, 0).unwrap().unwrap();
    assert_eq!(value, "1969-12-31T19:00:00-05:00");
    assert_eq!(stats::parse_timestamp(value), Some(0.0));
}

#[test]
fn json_empty_objects_preserve_row_count() {
    let dir = TempDir::new().unwrap();
    for (name, data) in [("objects.json", "[{},{}]"), ("objects.jsonl", "{}\n{}\n")] {
        let output = preview(text_file(&dir, name, data));
        assert_eq!(output.results.rows(), 2);
        assert!(output.schema.is_empty());
    }
}

#[test]
fn rejects_oversized_json_before_parsing() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("large.json");
    File::create(&path)
        .unwrap()
        .set_len(MAX_JSON_BYTES + 1)
        .unwrap();
    assert!(matches!(
        FileDataset::open(FileSource::new(path).unwrap()),
        Err(Error::JsonTooLarge)
    ));
}

#[test]
fn rejects_changed_sources_and_can_reopen() {
    let dir = TempDir::new().unwrap();
    let source = text_file(&dir, "change.csv", "id\n1\n");
    let dataset = FileDataset::open(source.clone()).unwrap();
    fs::write(&source.path, "id\n1\n2\n").unwrap();
    assert!(matches!(
        dataset.preview(PreviewRequest::default()),
        Err(Error::FileChanged(_))
    ));
    assert_eq!(preview(source).results.rows(), 2);
}

#[test]
fn validates_options_and_paths() {
    let dir = TempDir::new().unwrap();
    assert_eq!(
        FileFormat::from_path(Path::new("DATA.PARQUET")).unwrap(),
        FileFormat::Parquet
    );
    assert!(matches!(
        FileSource::new("unknown.xlsx"),
        Err(Error::UnsupportedFormat(_))
    ));
    assert!(matches!(
        FileDataset::open(FileSource::new(dir.path().join("missing.csv")).unwrap()),
        Err(Error::Io { .. })
    ));
    let source = text_file(&dir, "data.csv", "id\n1\n");
    let dataset = FileDataset::open(source.clone()).unwrap();
    for request in [
        PreviewRequest {
            row_limit: 0,
            ..Default::default()
        },
        PreviewRequest {
            row_limit: MAX_PREVIEW_ROWS + 1,
            ..Default::default()
        },
        PreviewRequest {
            byte_limit: 0,
            ..Default::default()
        },
    ] {
        assert!(matches!(
            dataset.preview(request),
            Err(Error::InvalidOptions(_))
        ));
    }
    let mut bad = source;
    bad.csv.separator = b'|';
    assert!(matches!(
        FileDataset::open(bad),
        Err(Error::InvalidOptions(_))
    ));
}

#[test]
fn bracket_filenames_are_literal_and_reads_do_not_modify_sources() {
    let dir = TempDir::new().unwrap();
    for (name, data) in [
        ("data[1].csv", "id\n1\n"),
        ("data[1].jsonl", "{\"id\":1}\n"),
    ] {
        let source = text_file(&dir, name, data);
        let before = fs::read(&source.path).unwrap();
        assert_eq!(preview(source.clone()).results.rows(), 1);
        assert_eq!(fs::read(&source.path).unwrap(), before);
    }
}
