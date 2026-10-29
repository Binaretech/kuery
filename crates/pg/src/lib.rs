//! PostgreSQL driver: an [`Adapter`] over one connection.
//!
//! The rules (read-only in two layers, write mode with its transaction and
//! savepoints, full writes, previews, row edits, batches, cancelling) are
//! the session's ([`kuery_core::session`]); this crate connects (TLS as the
//! connection's `sslmode` says, see [`tls`]), runs one statement at a time,
//! cancels it over the protocol, and reads the catalog and the server's
//! activity. Read-only connections also set `default_transaction_read_only`,
//! so every transaction starts read-only even if something opened one
//! unasked. It waits on a tokio runtime of its own.

mod activity;
mod catalog;
mod tls;

use std::sync::{Arc, OnceLock};

use futures::{TryStreamExt as _, pin_mut};
use kuery_core::activity::{Activity, Signal};
use kuery_core::adapter::{Adapter, Canceller, RowSink};
use kuery_core::catalog::{
    CatalogColumn, CatalogOverview, ForeignKey, FunctionInfo, TableDefinition, TableRef,
};
use kuery_core::driver::{ConnectOptions, DriverInfo, Family, Field, SqlDriver, Tls};
use kuery_core::result_set::ColumnKind;
pub use kuery_core::session::{Access, QueryError, RunOptions};
use kuery_core::sql::{Dialect, POSTGRES};
use tokio::runtime::Runtime;
use tokio_postgres::{CancelToken, Client, SimpleQueryMessage, SimpleQueryRow};

use crate::tls::{Connector, Target};

/// The runtime every Postgres connection lives on.
fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("kuery-pg")
            .enable_all()
            .build()
            .expect("start the Postgres runtime")
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("{}", with_causes(.0))]
    Postgres(#[from] tokio_postgres::Error),
    #[error("invalid connection URL: {0}")]
    Config(String),
    #[error("TLS: {0}")]
    Tls(String),
}

/// "error performing TLS handshake" says nothing without the rustls cause.
fn with_causes(error: &tokio_postgres::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text = format!("{text}: {cause_text}");
        }
        source = cause.source();
    }
    text
}

/// A Postgres error as the session's [`QueryError`]: the server's code and
/// message (with its hint), or a lost connection.
pub(crate) fn query_error(error: tokio_postgres::Error) -> QueryError {
    match error.as_db_error() {
        // A cancel the user asked for (not a statement timeout).
        Some(db) if db.code().code() == "57014" && db.message().contains("user request") => {
            QueryError::Cancelled
        }
        Some(db) => QueryError::Database {
            code: db.code().code().to_string(),
            message: match db.hint() {
                Some(hint) => format!("{} ({hint})", db.message()),
                None => db.message().to_string(),
            },
        },
        None => QueryError::Connection(error.to_string()),
    }
}

/// Text that is not UTF-8, as a `SQL_ASCII` database can hold.
fn not_utf8(row: &SimpleQueryRow, index: usize) -> QueryError {
    QueryError::Database {
        code: String::new(),
        message: format!(
            "Column \"{}\" has text that is not valid UTF-8, so it cannot be shown \
             (the database may use the SQL_ASCII encoding)",
            row.columns()[index].name()
        ),
    }
}

/// `.pg()?` turns a Postgres error into a [`QueryError`].
pub(crate) trait Pg<T> {
    fn pg(self) -> Result<T, QueryError>;
}

impl<T> Pg<T> for Result<T, tokio_postgres::Error> {
    fn pg(self) -> Result<T, QueryError> {
        self.map_err(query_error)
    }
}

/// One open connection.
pub struct Postgres {
    client: Client,
    cancel: CancelToken,
    connector: Connector,
    version: String,
}

/// PostgreSQL, for Kuery's driver list.
pub static DRIVER: SqlDriver = SqlDriver {
    info: DriverInfo {
        id: "postgres",
        name: "PostgreSQL",
        aliases: &["postgresql", "pg", "psql"],
        family: Family::Sql,
        default_port: 5432,
        database: Field {
            label: "Database",
            placeholder: "postgres",
            required: true,
            numeric: false,
            help: None,
        },
        user: Field {
            label: "User",
            placeholder: "postgres",
            required: true,
            numeric: false,
            help: None,
        },
        tls: Tls::Modes(&["disable", "prefer", "require", "verify-ca", "verify-full"]),
        production_note: "Production connections always open read-only and cannot edit rows.",
        file: false,
        host_help: None,
        options: &[],
        uri: None,
        ssh_route: None,
    },
    dialect: &POSTGRES,
    open: |options, access| open(&conninfo(options), access),
};

/// libpq `key=value` connection string, through the local end of a tunnel
/// when there is one (the host still names the server, for TLS). The
/// password is only ever added here, in memory.
pub fn conninfo(options: &ConnectOptions) -> String {
    let mut parts = vec![format!("host={}", quote(&options.host))];
    if options.tunnel.is_some() {
        parts.push("hostaddr=127.0.0.1".into());
    }
    parts.extend([
        format!("port={}", options.tunnel.unwrap_or(options.port)),
        format!("dbname={}", quote(&options.database)),
        format!("user={}", quote(&options.user)),
        "application_name=Kuery".to_string(),
    ]);
    if let Some(password) = options.password.as_deref().filter(|p| !p.is_empty()) {
        parts.push(format!("password={}", quote(password)));
    }
    if let Some(mode) = options.tls.as_deref().filter(|m| !m.is_empty()) {
        parts.push(format!("sslmode={}", quote(mode)));
    }
    parts.join(" ")
}

