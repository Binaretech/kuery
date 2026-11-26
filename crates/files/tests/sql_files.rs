use kuery_core::result_set::{BatchBuilder, ColumnKind, ResultSet};
use kuery_files::{
    Error, FileDataset, FileSource, MAX_NDJSON_SQL_BYTES, PreviewRequest, PreviewScope, SqlFile,
    SqlSource, SqlTable, query_files, query_sources,
};
use polars::prelude::*;
use std::fs::{self, File};
use tempfile::TempDir;
fn binding(path: &std::path::Path, name: &str) -> SqlFile {
    let dataset = FileDataset::open(FileSource::new(path).unwrap()).unwrap();
    SqlFile {
        name: name.into(),
        source: dataset.source().clone(),
        fingerprint: dataset.fingerprint().clone(),
    }
}
#[test]
fn aggregates_read_beyond_preview_and_output_limit() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("rows.csv");
    let mut csv = "n\n".to_owned();
    for n in 0..12_345 {
        csv.push_str(&format!("{n}\n"));
    }
    fs::write(&path, csv).unwrap();
    let file = binding(&path, "rows");
    let preview = FileDataset::open(file.source.clone())
        .unwrap()
        .preview(PreviewRequest::default())
        .unwrap();
    assert_eq!(preview.results.rows(), 10_000);
    let result = query_files(
        std::slice::from_ref(&file),
        "SELECT COUNT(*) AS total, SUM(n) AS amount FROM rows",
        PreviewRequest {
            row_limit: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.results.cell(0, 0), Some(Some("12345")));
    assert_eq!(result.results.cell(0, 1), Some(Some("76193340")));
    assert_eq!(result.scope, PreviewScope::Complete);
    let result = query_files(
        &[file],
        "SELECT n FROM rows ORDER BY n DESC",
        PreviewRequest {
            row_limit: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.results.cell(0, 0), Some(Some("12344")));
    assert!(matches!(
        result.scope,
        PreviewScope::PrefixLimited { loaded_rows: 2, .. }
    ));
}
#[test]
fn sql_refuses_jsonl_over_its_size_limit_before_reading() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("large.jsonl");
    // Sparse: as large as the limit allows plus one, without the disk space.
    File::create(&path)
        .unwrap()
        .set_len(MAX_NDJSON_SQL_BYTES + 1)
        .unwrap();
    let file = binding(&path, "large");
    let result = query_files(&[file], "SELECT * FROM large", PreviewRequest::default());
    assert!(
        matches!(result, Err(Error::NdJsonTooLarge)),
        "{:?}",
        result.err()
    );
}
#[test]
fn formats_join_cte_nulls_and_csv_options() {
    let dir = TempDir::new().unwrap();
    let csv = dir.path().join("orders.csv");
    fs::write(&csv, "id;amount\n1;10\n2;20\n3;NULL\n").unwrap();
    let mut orders = binding(&csv, "orders");
    orders.source.csv.separator = b';';
    orders.source.csv.null_value = Some("NULL".into());
    for (name, data) in [
        (
            "groups.json",
            "[{\"id\":1,\"g\":\"a\"},{\"id\":2,\"g\":\"a\"},{\"id\":3,\"g\":\"b\"}]",
        ),
        (
            "groups.jsonl",
            "{\"id\":1,\"g\":\"a\"}\n{\"id\":2,\"g\":\"a\"}\n{\"id\":3,\"g\":\"b\"}\n",
        ),
    ] {
        fs::write(dir.path().join(name), data).unwrap();
    }
    let path = dir.path().join("groups.parquet");
    ParquetWriter::new(File::create(&path).unwrap())
        .finish(&mut df!("id"=>[1i64,2,3],"g"=>["a","a","b"]).unwrap())
        .unwrap();
    for name in ["groups.json", "groups.jsonl", "groups.parquet"] {
        let result=query_files(&[orders.clone(),binding(&dir.path().join(name),"groups")],"WITH totals AS (SELECT g.g, SUM(o.amount) AS total FROM orders o JOIN groups g ON o.id = g.id GROUP BY g.g) SELECT * FROM totals ORDER BY g",PreviewRequest::default()).unwrap();
        assert_eq!(result.results.rows(), 2);
        assert_eq!(result.results.cell(0, 1), Some(Some("30")));
        assert_eq!(result.results.cell(1, 1), Some(None));
    }
}
#[test]
fn rejects_mutations_paths_scripts_and_changed_sources() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.csv");
    fs::write(&path, "x\n1\n").unwrap();
    let file = binding(&path, "a");
    for sql in [
        "DELETE FROM a",
        "DROP TABLE a",
        "CREATE TABLE t AS SELECT * FROM a",
        "SELECT * INTO t FROM a",
        "SELECT * FROM read_csv('/tmp/no.csv')",
        "WITH x AS (SELECT * FROM read_parquet('/tmp/no.parquet')) SELECT * FROM x",
        "SELECT * FROM a; SELECT * FROM a",
        "SELECT * FROM missing",
        "WITH x AS (DELETE FROM a RETURNING *) SELECT * FROM x",
    ] {
        assert!(
            query_files(std::slice::from_ref(&file), sql, PreviewRequest::default()).is_err(),
            "{sql}"
        );
    }
    fs::write(path, "x\n1\n2\n").unwrap();
    assert!(matches!(
        query_files(&[file], "SELECT * FROM a", PreviewRequest::default()),
        Err(kuery_files::Error::FileChanged(_))
    ));
}
#[test]
fn jsonl_conflicts_beyond_preview_are_not_silently_dropped() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.jsonl");
    fs::write(
        &path,
        format!("{}{{\"x\":\"wrong\"}}\n", "{\"x\":1}\n".repeat(10_005)),
    )
    .unwrap();
    let file = binding(&path, "a");
    assert!(query_files(&[file], "SELECT COUNT(*) FROM a", PreviewRequest::default()).is_err());
}

