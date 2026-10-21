//! Read-only policy.
//!
//! First line of defense: parses the SQL before sending it and rejects anything
//! that could write. The second line is the driver, which runs the query inside
//! a `READ ONLY` transaction.

use std::fmt;
use std::ops::ControlFlow;

use sqlparser::ast::{
    Delete, Expr, FromTable, ObjectName, ObjectType, Query, SessionParamStatsTopic, SetExpr,
    SetSessionParamKind, Statement, TableFactor, TableObject, Visit, Visitor,
};
use sqlparser::parser::Parser;

use crate::sql::Dialect;

/// Functions that have side effects even when called from a `SELECT`.
const SIDE_EFFECT_FUNCTIONS: &[&str] = &[
    // MySQL: named locks, and reading files on the server.
    "get_lock",
    "release_lock",
    "release_all_locks",
    "load_file",
    // SQLite: loading native code, writing files, editing in an editor.
    "load_extension",
    "writefile",
    "edit",
    "dblink",
    "dblink_exec",
    "lo_export",
    "lo_import",
    "lo_unlink",
    "nextval",
    "pg_advisory_lock",
    "pg_advisory_xact_lock",
    "pg_cancel_backend",
    "pg_create_restore_point",
    "pg_logical_emit_message",
    "pg_notify",
    "pg_promote",
    "pg_reload_conf",
    "pg_rotate_logfile",
    "pg_switch_wal",
    "pg_terminate_backend",
    "pg_try_advisory_lock",
    "pg_try_advisory_xact_lock",
    "set_config",
    "setval",
    // T-SQL: running programs, reading files and other servers, mail.
    "xp_cmdshell",
    "openrowset",
    "opendatasource",
    "openquery",
    "sp_send_dbmail",
];

/// T-SQL stored procedures that only read. `EXEC` of anything else may write.
const READ_ONLY_PROCEDURES: &[&str] = &[
    "sp_help",
    "sp_helptext",
    "sp_helpindex",
    "sp_columns",
    "sp_tables",
    "sp_who",
    "sp_who2",
    "sp_spaceused",
    "sp_databases",
    "sp_server_info",
];

/// T-SQL `SET` options that only change how this session shows or reads
/// data. Anything else (`implicit_transactions`, `identity_insert`,
/// `rowcount`…) is refused.
const SESSION_OPTIONS: &[&str] = &[
    "nocount",
    "showplan_xml",
    "arithabort",
    "quoted_identifier",
    "language",
    "dateformat",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// The SQL could not be parsed.
    Parse(String),
    /// There is no statement (empty or only comments).
    Empty,
    /// More than one statement; read-only mode runs one at a time.
    MultipleStatements(usize),
    /// A statement that is not a read (INSERT, CREATE, SET…).
    Statement(String),
    /// `SELECT … FOR UPDATE` and similar clauses lock rows.
    LockingClause,
    /// `SELECT … INTO` creates a table.
    SelectInto,
    /// A call to a function with side effects.
    Function(String),
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rejection::Parse(message) => write!(f, "Could not parse the SQL: {message}"),
            Rejection::Empty => write!(f, "There is no statement to run"),
            Rejection::MultipleStatements(n) => {
                write!(f, "Found {n} statements; read-only mode runs one at a time")
            }
            Rejection::Statement(kind) => {
                write!(f, "{kind} is not allowed in a read-only query")
            }
            Rejection::LockingClause => write!(
                f,
                "FOR UPDATE / FOR SHARE locks rows and is not allowed in read-only mode"
            ),
            Rejection::SelectInto => write!(f, "SELECT … INTO creates a table and is not allowed"),
            Rejection::Function(name) => {
                write!(
                    f,
                    "The function {name}() has side effects and is not allowed"
                )
            }
        }
    }
}

impl std::error::Error for Rejection {}

/// Checks that `sql` is exactly one PostgreSQL statement that only reads
/// data.
pub fn check(sql: &str) -> Result<(), Rejection> {
    check_in(&crate::sql::POSTGRES, sql)
}

