//! MySQL and MariaDB driver: an [`Adapter`] over one connection.
//!
//! The rules (read-only in two layers, write mode, previews, row edits,
//! batches, cancelling, refusing what MySQL commits by itself where Kuery
//! promises an undo) are the session's ([`kuery_core::session`]); this crate
//! connects, runs one statement at a time with the text protocol (values
//! come as the server writes them, the same text `cast(… as char)` gives),
//! kills a running statement from a side connection, and reads the catalog
//! from `information_schema`. It waits on a tokio runtime of its own.

mod catalog;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use kuery_core::adapter::{Adapter, Canceller, RowSink};
use kuery_core::catalog::{
    CatalogColumn, CatalogOverview, ForeignKey, FunctionInfo, TableDefinition, TableRef,
};
use kuery_core::driver::{ConnectOptions, DriverInfo, Family, Field, SqlDriver, Tls};
use kuery_core::result_set::ColumnKind;
use kuery_core::session::{Access, QueryError, RunOptions};
use kuery_core::sql::{Dialect, MYSQL};
use mysql_async::consts::ColumnType;
use mysql_async::prelude::Queryable as _;
use mysql_async::{Conn, Opts, OptsBuilder, SslOpts, Value};
use tokio::runtime::Runtime;

/// The runtime every MySQL connection waits on.
fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("kuery-mysql")
            .enable_all()
            .build()
            .expect("start the MySQL runtime")
    })
}

/// Where and how to connect.
#[derive(Clone)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    /// The database to start in; empty for none.
    pub database: String,
    /// TLS, checking the server's certificate against the system roots.
    pub tls: bool,
}

impl Target {
    fn opts(&self) -> Opts {
        let builder = OptsBuilder::default()
            .ip_or_hostname(self.host.clone())
            .tcp_port(self.port)
            .user(Some(self.user.clone()).filter(|u| !u.is_empty()))
            .pass(self.password.clone())
            .db_name(Some(self.database.clone()).filter(|d| !d.is_empty()))
            .prefer_socket(false)
            // Affected rows are the rows matched, not only those whose
            // values changed: a row edit that sets a value it already has
            // still counts as its one row.
            .client_found_rows(true)
            .ssl_opts(self.tls.then(SslOpts::default));
        Opts::from(builder)
    }
}

/// MySQL and MariaDB, for Kuery's driver list.
pub static DRIVER: SqlDriver = SqlDriver {
    info: DriverInfo {
        id: "mysql",
        name: "MySQL",
        aliases: &["mariadb", "my", "percona"],
        family: Family::Sql,
        default_port: 3306,
        database: Field {
            label: "Database",
            placeholder: "shop",
            required: false,
            numeric: false,
            help: Some("The database queries start in; the sidebar lists every one you can see."),
        },
        user: Field {
            label: "User",
            placeholder: "root",
            required: true,
            numeric: false,
            help: None,
        },
        tls: Tls::Toggle,
        production_note: "Production connections always open read-only and cannot edit rows.",
        file: false,
    },
    dialect: &MYSQL,
    open: |options, access| open(&Target::from(options), access),
};

impl From<&ConnectOptions> for Target {
    /// Through the local end of a tunnel when there is one.
    fn from(options: &ConnectOptions) -> Self {
        Target {
            host: match options.tunnel {
                Some(_) => "127.0.0.1".into(),
                None => options.host.clone(),
            },
            port: options.tunnel.unwrap_or(options.port),
            user: options.user.trim().to_string(),
            password: options.password.clone().filter(|p| !p.is_empty()),
            database: options.database.trim().to_string(),
            tls: options
                .tls
                .as_deref()
                .is_some_and(|mode| !mode.is_empty() && mode != "disable"),
        }
    }
}

/// One open connection.
pub struct Mysql {
    conn: Conn,
    /// To open the side connection that kills a running statement.
    opts: Opts,
    /// The connection's id on the server, for `KILL QUERY`.
    id: u32,
    mariadb: bool,
    version: String,
}

/// Connects. Read-only connections also make every transaction read-only
/// (`SESSION TRANSACTION READ ONLY`).
pub fn open(target: &Target, access: Access) -> Result<Box<dyn Adapter>, String> {
    let opts = target.opts();
    let (host, port) = (target.host.clone(), target.port);
    runtime().block_on(async move {
        let mut conn = tokio::time::timeout(Duration::from_secs(15), Conn::new(opts.clone()))
            .await
            .map_err(|_| format!("{host}:{port} did not answer in 15 seconds"))?
            .map_err(|error| error.to_string())?;
        let version: String = conn
            .query_first("select version()")
            .await
            .map_err(|error| error.to_string())?
            .unwrap_or_default();
        if access == Access::ReadOnly {
            conn.query_drop("set session transaction read only")
                .await
                .map_err(|error| error.to_string())?;
        }
        let id = conn.id();
        Ok(Box::new(Mysql {
            conn,
            opts,
            id,
            mariadb: version.contains("MariaDB"),
            version: version.split('-').next().unwrap_or(&version).to_string(),
        }) as Box<dyn Adapter>)
    })
}

