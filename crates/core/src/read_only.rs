//! Política de solo lectura.
//!
//! Primera capa de defensa: analiza el SQL antes de enviarlo y rechaza todo lo
//! que pueda escribir. La segunda capa la pone el driver, que ejecuta la query
//! dentro de una transacción `READ ONLY`.

use std::fmt;
use std::ops::ControlFlow;

use sqlparser::ast::{Expr, ObjectName, Query, SetExpr, Statement, Visit, Visitor};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

/// Funciones que tienen efectos aunque se llamen desde un `SELECT`.
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
    /// El SQL no se pudo analizar.
    Parse(String),
    /// No hay ninguna sentencia (vacío o solo comentarios).
    Empty,
    /// Hay más de una sentencia; en solo lectura se ejecuta una cada vez.
    MultipleStatements(usize),
    /// Sentencia que no es de lectura (INSERT, CREATE, SET…).
    Statement(String),
    /// `SELECT … FOR UPDATE` y similares bloquean filas.
    LockingClause,
    /// `SELECT … INTO` crea una tabla.
    SelectInto,
    /// Llamada a una función con efectos.
    Function(String),
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rejection::Parse(message) => write!(f, "No se pudo analizar el SQL: {message}"),
            Rejection::Empty => write!(f, "No hay ninguna sentencia que ejecutar"),
            Rejection::MultipleStatements(n) => write!(
                f,
                "Hay {n} sentencias; en solo lectura se ejecuta una cada vez"
            ),
            Rejection::Statement(kind) => {
                write!(f, "{kind} no está permitido en una query de solo lectura")
            }
            Rejection::LockingClause => write!(
                f,
                "FOR UPDATE / FOR SHARE bloquea filas y no está permitido en solo lectura"
            ),
            Rejection::SelectInto => write!(f, "SELECT … INTO crea una tabla y no está permitido"),
            Rejection::Function(name) => {
                write!(f, "La función {name}() tiene efectos y no está permitida")
            }
        }
    }
}

impl std::error::Error for Rejection {}

/// Comprueba que `sql` es exactamente una sentencia que solo lee datos.
pub fn check(sql: &str) -> Result<(), Rejection> {
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|error| Rejection::Parse(error.to_string()))?;

    match statements.as_slice() {
        [] => Err(Rejection::Empty),
        [statement] => match statement.visit(&mut ReadOnlyVisitor) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(rejection) => Err(rejection),
        },
        many => Err(Rejection::MultipleStatements(many.len())),
    }
}

struct ReadOnlyVisitor;

impl Visitor for ReadOnlyVisitor {
    type Break = Rejection;

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Rejection> {
        // Visita también las sentencias anidadas: un DELETE dentro de un CTE o
        // de un EXPLAIN ANALYZE llega aquí como sentencia propia.
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
        // Funciones usadas como tabla: `select * from dblink(...)`.
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

/// `SELECT … INTO tabla`, también dentro de UNION/INTERSECT.
fn creates_table(body: &SetExpr) -> bool {
    match body {
        SetExpr::Select(select) => select.into.is_some(),
        SetExpr::SetOperation { left, right, .. } => creates_table(left) || creates_table(right),
        _ => false,
    }
}

/// Nombre corto de la sentencia para el mensaje de error, p. ej. `DELETE`.
fn statement_kind(statement: &Statement) -> String {
    let text = statement.to_string();
    let mut words = text.split_whitespace();
    let first = words.next().unwrap_or("Esta sentencia").to_uppercase();
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
        assert_eq!(check(sql), Ok(()), "debería permitirse: {sql}");
    }

    fn rejected(sql: &str) -> Rejection {
        check(sql).expect_err(&format!("debería rechazarse: {sql}"))
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
        allowed("-- comentario\nselect now()");
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
        // `SET transaction_read_only = off` desactivaría la segunda capa.
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
        assert_eq!(rejected("-- solo un comentario"), Rejection::Empty);
        assert!(matches!(
            rejected("selec * fron orders"),
            Rejection::Parse(_)
        ));
        // Lo que el analizador no entiende se rechaza, aunque sea una lectura.
        assert!(matches!(rejected("table orders"), Rejection::Parse(_)));
    }
}
