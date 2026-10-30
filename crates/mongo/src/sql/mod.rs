//! Read-only SQL over MongoDB: one `SELECT` of one collection, translated
//! into the `aggregate` that answers it, which is what the console shows
//! and runs. SQL never runs by itself: the native text goes through the
//! same classifier and protocol guard as one written by hand, and this
//! module checks both before it hands a translation over, so a translation
//! that is not a read is refused as a fault of Kuery's.
//!
//! The SQL is "Kuery's SQL for documents", read with sqlparser's generic
//! dialect: what it cannot translate faithfully it refuses, with the bytes
//! at fault, and never approximates. The rules it follows (three-valued
//! logic, null against missing, arrays, `LIKE`) are the lowering's
//! ([`lower`]).

mod filter;
mod lower;
mod names;

#[cfg(test)]
mod tests;

use std::ops::Range;

use kuery_core::analysis::StatementKind;
use kuery_core::document::language::ParseError;
use kuery_core::driver::{QueryFrontend, TranslateContext, Translation};
use sqlparser::ast::{
    GroupByExpr, Query, Select, SelectFlavor, SetExpr, Spanned, Statement, TableFactor,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::{Parser, ParserError};

use crate::shell::{self, Op};
use crate::{classify, command, wire};

use names::Text;

/// What a write is refused with.
pub const READ_ONLY: &str =
    "Kuery's SQL over MongoDB only reads: switch to mongosh or edit in the grid";

/// SQL for the console's language picker.
pub const FRONTEND: QueryFrontend = QueryFrontend {
    name: "SQL",
    editor: "sql",
    split,
    translate,
};

/// Where each statement of a script is, by its semicolons (outside
/// strings, quoted names and comments).
pub fn split(text: &str) -> Vec<Range<usize>> {
    match kuery_core::script::statements(text) {
        Some(ranges) => ranges,
        // A script the tokenizer cannot read (an unterminated string) is
        // one statement, so translating it says where it breaks.
        None if text.trim().is_empty() => Vec::new(),
        None => vec![Text::new(text).whole()],
    }
}

/// The `aggregate` one statement stands for, or why it has none. Spans are
/// byte offsets into `text`.
pub fn translate(text: &str, context: &TranslateContext) -> Result<Translation, Vec<ParseError>> {
    let text = Text::new(text);
    translate_in(&text, context).map_err(|error| vec![error])
}

fn translate_in(text: &Text, context: &TranslateContext) -> Result<Translation, ParseError> {
    let statements = Parser::parse_sql(&GenericDialect {}, text.text)
        .map_err(|error| text.parse_error(error))?;
    let statement = match statements.as_slice() {
        [statement] => statement,
        [] => return Err(text.error(text.whole(), "Write a SELECT")),
        [_, second, ..] => {
            let at = text.span(second.span()).unwrap_or_else(|| text.whole());
            return Err(text.error(at, "Run one statement at a time"));
        }
    };
    let query = query(text, statement)?;
    let select = select(text, query)?;
    let translation = lower::lower(text, context, query, select)?;
    check(text, &translation)?;
    Ok(translation)
}

/// The query a statement holds, if it is one.
fn query<'q>(text: &Text, statement: &'q Statement) -> Result<&'q Query, ParseError> {
    // sqlparser places few statements but queries, and some of them
    // wrongly (`CREATE TABLE` from its name): the whole text is at fault.
    let refuse = |message: &str| Err(text.error(text.whole(), message));
    match statement {
        Statement::Query(query) => Ok(query),
        Statement::Explain { .. } | Statement::ExplainTable { .. } => {
            refuse("Write the SELECT alone: Explain in the console shows the plan of what it runs")
        }
        Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::Savepoint { .. }
        | Statement::ReleaseSavepoint { .. } => refuse(&format!(
            "{READ_ONLY}; there are no transactions to begin or end"
        )),
        _ => refuse(READ_ONLY),
    }
}