/// A MySQL error as a [`QueryError`].
pub(crate) fn query_error(error: mysql_async::Error) -> QueryError {
    match error {
        mysql_async::Error::Server(server) => {
            if server.code == 1317 {
                // ER_QUERY_INTERRUPTED: KILL QUERY.
                return QueryError::Cancelled;
            }
            QueryError::Database {
                code: server.state.clone(),
                message: if matches!(server.code, 3024 | 1969) {
                    "The statement ran out of time".into()
                } else {
                    server.message
                },
            }
        }
        other => QueryError::Connection(other.to_string()),
    }
}

impl Mysql {
    pub(crate) fn run(&mut self, sql: &str) -> Result<(), QueryError> {
        runtime()
            .block_on(self.conn.query_drop(sql))
            .map_err(query_error)
    }
}

impl Adapter for Mysql {
    fn dialect(&self) -> &'static Dialect {
        &MYSQL
    }

    fn server(&self) -> String {
        let name = if self.mariadb { "MariaDB" } else { "MySQL" };
        format!("{name} {}", self.version)
    }

    fn execute(&mut self, sql: &str, rows: &mut dyn RowSink) -> Result<u64, QueryError> {
        let conn = &mut self.conn;
        runtime().block_on(async move {
            let mut result = conn.query_iter(sql).await.map_err(query_error)?;
            let Some(columns) = result.columns().filter(|columns| !columns.is_empty()) else {
                let changed = result.affected_rows();
                result.drop_result().await.map_err(query_error)?;
                return Ok(changed);
            };
            let names: Vec<String> = columns
                .iter()
                .map(|column| column.name_str().into_owned())
                .collect();
            let kinds = columns
                .iter()
                .map(|column| kind_of(column.column_type()))
                .collect();
            let count = names.len();
            rows.columns(names, Some(kinds))?;
            while let Some(row) = result.next().await.map_err(query_error)? {
                let values: Vec<Option<String>> = (0..count)
                    .map(|index| match row.as_ref(index) {
                        None | Some(Value::NULL) => None,
                        Some(Value::Bytes(bytes)) => {
                            Some(String::from_utf8_lossy(bytes).into_owned())
                        }
                        Some(other) => Some(other.as_sql(true).trim_matches('\'').to_string()),
                    })
                    .collect();
                let borrowed: Vec<Option<&str>> = values.iter().map(Option::as_deref).collect();
                if let Err(error) = rows.row(&borrowed) {
                    // Leave the connection ready for the next statement.
                    let _ = result.drop_result().await;
                    return Err(error);
                }
            }
            result.drop_result().await.map_err(query_error)?;
            Ok(0)
        })
    }

    fn limit(&mut self, options: &RunOptions) -> Result<(), QueryError> {
        let millis = options.statement_timeout.as_millis();
        let lock = options.lock_timeout.as_secs().max(1);
        let timeout = if self.mariadb {
            format!("set session max_statement_time = {}", millis as f64 / 1000.)
        } else {
            format!("set session max_execution_time = {millis}")
        };
        self.run(&format!(
            "{timeout}; set session innodb_lock_wait_timeout = {lock}"
        ))
    }

    fn canceller(&self) -> Canceller {
        let (opts, id) = (self.opts.clone(), self.id);
        Arc::new(move || {
            let opts = opts.clone();
            runtime().spawn(async move {
                if let Ok(mut side) = Conn::new(opts).await {
                    let _ = side.query_drop(format!("kill query {id}")).await;
                    let _ = side.disconnect().await;
                }
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
}

fn kind_of(column_type: ColumnType) -> ColumnKind {
    use ColumnType::*;
    match column_type {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG
        | MYSQL_TYPE_INT24 | MYSQL_TYPE_YEAR => ColumnKind::Integer,
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL | MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
            ColumnKind::Number
        }
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => ColumnKind::Date,
        MYSQL_TYPE_DATETIME
        | MYSQL_TYPE_DATETIME2
        | MYSQL_TYPE_TIMESTAMP
        | MYSQL_TYPE_TIMESTAMP2 => ColumnKind::Timestamp,
        MYSQL_TYPE_NULL => ColumnKind::Unknown,
        _ => ColumnKind::Text,
    }
}

#[cfg(test)]
mod tests;