/// libpq quoting: single quotes, with `\` and `'` escaped by a backslash.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Connects with a libpq connection string or URL (`sslmode` honoured, see
/// [`tls`]).
pub fn open(conninfo: &str, access: Access) -> Result<Box<dyn Adapter>, String> {
    open_postgres(conninfo, access)
        .map(|postgres| Box::new(postgres) as Box<dyn Adapter>)
        .map_err(|error| error.to_string())
}

fn open_postgres(conninfo: &str, access: Access) -> Result<Postgres, ConnectError> {
    let target = Target::parse(conninfo)?;
    let connector = target.connector()?;
    let handle = runtime().handle().clone();
    runtime().block_on(async move {
        let client = connector.connect(&target.config, &handle).await?;
        if access == Access::ReadOnly {
            client
                .batch_execute("set default_transaction_read_only = on")
                .await?;
        }
        let version = client
            .simple_query("show server_version")
            .await?
            .into_iter()
            .find_map(|message| match message {
                SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
                _ => None,
            })
            .unwrap_or_default();
        Ok(Postgres {
            cancel: client.cancel_token(),
            connector,
            client,
            version,
        })
    })
}

impl Adapter for Postgres {
    fn dialect(&self) -> &'static Dialect {
        &POSTGRES
    }

    fn server(&self) -> String {
        format!("PostgreSQL {}", self.version)
    }

    fn execute(&mut self, sql: &str, rows: &mut dyn RowSink) -> Result<u64, QueryError> {
        let client = &self.client;
        runtime().block_on(async move {
            // The column types, when wanted, from preparing the statement.
            // A statement the server cannot prepare fails the same way when
            // it runs (and inside a transaction the failure aborts it), so
            // its error is the answer.
            let mut kinds: Option<Vec<ColumnKind>> = None;
            if rows.wants_kinds() {
                match client.prepare(sql).await {
                    Ok(statement) => {
                        kinds = Some(
                            statement
                                .columns()
                                .iter()
                                .map(|c| kind_of(c.type_()))
                                .collect(),
                        )
                    }
                    Err(error) if error.as_db_error().is_some() => return Err(query_error(error)),
                    Err(_) => {}
                }
            }
            let stream = client.simple_query_raw(sql).await.pg()?;
            pin_mut!(stream);
            let mut changed = 0;
            let mut returns_rows = false;
            while let Some(message) = stream.try_next().await.pg()? {
                match message {
                    SimpleQueryMessage::RowDescription(columns) => {
                        returns_rows = true;
                        let names: Vec<String> =
                            columns.iter().map(|c| c.name().to_string()).collect();
                        let kinds = kinds.take().filter(|kinds| kinds.len() == names.len());
                        rows.columns(names, kinds)?;
                    }
                    SimpleQueryMessage::Row(row) => {
                        let values = (0..row.len())
                            .map(|index| row.try_get(index).map_err(|_| not_utf8(&row, index)))
                            .collect::<Result<Vec<Option<&str>>, _>>()?;
                        rows.row(&values)?;
                    }
                    SimpleQueryMessage::CommandComplete(count) if !returns_rows => {
                        changed = count;
                    }
                    _ => {}
                }
            }
            Ok(changed)
        })
    }

    fn limit(&mut self, options: &RunOptions) -> Result<(), QueryError> {
        let sql = format!(
            "set statement_timeout = {}; set lock_timeout = {}",
            options.statement_timeout.as_millis(),
            options.lock_timeout.as_millis()
        );
        runtime().block_on(self.client.batch_execute(&sql)).pg()
    }

    fn canceller(&self) -> Canceller {
        let (connector, token) = (self.connector.clone(), self.cancel.clone());
        Arc::new(move || {
            let cancel = connector.cancel(token.clone());
            runtime().spawn(async move {
                let _ = cancel.await;
            });
        })
    }

    fn overview(&mut self) -> Result<CatalogOverview, QueryError> {
        catalog::overview(self)
    }

    fn definition(&mut self, table: &TableRef) -> Result<TableDefinition, QueryError> {
        catalog::definition(self, table)
    }

    fn columns(&mut self, schema: Option<&str>) -> Result<Vec<CatalogColumn>, QueryError> {
        catalog::catalog_columns(self, schema)
    }

    fn foreign_keys(&mut self, schema: &str) -> Result<Vec<ForeignKey>, QueryError> {
        catalog::catalog_foreign_keys(self, schema)
    }

    fn search_path(&mut self) -> Result<Vec<String>, QueryError> {
        catalog::search_path(self)
    }

    fn routine_source(&mut self, routine: &FunctionInfo) -> Result<String, QueryError> {
        catalog::function_definition(self, routine)
    }

    fn has_activity(&self) -> bool {
        true
    }

    fn activity(&mut self) -> Result<Activity, QueryError> {
        activity::activity(self)
    }

    fn signal(&mut self, pid: i32, signal: Signal) -> Result<bool, QueryError> {
        activity::signal(self, pid, signal)
    }
}

fn kind_of(ty: &tokio_postgres::types::Type) -> ColumnKind {
    use tokio_postgres::types::Type;
    match *ty {
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => ColumnKind::Integer,
        Type::FLOAT4 | Type::FLOAT8 | Type::NUMERIC => ColumnKind::Number,
        Type::BOOL => ColumnKind::Boolean,
        Type::DATE => ColumnKind::Date,
        Type::TIMESTAMP | Type::TIMESTAMPTZ => ColumnKind::Timestamp,
        _ => ColumnKind::Text,
    }
}

#[cfg(test)]
mod tests;