/// The same, for a database speaking `dialect`.
pub fn check_in(dialect: &Dialect, sql: &str) -> Result<(), Rejection> {
    if dialect.batch_separator.is_some() {
        return check_batch(dialect, sql);
    }
    let statements = Parser::parse_sql(&*dialect.parser(), sql)
        .map_err(|error| Rejection::Parse(error.to_string()))?;

    match statements.as_slice() {
        [] => Err(Rejection::Empty),
        [statement] => check_statement(statement),
        many => Err(Rejection::MultipleStatements(many.len())),
    }
}

/// A database whose script unit is a batch (T-SQL) runs several statements
/// at once, so one batch may hold many; they must all be reads or touch
/// only what the session itself made.
fn check_batch(dialect: &Dialect, sql: &str) -> Result<(), Rejection> {
    let batches = crate::script::statements_in(dialect, sql)
        .ok_or_else(|| Rejection::Parse("unterminated string, comment or identifier".into()))?;
    let batch = match batches.as_slice() {
        [] => return Err(Rejection::Empty),
        [batch] => &sql[batch.clone()],
        many => return Err(Rejection::MultipleStatements(many.len())),
    };
    let statements = Parser::parse_sql(&*dialect.parser(), batch)
        .map_err(|error| Rejection::Parse(error.to_string()))?;
    if statements.is_empty() {
        return Err(Rejection::Empty);
    }
    statements.iter().try_for_each(|statement| {
        match statement.visit(&mut ReadOnlyVisitor {
            session_local: true,
        }) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(rejection) => Err(rejection),
        }
    })
}

/// Checks that one parsed statement only reads data.
pub(crate) fn check_statement(statement: &Statement) -> Result<(), Rejection> {
    match statement.visit(&mut ReadOnlyVisitor {
        session_local: false,
    }) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(rejection) => Err(rejection),
    }
}

struct ReadOnlyVisitor {
    /// Also allow what only touches the session: variables, `#temp` tables,
    /// session options and a few read-only procedures (T-SQL).
    session_local: bool,
}

