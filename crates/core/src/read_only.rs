//! Read-only policy.
//!
//! First line of defense: parses the SQL before sending it and rejects anything
//! that could write. The second line is the driver, which runs the query inside
//! a `READ ONLY` transaction.

use std::fmt;
use std::ops::ControlFlow;

use sqlparser::ast::{Expr, ObjectName, Query, SetExpr, Statement, Visit, Visitor};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

/// Functions that have side effects even when called from a `SELECT`.
const SIDE_EFFECT_FUNCTIONS: &[&str] = &[
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

/// Checks that `sql` is exactly one statement that only reads data.
pub fn check(sql: &str) -> Result<(), Rejection> {
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|error| Rejection::Parse(error.to_string()))?;

    match statements.as_slice() {
        [] => Err(Rejection::Empty),
        [statement] => check_statement(statement),
        many => Err(Rejection::MultipleStatements(many.len())),
    }
}

/// Checks that one parsed statement only reads data.
pub(crate) fn check_statement(statement: &Statement) -> Result<(), Rejection> {
    match statement.visit(&mut ReadOnlyVisitor) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(rejection) => Err(rejection),
    }
}

struct ReadOnlyVisitor;

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
        );
        if allowed {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(Rejection::Statement(statement_kind(statement)))
        }
    }

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Rejection> {
        if !query.locks.is_empty() {
            return ControlFlow::Break(Rejection::LockingClause);
        }
        if creates_table(&query.body) {
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

/// `SELECT … INTO table`, also inside UNION/INTERSECT.
fn creates_table(body: &SetExpr) -> bool {
    match body {
        SetExpr::Select(select) => select.into.is_some(),
        SetExpr::SetOperation { left, right, .. } => creates_table(left) || creates_table(right),
        _ => false,
    }
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
