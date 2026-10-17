//! Run with a local path and optional row limit. Emits metadata, not cell values.
use std::error::Error;
use std::time::Instant;

use kuery_files::{FileDataset, FileSource, PreviewRequest};

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let path = arguments
        .next()
        .ok_or("usage: preview_bench <file> [row-limit]")?;
    let mut request = PreviewRequest::default();
    if let Some(limit) = arguments.next() {
        request.row_limit = limit.to_str().ok_or("row limit must be UTF-8")?.parse()?;
    }
    if arguments.next().is_some() {
        return Err("too many arguments".into());
    }
    let started = Instant::now();
    let dataset = FileDataset::open(FileSource::new(path)?)?;
    let preview = dataset.preview(request)?;
    println!("Polars 0.55.2 | {:?}", dataset.source().format);
    println!(
        "rows={} columns={} result_bytes={} elapsed_ms={}",
        preview.results.rows(),
        preview.schema.len(),
        preview.results.memory_size(),
        started.elapsed().as_millis()
    );
    println!("scope={:?}", preview.scope);
    for column in &preview.schema {
        println!(
            "{}: {} → {:?} (inferred={})",
            column.name, column.native_type, column.kind, column.inferred
        );
    }
    for warning in &preview.warnings {
        println!("note: {warning}");
    }
    Ok(())
}
