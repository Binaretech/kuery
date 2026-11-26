//! Read-only SQL against explicit local sources: files, and results Kuery
//! already holds. No file paths in SQL.

use crate::{
    Error, FileDataset, FileFingerprint, FileSource, PreviewRequest, PreviewScope, Result, convert,
    table,
};
use kuery_core::result_set::ResultSet;
use polars::prelude::*;
use polars_sql::SQLContext;
use sqlparser::{
    ast::{Select, Statement, TableFactor, Visit, Visitor},
    dialect::GenericDialect,
    parser::Parser,
};
use std::ops::ControlFlow;

#[derive(Debug, Clone)]
pub struct SqlFile {
    pub name: String,
    pub source: FileSource,
    /// Captured when the file preview was read. Changes require reopening SQL.
    pub fingerprint: FileFingerprint,
}

/// A result already in memory, such as a database query's, queried by
/// `name` like a file. It is the rows as they were when it was added.
#[derive(Debug, Clone)]
pub struct SqlTable {
    pub name: String,
    pub results: ResultSet,
}

/// What a file SQL query can read.
#[derive(Debug, Clone)]
pub enum SqlSource {
    File(SqlFile),
    Table(SqlTable),
}

impl SqlSource {
    pub fn name(&self) -> &str {
        match self {
            SqlSource::File(file) => &file.name,
            SqlSource::Table(table) => &table.name,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SqlResult {
    pub results: ResultSet,
    /// This describes the query output, never a limit on input rows.
    pub scope: PreviewScope,
}

/// Synchronous: call on a worker. The output limit is applied after SQL, so
/// aggregates and joins see entire inputs. It does not bound engine working memory.
pub fn query_files(files: &[SqlFile], sql: &str, output: PreviewRequest) -> Result<SqlResult> {
    let sources: Vec<_> = files.iter().cloned().map(SqlSource::File).collect();
    query_sources(&sources, sql, output)
}

/// [`query_files`] over files and results together, each by its name.
pub fn query_sources(
    sources: &[SqlSource],
    sql: &str,
    output: PreviewRequest,
) -> Result<SqlResult> {
    crate::contained(|| run_query(sources, sql, output))
}

fn run_query(sources: &[SqlSource], sql: &str, output: PreviewRequest) -> Result<SqlResult> {
    output.validate()?;
    validate(sql)?;
    if sources.is_empty() {
        return Err(Error::Sql("Open a data file before running SQL".into()));
    }
    let mut context = SQLContext::new();
    let mut datasets = Vec::with_capacity(sources.len());
    let mut names = std::collections::HashSet::new();
    for source in sources {
        let name = source.name();
        if name.is_empty() || !names.insert(name.to_string()) {
            return Err(Error::Sql("Table names must be nonempty and unique".into()));
        }
        match source {
            SqlSource::File(file) => {
                let dataset = FileDataset::open(file.source.clone())?;
                if dataset.fingerprint() != &file.fingerprint {
                    return Err(Error::FileChanged(file.source.path.clone()));
                }
                context.register(name, dataset.sql_plan()?);
                datasets.push(dataset);
            }
            SqlSource::Table(held) => context.register(name, table::frame(&held.results)?.lazy()),
        }
    }
    let result = context
        .execute(sql)
        .and_then(|plan| plan.limit((output.row_limit + 1) as IdxSize).collect())
        .map_err(|e| Error::Sql(e.to_string()))
        .and_then(|frame| convert::rows(&frame, None, output, false, Error::Sql));
    // Even a failed read may have raced with replacement. Never publish stale data.
    for dataset in &datasets {
        dataset.check_unchanged()?;
    }
    let (results, scope, _) = result?;
    Ok(SqlResult { results, scope })
}

fn validate(sql: &str) -> Result<()> {
    let statements =
        Parser::parse_sql(&GenericDialect, sql).map_err(|e| Error::Sql(e.to_string()))?;
    if statements.len() != 1 || !matches!(statements[0], Statement::Query(_)) {
        return Err(Error::Sql(
            "Run one SELECT or WITH query at a time; file SQL is read only".into(),
        ));
    }
    struct ReadOnly;
    impl Visitor for ReadOnly {
        type Break = String;
        fn pre_visit_statement(&mut self, stmt: &Statement) -> ControlFlow<String> {
            if matches!(stmt, Statement::Query(_)) {
                ControlFlow::Continue(())
            } else {
                ControlFlow::Break("Only read-only queries are supported".into())
            }
        }
        fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<String> {
            if select.into.is_some() {
                ControlFlow::Break("SELECT INTO is not supported".into())
            } else {
                ControlFlow::Continue(())
            }
        }
        fn pre_visit_table_factor(&mut self, table: &TableFactor) -> ControlFlow<String> {
            match table {
                TableFactor::Table { args: None, .. } | TableFactor::Derived { .. } | TableFactor::NestedJoin { .. } => ControlFlow::Continue(()),
                _ => ControlFlow::Break("Use registered file tables; table functions and paths in SQL are not supported".into()),
            }
        }
    }
    if let ControlFlow::Break(error) = statements.visit(&mut ReadOnly) {
        return Err(Error::Sql(error));
    }
    Ok(())
}
