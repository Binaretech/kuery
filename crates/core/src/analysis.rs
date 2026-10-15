//! What a script does, before it runs in write mode.
//!
//! Every statement is classified (read, data change, schema change,
//! transaction control or other) and the ones that are easy to regret are
//! flagged: `UPDATE`/`DELETE` without `WHERE`, `TRUNCATE`, `DROP`, dropping
//! columns, and `GRANT`/`REVOKE`. The UI uses this to ask for confirmation;
//! the database still runs everything inside a transaction the user commits.

use std::fmt;
use std::ops::ControlFlow;

use sqlparser::ast::{AlterTableOperation, FromTable, Statement, Visit, Visitor};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

use crate::read_only::{self, Rejection};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    /// Only reads (what read-only mode accepts).
    Read,
    /// `INSERT`, `UPDATE`, `DELETE`, `MERGE`, `TRUNCATE`, `COPY`.
    Data,
    /// `CREATE`, `ALTER`, `DROP`, `COMMENT`, `GRANT`, `REVOKE`.
    Schema,
    /// `BEGIN`, `COMMIT`, `ROLLBACK`, savepoints: Kuery owns the transaction.
    Transaction,
    /// Anything else that is not a plain read (`SET`, `CALL`, `VACUUM`…).
    Other,
}

/// A statement worth a second look before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Danger {
    UpdateWithoutWhere(String),
    DeleteWithoutWhere(String),
    Truncate(String),
    Drop { object: String, names: String },
    DropColumn { table: String, columns: String },
    Privileges(String),
}

impl fmt::Display for Danger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Danger::UpdateWithoutWhere(table) => {
                write!(f, "UPDATE {table} has no WHERE: it changes every row")
            }
            Danger::DeleteWithoutWhere(table) => {
                write!(f, "DELETE FROM {table} has no WHERE: it removes every row")
            }
            Danger::Truncate(tables) => write!(f, "TRUNCATE empties {tables}"),
            Danger::Drop { object, names } => write!(f, "DROP {object} {names} removes it"),
            Danger::DropColumn { table, columns } => {
                write!(f, "Drops column {columns} from {table}, with its data")
            }
            Danger::Privileges(statement) => write!(f, "{statement} changes privileges"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementInfo {
    pub kind: StatementKind,
    /// Short description, e.g. `UPDATE orders`.
    pub summary: String,
    pub dangers: Vec<Danger>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    pub statements: Vec<StatementInfo>,
}

impl Analysis {
    /// Whether anything is more than a read.
    pub fn writes(&self) -> bool {
        self.statements
            .iter()
            .any(|s| s.kind != StatementKind::Read)
    }

    pub fn dangers(&self) -> impl Iterator<Item = &Danger> {
        self.statements.iter().flat_map(|s| &s.dangers)
    }

    /// The first statement that tries to control the transaction itself.
    pub fn transaction_control(&self) -> Option<&StatementInfo> {
        self.statements
            .iter()
            .find(|s| s.kind == StatementKind::Transaction)
    }
}

/// Analyses a script of one or more statements.
pub fn analyze(sql: &str) -> Result<Analysis, Rejection> {
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|error| Rejection::Parse(error.to_string()))?;
    if statements.is_empty() {
        return Err(Rejection::Empty);
    }
    Ok(Analysis {
        statements: statements.iter().map(describe).collect(),
    })
}

fn describe(statement: &Statement) -> StatementInfo {
    let mut dangers = DangerVisitor::default();
    let _ = statement.visit(&mut dangers);
    StatementInfo {
        kind: kind(statement),
        summary: summary(statement),
        dangers: dangers.found,
    }
}

fn kind(statement: &Statement) -> StatementKind {
    if read_only::check_statement(statement).is_ok() {
        return StatementKind::Read;
    }
    match statement {
        Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::Savepoint { .. }
        | Statement::ReleaseSavepoint { .. } => StatementKind::Transaction,
        Statement::Insert(_)
        | Statement::Update(_)
        | Statement::Delete(_)
        | Statement::Merge(_)
        | Statement::Truncate(_)
        | Statement::Copy { .. } => StatementKind::Data,
        // A read that writes underneath, e.g. a DELETE inside a CTE.
        Statement::Query(_) | Statement::Explain { .. } => StatementKind::Data,
        _ => match read_only::statement_kind(statement)
            .split_whitespace()
            .next()
            .unwrap_or_default()
        {
            "CREATE" | "ALTER" | "DROP" | "COMMENT" | "GRANT" | "REVOKE" => StatementKind::Schema,
            "COMMIT" | "ROLLBACK" | "BEGIN" | "START" | "END" | "ABORT" | "SAVEPOINT"
            | "RELEASE" => StatementKind::Transaction,
            _ => StatementKind::Other,
        },
    }
}

fn summary(statement: &Statement) -> String {
    match statement {
        Statement::Insert(insert) => format!("INSERT INTO {}", insert.table),
        Statement::Update(update) => format!("UPDATE {}", update.table),
        Statement::Delete(delete) => format!("DELETE FROM {}", delete_target(&delete.from)),
        Statement::Truncate(truncate) => format!("TRUNCATE {}", truncate_targets(truncate)),
        Statement::Drop {
            object_type, names, ..
        } => format!("DROP {object_type} {}", join(names)),
        Statement::AlterTable(alter) => format!("ALTER TABLE {}", alter.name),
        Statement::Query(_) => "SELECT".into(),
        other => read_only::statement_kind(other),
    }
}

