use kuery_files::{FileDataset, FileSource, PreviewRequest, PreviewScope, SqlFile, query_files};
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
