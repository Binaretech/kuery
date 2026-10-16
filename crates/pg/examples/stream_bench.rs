//! Measures streaming of a large query without a UI.
//!
//! `cargo run --release -p kuery-pg --example stream_bench -- "select * from orders"`

use std::time::Instant;

use std::sync::Arc;

use kuery_core::result_set::ResultSet;
use kuery_core::session::{Access, Event, Opener, RunOptions, Session};

fn main() -> anyhow::Result<()> {
    let sql = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "select * from orders".into());
    let url = std::env::var("KUERY_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://kuery:kuery@localhost:54329/kuery".into());

    let opener: Opener = Arc::new(move |access| kuery_pg::open(&url, access));
    let session = futures::executor::block_on(Session::open(opener, Access::ReadOnly))
        .map_err(anyhow::Error::msg)?;
    println!("{}", session.server_label());

    let started = Instant::now();
    let rx = session.run_read_only(sql, RunOptions::default());
    let mut set = ResultSet::default();
    let mut batches = 0;
    futures::executor::block_on(async {
        while let Ok(event) = rx.recv().await {
            match event {
                Event::Columns(names) => set = ResultSet::new(names),
                Event::Types(kinds) => set.set_kinds(kinds),
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
