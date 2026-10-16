//! A session of any SQL driver, as the SQL family's views see it: run
//! queries read-only or in write mode, apply row changes, read the catalog.
//! Each variant hands the call to its driver's crate; what one engine does
//! not have (query plans and server activity are PostgreSQL's) says so
//! instead of failing somewhere deeper.

use std::sync::Arc;

use kuery_core::catalog::{CatalogOverview, FunctionInfo, TableDefinition, TableRef};
use kuery_core::completion;
use kuery_core::diagram::Diagram;
use kuery_core::edit::WriteStatement;
use kuery_core::result_set::ResultSet;
use kuery_core::session::{Access, Event, QueryError, RunOptions, StatementImpact};
use kuery_core::sql::Dialect;

use crate::drivers::{BoxFuture, Engines};

/// An open SQL connection. Cheap to clone; runs one query at a time.
#[derive(Clone)]
pub enum SqlSession {
    Postgres(kuery_pg::Session),
    Sqlite(kuery_sqlite::Session),
    Mysql(kuery_mysql::Session),
}

/// Runs `$body` with `$session` bound to the variant's session and
/// `$driver` naming its crate.
macro_rules! each {
    ($value:expr, $session:ident, $driver:ident => $body:expr) => {
        match $value {
            SqlSession::Postgres($session) => {
                #[allow(unused_imports)]
                use kuery_pg as $driver;
                $body
            }
            SqlSession::Sqlite($session) => {
                #[allow(unused_imports)]
                use kuery_sqlite as $driver;
                $body
            }
            SqlSession::Mysql($session) => {
                #[allow(unused_imports)]
                use kuery_mysql as $driver;
                $body
            }
        }
    };
}

/// What an engine without a feature answers.
fn missing<T: Send + 'static>(what: &str) -> BoxFuture<Result<T, QueryError>> {
    let message = what.to_string();
    Box::pin(async move { Err(QueryError::Unsupported(message)) })
}

impl SqlSession {
    pub fn dialect(&self) -> Dialect {
        match self {
            SqlSession::Postgres(_) => Dialect::Postgres,
            SqlSession::Sqlite(_) => Dialect::Sqlite,
            SqlSession::Mysql(_) => Dialect::Mysql,
        }
    }

    /// `PostgreSQL 16.4`, for the status bar.
    pub fn server_label(&self) -> String {
        match self {
            SqlSession::Postgres(session) => format!("PostgreSQL {}", session.server_version()),
            SqlSession::Sqlite(session) => format!("SQLite {}", session.server_version()),
            SqlSession::Mysql(session) => format!(
                "{} {}",
                if session.is_mariadb() {
                    "MariaDB"
                } else {
                    "MySQL"
                },
                session.server_version()
            ),
        }
    }

    pub fn access(&self) -> Access {
        each!(self, session, _d => session.access())
    }

    /// The Postgres session, for what only Postgres has.
    pub fn postgres(&self) -> Option<&kuery_pg::Session> {
        match self {
            SqlSession::Postgres(session) => Some(session),
            _ => None,
        }
    }

    /// Whether Plan (EXPLAIN as a tree) works here.
    pub fn has_plans(&self) -> bool {
        self.postgres().is_some()
    }

    /// Whether the server's sessions can be watched (Server Activity).
    pub fn has_activity(&self) -> bool {
        self.postgres().is_some()
    }

    pub fn run_read_only(
        &self,
        sql: String,
        options: RunOptions,
    ) -> async_channel::Receiver<Event> {
        each!(self, session, _d => session.run_read_only(sql, options))
    }

    pub fn fetch_all(
        &self,
        sql: String,
        options: RunOptions,
    ) -> BoxFuture<Result<ResultSet, QueryError>> {
        each!(self, session, _d => Box::pin(session.fetch_all(sql, options)))
    }

    pub fn apply(&self, statements: Vec<WriteStatement>) -> BoxFuture<Result<u64, QueryError>> {
        each!(self, session, _d => Box::pin(session.apply(statements)))
    }

    pub fn run_in_transaction(
        &self,
        sql: String,
        options: RunOptions,
    ) -> async_channel::Receiver<Event> {
        each!(self, session, _d => session.run_in_transaction(sql, options))
    }

