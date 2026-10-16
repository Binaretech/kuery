//! SQLite driver: an [`Adapter`] over one database file.
//!
//! The rules (read-only in two layers, write mode, previews, row edits,
//! batches, cancelling) are the session's ([`kuery_core::session`]); this
//! crate opens the file, runs one statement at a time, interrupts it, and
//! reads the catalog. Read-only connections open the file read-only and
//! keep `PRAGMA query_only` on; read-write ones turn foreign keys on, so
//! their cascades and checks apply. Statements stop at their timeout
//! through the progress handler, and wait for another process's lock up to
//! the lock timeout.

mod catalog;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use kuery_core::adapter::{Adapter, Canceller, RowSink};
use kuery_core::catalog::{CatalogColumn, CatalogOverview, ForeignKey, TableDefinition, TableRef};
use kuery_core::driver::{DriverInfo, Family, Field, SqlDriver, Tls};
use kuery_core::result_set::ColumnKind;
use kuery_core::session::{Access, QueryError, RunOptions};
use kuery_core::sql::{Dialect, SQLITE};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode, OpenFlags};

/// SQLite, for Kuery's driver list: `database` is the file's path (made
/// absolute by the app).
pub static DRIVER: SqlDriver = SqlDriver {
    info: DriverInfo {
        id: "sqlite",
        name: "SQLite",
        aliases: &["sqlite3", "file", "embedded"],
        family: Family::Sql,
        default_port: 0,
        database: Field {
            label: "File",
            placeholder: "data/app.sqlite",
            required: true,
            numeric: false,
            help: Some("The database file; a relative path starts at the project folder."),
        },
        user: Field {
            label: "User",
            placeholder: "",
            required: false,
            numeric: false,
            help: None,
        },
        tls: Tls::Toggle,
        production_note: "Production connections open the file read-only and cannot edit rows.",
        file: true,
    },
    dialect: &SQLITE,
    open: |options, access| open(Path::new(&options.database), access),
};

/// An open database file.
pub struct Sqlite {
    connection: Connection,
    version: String,
    /// Set by the progress handler when a statement ran out of time.
    timed_out: Arc<AtomicBool>,
}

/// Opens the database at `path`, which must exist: a mistyped path never
/// creates an empty database.
pub fn open(path: &Path, access: Access) -> Result<Box<dyn Adapter>, String> {
    if !path.is_file() {
        return Err(format!("There is no database file at {}", path.display()));
    }
    let flags = match access {
        Access::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
        Access::ReadWrite => OpenFlags::SQLITE_OPEN_READ_WRITE,
    } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(path, flags)
        .map_err(|error| format!("Could not open {}: {error}", path.display()))?;
    let setup = match access {
        // Even temporary tables are refused.
        Access::ReadOnly => "pragma query_only = on",
        Access::ReadWrite => "pragma foreign_keys = on",
    };
    connection
        .execute_batch(setup)
        .map_err(|error| format!("Could not open {}: {error}", path.display()))?;
    let version: String = connection
        .query_row("select sqlite_version()", [], |row| row.get(0))
        .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
    // Is it a database at all? Reading the schema says.
    connection
        .query_row("select count(*) from sqlite_master", [], |_| Ok(()))
        .map_err(|error| format!("{} is not a database: {error}", path.display()))?;
    Ok(Box::new(Sqlite {
        connection,
        version,
        timed_out: Arc::new(AtomicBool::new(false)),
    }))
}

impl Sqlite {
    fn error(&self, error: rusqlite::Error) -> QueryError {
        if let rusqlite::Error::SqliteFailure(failure, message) = &error {
            if failure.code == ErrorCode::OperationInterrupted {
                if self.timed_out.swap(false, Ordering::SeqCst) {
                    return QueryError::Database {
                        code: "timeout".into(),
                        message: "The statement ran out of time".into(),
                    };
                }
                return QueryError::Cancelled;
            }
            return QueryError::Database {
                code: format!("{}", failure.extended_code),
                message: message.clone().unwrap_or_else(|| failure.to_string()),
            };
        }
        QueryError::Database {
            code: String::new(),
            message: error.to_string(),
        }
    }
}