#[test]
fn csv_sql_rejects_lossy_integer_promotion_even_beyond_preview() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("mixed.csv");
    for data in [
        "id\n9007199254740993\n1.5\n".to_owned(),
        format!("id\n{}9007199254740993\n", "1.5\n".repeat(10_005)),
    ] {
        fs::write(&path, data).unwrap();
        assert!(
            query_files(
                &[binding(&path, "mixed")],
                "SELECT COUNT(DISTINCT id) FROM mixed",
                PreviewRequest::default()
            )
            .is_err()
        );
    }
    fs::write(&path, "id\n9007199254740994\n1.5\n").unwrap();
    assert!(
        query_files(
            &[binding(&path, "mixed")],
            "SELECT * FROM mixed",
            PreviewRequest::default()
        )
        .is_ok()
    );
}

/// A database result, kept as text with its columns' kinds.
fn held(columns: &[(&str, ColumnKind)], rows: &[&[Option<&str>]]) -> ResultSet {
    let mut set = ResultSet::new(columns.iter().map(|(name, _)| name.to_string()).collect());
    set.set_kinds(columns.iter().map(|(_, kind)| *kind).collect());
    let mut builder = BatchBuilder::new(columns.len(), rows.len());
    for row in rows {
        builder.push_row(|index| row[index]);
    }
    set.push(builder.finish());
    set
}

#[test]
fn a_held_result_joins_a_file_by_its_typed_columns() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("targets.csv");
    fs::write(&path, "region,target\nnorth,100\nsouth,50\n").unwrap();
    let orders = held(
        &[
            ("region", ColumnKind::Text),
            ("total", ColumnKind::Number),
            ("placed", ColumnKind::Date),
        ],
        &[
            &[Some("north"), Some("70.5"), Some("2026-01-02")],
            &[Some("north"), Some("40"), Some("2026-01-20")],
            &[Some("south"), Some("10"), Some("2026-02-01")],
            &[Some("south"), None, Some("2026-02-03")],
        ],
    );
    let sources = [
        SqlSource::Table(SqlTable {
            name: "orders".into(),
            results: orders,
        }),
        SqlSource::File(binding(&path, "targets")),
    ];
    let result = query_sources(
        &sources,
        "SELECT o.region, SUM(o.total) AS sold, t.target, MAX(o.placed) AS last \
         FROM orders o JOIN targets t ON o.region = t.region \
         GROUP BY o.region, t.target ORDER BY o.region",
        PreviewRequest::default(),
    )
    .unwrap();
    assert_eq!(
        result.results.columns(),
        ["region", "sold", "target", "last"]
    );
    assert_eq!(result.results.rows(), 2);
    assert_eq!(result.results.cell(0, 1), Some(Some("110.5")));
    assert_eq!(result.results.cell(0, 2), Some(Some("100")));
    assert_eq!(result.results.cell(1, 1), Some(Some("10")));
    assert_eq!(result.results.cell(1, 3), Some(Some("2026-02-03")));
    assert_eq!(result.results.kinds()[1], ColumnKind::Number);
}

#[test]
fn table_names_must_differ_across_files_and_results() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.csv");
    fs::write(&path, "x\n1\n").unwrap();
    let sources = [
        SqlSource::File(binding(&path, "a")),
        SqlSource::Table(SqlTable {
            name: "a".into(),
            results: held(&[("x", ColumnKind::Integer)], &[&[Some("1")]]),
        }),
    ];
    assert!(query_sources(&sources, "SELECT * FROM a", PreviewRequest::default()).is_err());
}
