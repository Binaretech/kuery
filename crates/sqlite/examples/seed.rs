//! Builds the sample database `dev/shop.sqlite` from `dev/seed-sqlite.sql`,
//! replacing it if it exists: `cargo run -p kuery-sqlite --example seed`.

use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dev = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dev");
    let seed = std::fs::read_to_string(dev.join("seed-sqlite.sql"))?;
    let path = dev.join("shop.sqlite");
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let connection = rusqlite::Connection::open(&path)?;
    connection.execute_batch(&seed)?;
    let orders: i64 = connection.query_row("select count(*) from orders", [], |row| row.get(0))?;
    println!(
        "Wrote {} with {orders} orders",
        path.canonicalize()?.display()
    );
    Ok(())
}
