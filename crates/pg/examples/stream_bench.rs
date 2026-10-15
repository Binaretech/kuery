//! Mide el streaming de una query grande sin interfaz.
//!
//! `cargo run --release -p dbadmin-pg --example stream_bench -- "select * from orders"`

use std::time::Instant;

use dbadmin_core::result_set::ResultSet;
use dbadmin_pg::{Engine, Event, RunOptions};

fn main() -> anyhow::Result<()> {
    let sql = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "select * from orders".into());
    let url = std::env::var("DBADMIN_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://dbadmin:dbadmin@localhost:54329/dbadmin".into());

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
                        "{} filas en {} lotes · primer lote {:?} · total {:?}",
                        summary.rows, batches, summary.first_batch_after, summary.elapsed
                    );
                }
                Event::Failed(error) => println!("error: {error}"),
            }
        }
    });
    println!(
        "{} columnas · {:.1} MB en Arrow · {:?} hasta tener todo en memoria",
        set.columns().len(),
        set.memory_size() as f64 / 1_048_576.0,
        started.elapsed()
    );
    Ok(())
}