impl Visitor for ReadOnlyVisitor {
    type Break = Rejection;

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Rejection> {
        // Nested statements are visited too: a DELETE inside a CTE or an
        // EXPLAIN ANALYZE arrives here as its own statement.
        let allowed = matches!(
            statement,
            Statement::Query(_)
                | Statement::Explain { .. }
                | Statement::ExplainTable { .. }
                | Statement::ShowVariable { .. }
                | Statement::ShowVariables { .. }
                | Statement::ShowColumns { .. }
                | Statement::ShowTables { .. }
                | Statement::ShowCreate { .. }
                | Statement::ShowDatabases { .. }
                | Statement::ShowSchemas { .. }
                | Statement::ShowStatus { .. }
                | Statement::ShowFunctions { .. }
                | Statement::ShowCollation { .. }
                | Statement::ShowViews { .. }
        );
        if allowed || (self.session_local && is_session_local(statement)) {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(Rejection::Statement(statement_kind(statement)))
        }
    }

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Rejection> {
        if !query.locks.is_empty() {
            return ControlFlow::Break(Rejection::LockingClause);
        }
        if creates_table(&query.body, self.session_local) {
            return ControlFlow::Break(Rejection::SelectInto);
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Rejection> {
        match expr {
            Expr::Function(function) => check_function_name(&function.name),
            _ => ControlFlow::Continue(()),
        }
    }

    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<Rejection> {
        // Functions used as a table: `select * from dblink(...)`.
        check_function_name(relation)
    }
}

fn check_function_name(name: &ObjectName) -> ControlFlow<Rejection> {
    let full = name.to_string().to_lowercase();
    let unqualified = full.rsplit('.').next().unwrap_or(&full).trim_matches('"');
    if SIDE_EFFECT_FUNCTIONS.contains(&unqualified) {
        ControlFlow::Break(Rejection::Function(unqualified.to_string()))
    } else {
        ControlFlow::Continue(())
    }
}

/// `SELECT … INTO table`, also inside UNION/INTERSECT. With `session_local`
/// a `#temp` table is no table worth refusing.
fn creates_table(body: &SetExpr, session_local: bool) -> bool {
    match body {
        SetExpr::Select(select) => select.into.as_ref().is_some_and(|into| {
            !(session_local
                && !into.targets.is_empty()
                && into.targets.iter().all(|target| match target {
                    Expr::Identifier(ident) => is_temp_table(&ident.value),
                    _ => false,
                }))
        }),
        SetExpr::SetOperation { left, right, .. } => {
            creates_table(left, session_local) || creates_table(right, session_local)
        }
        _ => false,
    }
}

/// `#t` and `##t`.
fn is_temp_table(name: &str) -> bool {
    name.starts_with('#')
}

/// A name that lives and dies with the session: `#t`, `##t` or `@t`.
fn is_session_local_name(name: &ObjectName) -> bool {
    match name.0.as_slice() {
        [part] => part
            .as_ident()
            .is_some_and(|ident| is_temp_table(&ident.value) || ident.value.starts_with('@')),
        _ => false,
    }
}

fn is_session_local_table(factor: &TableFactor) -> bool {
    matches!(factor, TableFactor::Table { name, .. } if is_session_local_name(name))
}

/// T-SQL statements that cannot change anything outside this session. Their
/// expressions are still visited, so a subquery or function in them is
/// checked like any other.
fn is_session_local(statement: &Statement) -> bool {
    match statement {
        Statement::Declare { .. } | Statement::WaitFor(_) => true,
        Statement::Set(set) => match set {
            sqlparser::ast::Set::SingleAssignment { variable, .. } => {
                variable.to_string().starts_with('@')
            }
            sqlparser::ast::Set::SetTransaction {
                modes,
                snapshot,
                session,
            } => {
                !session
                    && snapshot.is_none()
                    && modes.iter().all(|mode| {
                        matches!(mode, sqlparser::ast::TransactionMode::IsolationLevel(_))
                    })
            }
            sqlparser::ast::Set::SetSessionParam(param) => match param {
                SetSessionParamKind::Statistics(statistics) => matches!(
                    statistics.topic,
                    SessionParamStatsTopic::IO
                        | SessionParamStatsTopic::Time
                        | SessionParamStatsTopic::Xml
                ),
                SetSessionParamKind::Generic(generic) => match generic.names.as_slice() {
                    [name] => {
                        let name = name.to_string().to_lowercase();
                        SESSION_OPTIONS.contains(&name.as_str()) || name.starts_with("ansi_")
                    }
                    _ => false,
                },
                _ => false,
            },
            _ => false,
        },
        Statement::Execute {
            name: Some(name), ..
        } => is_read_only_procedure(name),
        Statement::CreateTable(create) => is_session_local_name(&create.name),
        Statement::Drop {
            object_type: ObjectType::Table,
            names,
            ..
        } => names.iter().all(is_session_local_name),
        Statement::Insert(insert) => {
            matches!(&insert.table, TableObject::TableName(name) if is_session_local_name(name))
        }
        Statement::Update(update) => is_session_local_table(&update.table.relation),
        Statement::Delete(delete) => deletes_only_session_local(delete),
        _ => false,
    }
}

/// `sp_help` and friends, as the system procedures (not a same-named one in
/// a user schema).
fn is_read_only_procedure(name: &ObjectName) -> bool {
    let parts: Vec<String> = name
        .0
        .iter()
        .filter_map(|part| part.as_ident())
        .map(|ident| ident.value.to_lowercase())
        .collect();
    let [prefix @ .., procedure] = parts.as_slice() else {
        return false;
    };
    let system_schema = matches!(
        prefix
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
        [] | ["sys"] | ["master", "dbo"] | ["master", "sys"]
    );
    system_schema && READ_ONLY_PROCEDURES.contains(&procedure.as_str())
}

fn deletes_only_session_local(delete: &Delete) -> bool {
    let (FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from)) = &delete.from;
    delete.using.is_none()
        && delete.tables.iter().all(is_session_local_name)
        && !from.is_empty()
        && from
            .iter()
            .all(|table| is_session_local_table(&table.relation))
}

/// Short statement name for the error message, e.g. `DELETE`.
pub(crate) fn statement_kind(statement: &Statement) -> String {
    let text = statement.to_string();
    let mut words = text.split_whitespace();
    let first = words.next().unwrap_or("This statement").to_uppercase();
    match first.as_str() {
        "CREATE" | "ALTER" | "DROP" => match words.next() {
            Some(object) => format!("{first} {}", object.to_uppercase()),
            None => first,
        },
        _ => first,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(sql: &str) {
        assert_eq!(check(sql), Ok(()), "should be allowed: {sql}");
    }

    fn rejected(sql: &str) -> Rejection {
        check(sql).expect_err(&format!("should be rejected: {sql}"))
    }

    #[test]
    fn reads_other_dialects() {
        assert_eq!(
            check_in(
                &crate::sql::MYSQL,
                "select `id` from `shop`.`orders` # note"
            ),
            Ok(())
        );
        assert_eq!(
            check_in(&crate::sql::MYSQL, "show create table orders"),
            Ok(())
        );
        assert_eq!(check_in(&crate::sql::MYSQL, "show databases"), Ok(()));
        assert!(matches!(
            check_in(&crate::sql::MYSQL, "select get_lock('x', 10)"),
            Err(Rejection::Function(name)) if name == "get_lock"
        ));
        assert!(check_in(&crate::sql::MYSQL, "delete from orders").is_err());
        assert_eq!(
            check_in(
                &crate::sql::SQLITE,
                "select * from \"main\".\"orders\" limit 3"
            ),
            Ok(())
        );
        assert!(matches!(
            check_in(&crate::sql::SQLITE, "select load_extension('x')"),
            Err(Rejection::Function(_))
        ));
        assert!(check_in(&crate::sql::SQLITE, "pragma journal_mode = delete").is_err());
        assert!(check_in(&crate::sql::SQLITE, "attach database 'x.db' as x").is_err());
    }

    #[test]
    fn allows_plain_reads() {
        allowed("select * from orders");
        allowed("select count(*) from orders where status = 'paid'");
        allowed("with recent as (select * from orders limit 10) select * from recent");
        allowed("select 1 union all select 2");
        allowed("values (1, 'a'), (2, 'b')");
        allowed("explain select * from orders");
        allowed("explain analyze select * from orders");
        allowed("show search_path");
        allowed("-- comment\nselect now()");
        allowed("select * from orders;");
    }

    #[test]
    fn rejects_writes_and_ddl() {
        for (sql, kind) in [
            ("insert into orders (total) values (1)", "INSERT"),
            ("update orders set total = 0", "UPDATE"),
            ("delete from orders", "DELETE"),
            ("truncate orders", "TRUNCATE"),
            ("drop table orders", "DROP TABLE"),
            ("create table x (id int)", "CREATE TABLE"),
            ("alter table orders add column x int", "ALTER TABLE"),
            ("grant select on orders to public", "GRANT"),
            ("copy orders to stdout", "COPY"),
            ("vacuum orders", "VACUUM"),
        ] {
            assert_eq!(rejected(sql), Rejection::Statement(kind.into()), "{sql}");
        }
    }

    #[test]
    fn rejects_session_and_transaction_control() {
        // `SET transaction_read_only = off` would disable the second line of defense.
        assert!(matches!(
            rejected("set transaction_read_only = off"),
            Rejection::Statement(_)
        ));
        assert!(matches!(rejected("begin"), Rejection::Statement(_)));
        assert!(matches!(rejected("commit"), Rejection::Statement(_)));
        assert!(matches!(rejected("rollback"), Rejection::Statement(_)));
    }

    #[test]
    fn rejects_writes_hidden_inside_reads() {
        assert_eq!(
            rejected("with gone as (delete from orders returning *) select * from gone"),
            Rejection::Statement("DELETE".into())
        );
        assert_eq!(
            rejected("explain analyze delete from orders"),
            Rejection::Statement("DELETE".into())
        );
        assert_eq!(
            rejected("select * from orders for update"),
            Rejection::LockingClause
        );
        assert_eq!(
            rejected("select * into orders_copy from orders"),
            Rejection::SelectInto
        );
    }

    #[test]
    fn rejects_side_effect_functions() {
        assert_eq!(
            rejected("select pg_terminate_backend(123)"),
            Rejection::Function("pg_terminate_backend".into())
        );
        assert_eq!(
            rejected("select pg_catalog.set_config('transaction_read_only', 'off', true)"),
            Rejection::Function("set_config".into())
        );
        assert_eq!(
            rejected("select * from orders where id = nextval('orders_id_seq')"),
            Rejection::Function("nextval".into())
        );
        assert_eq!(
            rejected("select * from dblink('host=x', 'delete from t') as t(n int)"),
            Rejection::Function("dblink".into())
        );
    }

    #[test]
    fn rejects_more_than_one_statement() {
        assert_eq!(
            rejected("select 1; delete from orders"),
            Rejection::MultipleStatements(2)
        );
        assert_eq!(
            rejected("select 1; select 2"),
            Rejection::MultipleStatements(2)
        );
    }

    #[test]
    fn t_sql_allows_session_local_statements_in_one_batch() {
        use crate::sql::MSSQL;
        let ok = |sql| assert_eq!(check_in(&MSSQL, sql), Ok(()), "{sql}");
        let no = |sql| assert!(check_in(&MSSQL, sql).is_err(), "{sql}");
        ok("declare @n int = 3; select top (@n) * from dbo.orders");
        ok("set @n = 4");
        ok("select * into #t from dbo.orders; select * from #t");
        ok("select * into ##t from dbo.orders");
        ok("exec sp_help 'dbo.orders'");
        ok("execute sp_who2");
        ok("set statistics io on");
        ok("set nocount on");
        ok("set transaction isolation level read uncommitted");
        ok("waitfor delay '00:00:01'");
        ok("waitfor delay '00:00:20'; select 1");
        ok("create table #t (id int); insert into #t values (1); drop table #t");
        ok("declare @t table (id int); insert into @t values (1); update @t set id = 2");
        ok("select 1\nGO");
        no("select * into dbo.copy from dbo.orders");
        no("select * into #t from dbo.orders; select * into dbo.copy from #t");
        no("exec dbo.recalculate");
        no("exec xp_cmdshell 'dir'");
        no("exec sp_send_dbmail @recipients = 'a@b.c'");
        no("select * from openrowset('SQLNCLI', 'x', 'select 1')");
        no("select * from openquery(srv, 'select 1')");
        no("select next value for dbo.seq");
        no("update dbo.orders set total = 0");
        no("insert into dbo.orders values (1)");
        no("drop table dbo.orders");
        no("set implicit_transactions on");
        no("set identity_insert dbo.orders on");
        no("waitfor delay '00:00:01'; delete from dbo.orders");
        no("exec ('delete from dbo.orders')");
        no("exec dbo.sp_help 'x'");
        no("set @n = (select count(*) from openquery(srv, 'select 1'))");
        assert_eq!(
            check_in(&MSSQL, "select * from openrowset('a', 'b', 'c')"),
            Err(Rejection::Function("openrowset".into()))
        );
        assert_eq!(
            check_in(&MSSQL, "select * into dbo.copy from dbo.orders"),
            Err(Rejection::SelectInto)
        );
        assert!(matches!(
            check_in(&MSSQL, "select 1\nGO\nselect 2"),
            Err(Rejection::MultipleStatements(2))
        ));
        assert_eq!(check_in(&MSSQL, "-- nothing\nGO"), Err(Rejection::Empty));
    }

    #[test]
    fn rejects_empty_and_invalid_sql() {
        assert_eq!(rejected(""), Rejection::Empty);
        assert_eq!(rejected("-- just a comment"), Rejection::Empty);
        assert!(matches!(
            rejected("selec * fron orders"),
            Rejection::Parse(_)
        ));
        // Anything the parser does not understand is rejected, even if it is a read.
        assert!(matches!(rejected("table orders"), Rejection::Parse(_)));
    }
}