    pub fn run_autocommit(
        &self,
        sql: String,
        options: RunOptions,
    ) -> async_channel::Receiver<Event> {
        each!(self, session, _d => session.run_autocommit(sql, options))
    }

    pub fn preview(
        &self,
        sql: String,
        options: RunOptions,
    ) -> BoxFuture<Result<Vec<StatementImpact>, QueryError>> {
        each!(self, session, _d => Box::pin(session.preview(sql, options)))
    }

    pub fn explain(
        &self,
        statement: String,
        analyze: bool,
        options: RunOptions,
    ) -> BoxFuture<Result<String, QueryError>> {
        match self {
            SqlSession::Postgres(session) => Box::pin(session.explain(statement, analyze, options)),
            _ => missing("Plans are drawn for PostgreSQL only"),
        }
    }

    pub fn explain_and_undo(
        &self,
        statement: String,
        options: RunOptions,
    ) -> BoxFuture<Result<String, QueryError>> {
        match self {
            SqlSession::Postgres(session) => Box::pin(session.explain_and_undo(statement, options)),
            _ => missing("Plans are drawn for PostgreSQL only"),
        }
    }

    pub fn in_transaction(&self) -> bool {
        each!(self, session, _d => session.in_transaction())
    }

    pub fn commit(&self) -> BoxFuture<Result<(), QueryError>> {
        each!(self, session, _d => Box::pin(session.commit()))
    }

    pub fn rollback(&self) -> BoxFuture<Result<(), QueryError>> {
        each!(self, session, _d => Box::pin(session.rollback()))
    }

    pub fn cancel(&self) {
        each!(self, session, _d => session.cancel())
    }

    /// Schemas with their tables, views, functions, triggers and enums.
    pub fn overview(&self) -> BoxFuture<Result<CatalogOverview, QueryError>> {
        each!(self, session, driver => Box::pin(driver::catalog::overview(session.clone())))
    }

    pub fn definition(&self, table: TableRef) -> BoxFuture<Result<TableDefinition, QueryError>> {
        each!(self, session, driver => Box::pin(driver::catalog::definition(session.clone(), table)))
    }

    pub fn function_definition(
        &self,
        function: FunctionInfo,
    ) -> BoxFuture<Result<String, QueryError>> {
        match self {
            SqlSession::Postgres(session) => Box::pin(kuery_pg::catalog::function_definition(
                session.clone(),
                function,
            )),
            SqlSession::Mysql(session) => Box::pin(kuery_mysql::catalog::function_definition(
                session.clone(),
                function,
            )),
            SqlSession::Sqlite(_) => missing("SQLite has no stored functions"),
        }
    }

    pub fn completion_catalog(&self) -> BoxFuture<Result<completion::Catalog, QueryError>> {
        each!(self, session, driver => Box::pin(driver::catalog::completion_catalog(session.clone())))
    }

    pub fn diagram(&self, schema: String) -> BoxFuture<Result<Diagram, QueryError>> {
        each!(self, session, driver => Box::pin(driver::catalog::diagram(session.clone(), schema)))
    }
}

/// Opens more connections like the one a [`crate::connection::Db`] has, for
/// write mode and Server Activity. It holds what it connects with
/// (password included) only in memory.
#[derive(Clone)]
pub enum Reconnect {
    Postgres { conninfo: Arc<str> },
    Sqlite { path: std::path::PathBuf },
    Mysql { target: Arc<kuery_mysql::Target> },
}

impl Reconnect {
    pub fn open(&self, engines: &Engines, access: Access) -> BoxFuture<Result<SqlSession, String>> {
        match self {
            Reconnect::Postgres { conninfo } => {
                let connect = engines.postgres.connect_with(conninfo.to_string(), access);
                Box::pin(async move {
                    connect
                        .await
                        .map(SqlSession::Postgres)
                        .map_err(|error| error.to_string())
                })
            }
            Reconnect::Sqlite { path } => {
                let open = kuery_sqlite::Session::open(path.clone(), access);
                Box::pin(async move { open.await.map(SqlSession::Sqlite) })
            }
            Reconnect::Mysql { target } => {
                let connect = engines.mysql.connect((**target).clone(), access);
                Box::pin(async move { connect.await.map(SqlSession::Mysql) })
            }
        }
    }
}

impl std::fmt::Debug for Reconnect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Reconnect(…)")
    }
}
