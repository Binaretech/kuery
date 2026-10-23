//! Read-only SQL against explicit local sources. No file paths in SQL.

use crate::{
    Error, FileDataset, FileFingerprint, FileSource, PreviewRequest, PreviewScope, Result, convert,
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

#[derive(Debug, Clone)]
pub struct SqlResult {
    pub results: ResultSet,
    /// This describes the query output, never a limit on input rows.
    pub scope: PreviewScope,
}

/// Synchronous: call on a worker. The output limit is applied after SQL, so
/// aggregates and joins see entire inputs. It does not bound engine working memory.
pub fn query_files(files: &[SqlFile], sql: &str, output: PreviewRequest) -> Result<SqlResult> {
    crate::contained(|| run_query(files, sql, output))
}

fn run_query(files: &[SqlFile], sql: &str, output: PreviewRequest) -> Result<SqlResult> {
    output.validate()?;
    validate(sql)?;
    let mut context = SQLContext::new();
    let mut datasets = Vec::with_capacity(files.len());
    let mut names = std::collections::HashSet::new();
    for file in files {
        if file.name.is_empty() || !names.insert(file.name.clone()) {
            return Err(Error::Sql(
                "File table names must be nonempty and unique".into(),
            ));
        }
        let dataset = FileDataset::open(file.source.clone())?;
        if dataset.fingerprint() != &file.fingerprint {
            return Err(Error::FileChanged(file.source.path.clone()));
        }
        context.register(&file.name, dataset.sql_plan()?);
        datasets.push(dataset);
    }
    let first = datasets
        .first()
        .ok_or_else(|| Error::Sql("Open a data file before running SQL".into()))?;
    let result = context
        .execute(sql)
        .and_then(|plan| plan.limit((output.row_limit + 1) as IdxSize).collect())
        .map_err(|e| Error::Sql(e.to_string()))
        .and_then(|frame| {
            convert::preview(
                &frame,
                None,
                first.source(),
                output,
                first.fingerprint().clone(),
            )
        });
    // Even a failed read may have raced with replacement. Never publish stale data.
    for dataset in &datasets {
        dataset.check_unchanged()?;
    }
    let preview = result?;
    Ok(SqlResult {
        results: preview.results,
        scope: preview.scope,
    })
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