#[derive(Default)]
struct DangerVisitor {
    found: Vec<Danger>,
}

impl Visitor for DangerVisitor {
    type Break = ();

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<()> {
        match statement {
            Statement::Update(update) if update.selection.is_none() => self
                .found
                .push(Danger::UpdateWithoutWhere(update.table.to_string())),
            Statement::Delete(delete) if delete.selection.is_none() => self
                .found
                .push(Danger::DeleteWithoutWhere(delete_target(&delete.from))),
            Statement::Truncate(truncate) => self
                .found
                .push(Danger::Truncate(truncate_targets(truncate))),
            Statement::Drop {
                object_type, names, ..
            } => self.found.push(Danger::Drop {
                object: object_type.to_string(),
                names: join(names),
            }),
            Statement::AlterTable(alter) => {
                for operation in &alter.operations {
                    if let AlterTableOperation::DropColumn { column_names, .. } = operation {
                        self.found.push(Danger::DropColumn {
                            table: alter.name.to_string(),
                            columns: join(column_names),
                        });
                    }
                }
            }
            Statement::Grant(_) | Statement::Revoke(_) => self
                .found
                .push(Danger::Privileges(read_only::statement_kind(statement))),
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

fn delete_target(from: &FromTable) -> String {
    match from {
        FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => join(tables),
    }
}

fn truncate_targets(truncate: &sqlparser::ast::Truncate) -> String {
    join(truncate.table_names.iter().map(|target| &target.name))
}

fn join<T: fmt::Display>(items: impl IntoIterator<Item = T>) -> String {
    items
        .into_iter()
        .map(|item| item.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(sql: &str) -> Vec<StatementKind> {
        analyze(sql)
            .unwrap()
            .statements
            .iter()
            .map(|s| s.kind)
            .collect()
    }

    fn dangers(sql: &str) -> Vec<Danger> {
        analyze(sql).unwrap().dangers().cloned().collect()
    }

    #[test]
    fn classifies_each_statement() {
        use StatementKind::*;
        assert_eq!(
            kinds(
                "select 1; insert into t values (1); update t set a = 1 where id = 2; \
                 create table x (id int); alter table x add column y int; vacuum t; \
                 begin; commit"
            ),
            [
                Read,
                Data,
                Data,
                Schema,
                Schema,
                Other,
                Transaction,
                Transaction
            ]
        );
        assert_eq!(
            kinds("with gone as (delete from t returning *) select * from gone"),
            [Data]
        );
        assert_eq!(kinds("truncate t"), [Data]);
        assert_eq!(kinds("grant select on t to public"), [Schema]);
    }

    #[test]
    fn reads_alone_are_not_writes() {
        assert!(!analyze("select 1; select now()").unwrap().writes());
        assert!(
            analyze("select 1; delete from t where id = 1")
                .unwrap()
                .writes()
        );
    }

    #[test]
    fn summarizes_statements() {
        let summaries: Vec<String> = analyze(
            "insert into public.orders (id) values (1); update orders set a = 1 where id = 1; \
             delete from orders where id = 1; drop table if exists a, b",
        )
        .unwrap()
        .statements
        .into_iter()
        .map(|s| s.summary)
        .collect();
        assert_eq!(
            summaries,
            [
                "INSERT INTO public.orders",
                "UPDATE orders",
                "DELETE FROM orders",
                "DROP TABLE a, b"
            ]
        );
    }

    #[test]
    fn flags_statements_that_are_easy_to_regret() {
        assert_eq!(
            dangers("update orders set total = 0"),
            [Danger::UpdateWithoutWhere("orders".into())]
        );
        assert_eq!(
            dangers("delete from orders"),
            [Danger::DeleteWithoutWhere("orders".into())]
        );
        assert_eq!(
            dangers("truncate orders, items"),
            [Danger::Truncate("orders, items".into())]
        );
        assert_eq!(
            dangers("drop table orders"),
            [Danger::Drop {
                object: "TABLE".into(),
                names: "orders".into()
            }]
        );
        assert_eq!(
            dangers("alter table orders drop column total"),
            [Danger::DropColumn {
                table: "orders".into(),
                columns: "total".into()
            }]
        );
        assert!(matches!(
            dangers("revoke select on orders from public").as_slice(),
            [Danger::Privileges(_)]
        ));
        // Also when hidden inside a read.
        assert_eq!(
            dangers("with gone as (delete from orders returning *) select * from gone"),
            [Danger::DeleteWithoutWhere("orders".into())]
        );
    }

    #[test]
    fn targeted_changes_are_not_flagged() {
        assert!(dangers("update orders set total = 0 where id = 1").is_empty());
        assert!(dangers("delete from orders where id = 1").is_empty());
        assert!(dangers("insert into orders (total) values (1)").is_empty());
        assert!(dangers("alter table orders add column note text").is_empty());
    }

    #[test]
    fn finds_transaction_control() {
        let analysis = analyze("update t set a = 1 where id = 1; commit").unwrap();
        assert_eq!(analysis.transaction_control().unwrap().summary, "COMMIT");
        assert!(
            analyze("update t set a = 1 where id = 1")
                .unwrap()
                .transaction_control()
                .is_none()
        );
    }

    #[test]
    fn rejects_empty_and_unparsable_sql() {
        assert_eq!(analyze("-- nothing"), Err(Rejection::Empty));
        assert!(matches!(analyze("updat t"), Err(Rejection::Parse(_))));
    }
}
