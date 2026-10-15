//! Measures streaming of a large query without a UI.
//!
//! `cargo run --release -p kuery-pg --example stream_bench -- "select * from orders"`

use std::time::Instant;

use kuery_core::result_set::ResultSet;
use kuery_pg::{Engine, Event, RunOptions};

fn main() -> anyhow::Result<()> {
    let sql = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "select * from orders".into());
    let url = std::env::var("KUERY_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://kuery:kuery@localhost:54329/kuery".into());

    let engine = Engine::new()?;
    let session = futures::executor::block_on(engine.connect(url))?;
    println!("PostgreSQL {}", session.server_version());

    let started = Instant::now();
    let rx = session.run_read_only(sql, RunOptions::default());
    let mut set = ResultSet::default();
    let mut batches = 0;
    futures::executor::block_on(async {
        while let Ok(event) = rx.recv().await {
            match event {
                Event::Columns(names) => set = ResultSet::new(names),
                Event::Batch(batch) => {
                    batches += 1;
                    set.push(batch);
                }
                Event::Finished(summary) => {
                    println!(
                        "{} rows in {} batches · first batch {:?} · total {:?}",
                        summary.rows, batches, summary.first_batch_after, summary.elapsed
                    );
                }
                Event::Failed(error) => println!("error: {error}"),
            }
        }
    });
    println!(
        "{} columns · {:.1} MB in Arrow · {:?} until everything is in memory",
        set.columns().len(),
        set.memory_size() as f64 / 1_048_576.0,
        started.elapsed()
    );
    Ok(())
}