impl Adapter for Sqlite {
    fn dialect(&self) -> &'static Dialect {
        &SQLITE
    }

    fn server(&self) -> String {
        format!("SQLite {}", self.version)
    }

    fn execute(&mut self, sql: &str, rows: &mut dyn RowSink) -> Result<u64, QueryError> {
        let mut statement = self
            .connection
            .prepare(sql)
            .map_err(|error| self.error(error))?;
        let count = statement.column_count();
        if count == 0 {
            let changed = statement.raw_execute().map_err(|error| self.error(error))?;
            return Ok(changed as u64);
        }
        let names: Vec<String> = statement
            .column_names()
            .into_iter()
            .map(str::to_string)
            .collect();
        let kinds: Vec<ColumnKind> = statement
            .columns()
            .iter()
            .map(|column| kind_of(column.decl_type()))
            .collect();
        rows.columns(names, Some(kinds))?;
        let mut result = statement.raw_query();
        while let Some(row) = result.next().map_err(|error| self.error(error))? {
            let values: Vec<Option<String>> = (0..count)
                .map(|index| row.get_ref(index).ok().and_then(text))
                .collect();
            let borrowed: Vec<Option<&str>> = values.iter().map(Option::as_deref).collect();
            rows.row(&borrowed)?;
        }
        Ok(0)
    }

    fn limit(&mut self, options: &RunOptions) -> Result<(), QueryError> {
        let started = Instant::now();
        let (limit, timed_out) = (options.statement_timeout, self.timed_out.clone());
        timed_out.store(false, Ordering::SeqCst);
        let _ = self.connection.progress_handler(
            10_000,
            Some(move || {
                let over = started.elapsed() > limit;
                if over {
                    timed_out.store(true, Ordering::SeqCst);
                }
                over
            }),
        );
        self.connection
            .busy_timeout(options.lock_timeout)
            .map_err(|error| self.error(error))
    }

    fn canceller(&self) -> Canceller {
        let interrupt = self.connection.get_interrupt_handle();
        Arc::new(move || interrupt.interrupt())
    }

    fn overview(&mut self) -> Result<CatalogOverview, QueryError> {
        catalog::overview(&self.connection)
    }

    fn definition(&mut self, table: &TableRef) -> Result<TableDefinition, QueryError> {
        catalog::definition(&self.connection, table)
    }

    fn columns(&mut self, schema: Option<&str>) -> Result<Vec<CatalogColumn>, QueryError> {
        catalog::catalog_columns(&self.connection, schema)
    }

    fn foreign_keys(&mut self, schema: &str) -> Result<Vec<ForeignKey>, QueryError> {
        catalog::catalog_foreign_keys(&self.connection, schema)
    }

    fn search_path(&mut self) -> Result<Vec<String>, QueryError> {
        Ok(vec!["main".into()])
    }
}

/// A value as text, the way SQLite's own `cast(… as text)` writes it (so a
/// row's shown values can be compared back), and blobs as `\x` hex.
fn text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(n) => Some(n.to_string()),
        ValueRef::Real(f) => Some(real(f)),
        ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Some(format!(
            "\\x{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )),
    }
}

/// `1.0`, `0.25`, `1.0e+20`: SQLite's `%!.15g`.
fn real(value: f64) -> String {
    if !value.is_finite() {
        return if value.is_nan() {
            String::new()
        } else if value > 0. {
            "Inf".into()
        } else {
            "-Inf".into()
        };
    }
    let formatted = format!("{:.15e}", value);
    let (mantissa, exponent) = formatted.split_once('e').unwrap_or((&formatted, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if (-5..15).contains(&exponent) {
        let digits = (14 - exponent).max(0) as usize;
        let fixed = format!("{value:.digits$}");
        let trimmed = if fixed.contains('.') {
            fixed
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_string()
        } else {
            fixed
        };
        if trimmed.contains('.') {
            trimmed
        } else {
            format!("{trimmed}.0")
        }
    } else {
        let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
        let mantissa = if mantissa.contains('.') {
            mantissa.to_string()
        } else {
            format!("{mantissa}.0")
        };
        format!(
            "{mantissa}e{}{:02}",
            if exponent < 0 { '-' } else { '+' },
            exponent.abs()
        )
    }
}

/// What a column holds, from its declared type and SQLite's affinity rules.
fn kind_of(declared: Option<&str>) -> ColumnKind {
    let Some(declared) = declared else {
        return ColumnKind::Unknown;
    };
    let upper = declared.to_uppercase();
    if upper.starts_with("BOOL") {
        ColumnKind::Boolean
    } else if upper == "DATE" {
        ColumnKind::Date
    } else if upper.starts_with("DATETIME") || upper.starts_with("TIMESTAMP") {
        ColumnKind::Timestamp
    } else if upper.contains("INT") {
        ColumnKind::Integer
    } else if ["CHAR", "CLOB", "TEXT"].iter().any(|t| upper.contains(t)) {
        ColumnKind::Text
    } else if upper.is_empty() || upper.contains("BLOB") {
        ColumnKind::Unknown
    } else {
        // REAL, FLOAT, DOUBLE, NUMERIC, DECIMAL…
        ColumnKind::Number
    }
}

#[cfg(test)]
mod tests;