/// The `SELECT` a query is, checked against what Kuery's SQL allows: a
/// query that is anything else (a `SELECT … INTO`, an `INSERT` hidden in
/// a set expression, a lock) is refused here, whatever the rest says.
fn select<'q>(text: &Text, query: &'q Query) -> Result<&'q Select, ParseError> {
    let whole = || text.of(query);
    if let Some(with) = &query.with {
        let at = text.span(with.span()).unwrap_or_else(whole);
        return Err(text.error(at, "WITH (common table expressions) is not supported"));
    }
    // sqlparser's span of a query leaves its locks and FETCH out.
    if !query.locks.is_empty() {
        return Err(text.error(text.whole(), format!("{READ_ONLY}: FOR UPDATE locks rows")));
    }
    if query.fetch.is_some() {
        return Err(text.error(text.whole(), "Write LIMIT n instead of FETCH"));
    }
    if query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return Err(text.error(whole(), "This clause is not supported in Kuery's SQL"));
    }
    let select = match &*query.body {
        SetExpr::Select(select) => select,
        SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_) | SetExpr::Merge(_) => {
            return Err(text.error(text.of(&*query.body), READ_ONLY));
        }
        SetExpr::SetOperation { .. } => {
            return Err(text.error(
                text.of(&*query.body),
                "UNION, INTERSECT and EXCEPT are not supported",
            ));
        }
        SetExpr::Query(_) => {
            return Err(text.error(
                text.of(&*query.body),
                "Write the SELECT without parentheses",
            ));
        }
        SetExpr::Values(_) | SetExpr::Table(_) => {
            return Err(text.error(text.of(&*query.body), "Write a SELECT of one collection"));
        }
    };
    let at = text.of(&select.select_token.0);
    let refuse = |message: &str| Err(text.error(at.clone(), message));
    if let Some(into) = &select.into {
        let at = text.span(into.span()).unwrap_or(at.clone());
        return Err(text.error(at, format!("{READ_ONLY}: SELECT … INTO writes a table")));
    }
    if select.distinct.is_some() {
        return refuse("DISTINCT is not supported yet: write GROUP BY");
    }
    if select.top.is_some() {
        return refuse("Write LIMIT n instead of TOP");
    }
    if select.flavor != SelectFlavor::Standard {
        return refuse("Write SELECT … FROM …");
    }
    if !select.optimizer_hints.is_empty()
        || select.select_modifiers.is_some()
        || select.exclude.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.qualify.is_some()
        || select.value_table_mode.is_some()
    {
        return refuse("This clause is not supported in Kuery's SQL");
    }
    if !select.named_window.is_empty() {
        return refuse("Window functions are not supported");
    }
    match &select.group_by {
        GroupByExpr::Expressions(_, modifiers) if modifiers.is_empty() => {}
        GroupByExpr::Expressions(..) => {
            return refuse(
                "GROUP BY takes fields only: ROLLUP, CUBE and the like are not supported",
            );
        }
        GroupByExpr::All(_) => return refuse("Name the fields to group by"),
    }
    match select.from.as_slice() {
        [] => refuse("Name the collection: SELECT … FROM orders"),
        [from] if !from.joins.is_empty() => {
            let at = text.of(&from.joins[0].relation);
            Err(text.error(
                at,
                "Joins are not supported: Kuery's SQL reads one collection",
            ))
        }
        [from] => match &from.relation {
            TableFactor::Table { .. } => Ok(select),
            TableFactor::Derived { .. } => {
                Err(text.error(text.of(&from.relation), "Subqueries are not supported"))
            }
            other => Err(text.error(text.of(other), "Read from a collection by its name")),
        },
        [_, second, ..] => Err(text.error(
            text.of(&second.relation),
            "Joins are not supported: Kuery's SQL reads one collection",
        )),
    }
}

/// Checks a translation with the layers every native statement goes
/// through: it reads back as the statement it was printed from, the
/// classifier sees a read that is not heavy, and the protocol guard lets
/// its command through. Failing any is a fault of Kuery's, and the
/// statement does not run.
fn check(text: &Text, translation: &Translation) -> Result<(), ParseError> {
    let fault = |why: &str| {
        Err(text.error(
            text.whole(),
            format!(
                "Kuery translated this into something it cannot vouch for ({why}), so it does not run; please report it: {}",
                translation.native
            ),
        ))
    };
    let native = std::iter::once(translation.native.as_str())
        .chain(translation.estimate.as_deref().map(|e| e.native.as_str()));
    for native in native {
        let op = match shell::statements(native) {
            Ok(statements) if statements.len() == 1 => statements.into_iter().next().unwrap().1,
            _ => return fault("it does not read back"),
        };
        let info = classify::classify(&op);
        if info.kind != StatementKind::Read || info.heavy {
            return fault("it is not a read");
        }
        let built = match command::build(&op, "kuery") {
            Ok(Some(built)) => built,
            _ => return fault("it is no command"),
        };
        if wire::guard(&built.command).is_err() {
            return fault("a read-only connection would not send it");
        }
        if !matches!(op, Op::Collection { .. }) {
            return fault("it reads no collection");
        }
    }
    Ok(())
}

impl Text<'_> {
    /// Where sqlparser stopped, with its message and a hint for the usual
    /// causes (a reserved word as a field, an array index).
    fn parse_error(&self, error: ParserError) -> ParseError {
        let message = match error {
            ParserError::TokenizerError(message) | ParserError::ParserError(message) => message,
            ParserError::RecursionLimitExceeded => {
                return self.error(self.whole(), "The statement nests too deeply");
            }
        };
        names::parse_error(self, &message)
    }
}
