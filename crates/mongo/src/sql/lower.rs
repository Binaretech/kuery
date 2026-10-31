//! A checked `SELECT` as the `aggregate` that answers it.
//!
//! The pipeline is always `$match` (WHERE), `$group` with a `$project`
//! after it, a second `$match` (HAVING), `$sort`, `$skip`, `$limit` and a
//! last `$project`, each only when the query needs it. Rows come back
//! under synthetic names (`c0`, `c1`…), with values as `$literal`, so no
//! name an alias picks can clash with an operator, and the console builds
//! each row by those names in the order of [`Translation::columns`].
//!
//! SQL's semantics, kept on documents:
//!
//! - **Three-valued logic.** Each predicate compiles to two filters, the
//!   documents it is TRUE for and those it is FALSE for; an unknown
//!   predicate meets neither. `NOT` swaps them, `AND` and `OR` combine
//!   them. MongoDB's `$ne`, `$nin`, `$not` and `$nor` take in missing and
//!   null fields, so they are never how `<>` or `NOT` read: `a <> 5` is
//!   `{a: {$nin: [5, null]}}`.
//! - **Null and missing.** `IS NULL` holds for both; `missing(a)` only for
//!   a field that is not there. `IN` drops the NULLs of its list, and
//!   `NOT IN` with a NULL in it is never true.
//! - **Arrays.** `{"a.b": 5}` matches through an array at `a`, and `{a: 5}`
//!   matches an array holding 5. A predicate on `a.b` is unknown when `a`
//!   or `a.b` holds an array, which each prefix's `{$not: {$type:
//!   "array"}}` says; only `_id`, which MongoDB never lets be an array,
//!   goes without. `IS NULL` and `missing()` are unknown only when `a`
//!   holds one: an array at `a.b` is a value, there and not null. The
//!   sample never drops a guard.
//! - **Types.** `=` between types is false; `<` and `BETWEEN` between
//!   types are unknown, as MongoDB's type brackets have them. Order is
//!   MongoDB's: null and missing first when ascending.
//! - **LIKE** is a regular expression anchored at both ends (`\A…\z`) with
//!   the `s` option, its other characters escaped; `ILIKE` adds `i`.
//! - **Aggregates.** `COUNT(a)` counts what is neither null nor missing,
//!   `SUM` is NULL without any number to add (MongoDB's would be 0), and
//!   `GROUP BY` puts null and missing in one group. Without `GROUP BY`
//!   there is one row even for no documents, so the `$group` runs inside
//!   a `$facet`, which always answers.

use std::ops::Range;

use kuery_core::document::catalog::CollectionInfo;
use kuery_core::document::language::{ParseError, Style};
use kuery_core::document::schema::{FieldSchema, Part};
use kuery_core::document::value::{CollectionRef, Document, Value};
use kuery_core::driver::{OutputColumn, TranslateContext, Translation, Warning};
use sqlparser::ast::{
    BinaryOperator, DataType, DuplicateTreatment, Expr, FunctionArg, FunctionArgExpr,
    FunctionArguments, GroupByExpr, Ident, LimitClause, ObjectNamePart, OrderByKind, OrderBySort,
    Query, Select, SelectItem, SelectItemQualifiedWildcardKind, TableFactor, UnaryOperator,
    Value as SqlValue,
};

use super::filter::{Filter, doc};
use super::names::{Path, Text, unreadable};
use crate::literal;
use crate::shell::{self, Method, Op};

/// What an expression the SELECT does not take is refused with.
pub(super) const SELECT_TAKES: &str =
    "SELECT takes fields, values, and COUNT, SUM, AVG, MIN and MAX";

/// The translation of a checked `SELECT`.
pub(super) fn lower(
    text: &Text,
    context: &TranslateContext,
    query: &Query,
    select: &Select,
) -> Result<Translation> {
    let mut lowering = Lowering {
        text,
        context,
        alias: None,
        warnings: Vec::new(),
        warned: Vec::new(),
        sort_key: None,
        like: false,
    };
    lowering.translate(query, select)
}

type Result<T> = std::result::Result<T, ParseError>;

struct Lowering<'a> {
    text: &'a Text<'a>,
    context: &'a TranslateContext,
    /// The collection's alias (`o` in `FROM orders o`), else its name
    /// (`orders`), which paths may start with.
    alias: Option<String>,
    warnings: Vec<Warning>,
    /// Paths already warned about.
    warned: Vec<String>,
    /// The first field ORDER BY sorts the documents by, which an index
    /// may serve instead of the filter.
    sort_key: Option<String>,
    /// A LIKE is in the query, which the collation does not reach.
    like: bool,
}

/// One column of the SELECT.
#[derive(Debug, Clone)]
struct Item {
    column: Column,
    label: String,
    /// The name it was given with `AS`.
    alias: Option<String>,
    span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
enum Column {
    Path(Path),
    Literal(Value),
    Aggregate(Aggregate),
}

#[derive(Debug, Clone, PartialEq)]
enum Aggregate {
    CountAll,
    Count(Path),
    Sum(Path),
    Avg(Path),
    Min(Path),
    Max(Path),
}

impl Aggregate {
    /// Its result is a number or null, never an array.
    fn scalar(&self) -> bool {
        !matches!(self, Aggregate::Min(_) | Aggregate::Max(_))
    }
}

/// A field a predicate tests, as the stage it filters names it.
struct Operand {
    path: String,
    /// The fields that must not hold an array for the predicate to be
    /// known: each prefix of the path, itself last; none for a field
    /// known to be scalar.
    guards: Vec<String>,
}

/// A predicate as the documents it is TRUE for and those it is FALSE for.
#[derive(Debug, Clone, PartialEq)]
struct Pair {
    yes: Filter,
    no: Filter,
}

impl Pair {
    fn not(self) -> Pair {
        Pair {
            yes: self.no,
            no: self.yes,
        }
    }
}

/// What the stages after `$group` call its outputs.
#[derive(Debug, Clone, PartialEq)]
enum Output {
    Key(usize),
    Aggregate(usize),
    Literal(Value),
}

/// GROUP BY, the aggregates the query names, and the fields `$group`'s
/// `$project` gives each.
struct Grouping {
    keys: Vec<Path>,
    aggregates: Vec<Aggregate>,
    /// Each output and its name: `c0`… for the SELECT's columns, in
    /// order, then `h0`… for what only HAVING and ORDER BY use.
    outputs: Vec<(Output, String)>,
    hidden: usize,
}

impl Grouping {
    fn aggregate(&mut self, aggregate: &Aggregate) -> usize {
        match self.aggregates.iter().position(|known| known == aggregate) {
            Some(at) => at,
            None => {
                self.aggregates.push(aggregate.clone());
                self.aggregates.len() - 1
            }
        }
    }

    /// The name of an output, given one when only HAVING or ORDER BY uses
    /// it.
    fn name(&mut self, output: Output) -> String {
        if let Some((_, name)) = self.outputs.iter().find(|(known, _)| *known == output) {
            return name.clone();
        }
        let name = format!("h{}", self.hidden);
        self.hidden += 1;
        self.outputs.push((output, name.clone()));
        name
    }
}

/// Where a query's names resolve: in the documents, or in the groups.
enum Scope<'g> {
    Documents,
    Groups {
        grouping: &'g mut Grouping,
        items: &'g [Item],
    },
}

impl Lowering<'_> {
    fn error<T>(&self, span: Range<usize>, message: impl Into<String>) -> Result<T> {
        Err(self.text.error(span, message))
    }

    fn translate(&mut self, query: &Query, select: &Select) -> Result<Translation> {
        let target = self.from(&select.from[0].relation)?;
        let (items, wildcard) = self.items(select)?;
        let group_by = match &select.group_by {
            GroupByExpr::Expressions(exprs, _) => exprs.as_slice(),
            GroupByExpr::All(_) => &[],
        };
        let order_by: &[_] = match &query.order_by {
            None => &[],
            Some(order_by) => match &order_by.kind {
                OrderByKind::Expressions(exprs) => exprs,
                OrderByKind::All(_) => {
                    return self.error(self.text.whole(), "ORDER BY takes fields");
                }
            },
        };
        if query
            .order_by
            .as_ref()
            .is_some_and(|o| o.interpolate.is_some())
        {
            return self.error(self.text.whole(), "INTERPOLATE is not supported");
        }
        let aggregated = items
            .iter()
            .any(|item| matches!(item.column, Column::Aggregate(_)))
            || order_by
                .iter()
                .map(|order| self.aggregate(&order.expr))
                .collect::<Result<Vec<_>>>()?
                .iter()
                .any(Option::is_some);
        let grouped = !group_by.is_empty() || select.having.is_some() || aggregated;
        if grouped && wildcard {
            let at = self.text.of(&select.projection[0]);
            return self.error(
                at,
                "Name the fields: * cannot go with GROUP BY or an aggregate",
            );
        }

        let mut stages = Vec::new();
        if let Some(selection) = &select.selection {
            let pair = self.predicate(selection, &mut Scope::Documents)?;
            if pair.yes != Filter::All {
                stages.push(stage("$match", Value::Object(pair.yes.document())));
            }
        }
        let (skip, limit) = self.limits(query)?;
        let mut columns = Vec::new();
        let mut estimate = None;
        if grouped {
            let mut grouping = self.grouping(group_by, &items)?;
            let having = match &select.having {
                Some(having) => Some(self.predicate(
                    having,
                    &mut Scope::Groups {
                        grouping: &mut grouping,
                        items: &items,
                    },
                )?),
                None => None,
            };
            let sort = self.group_sort(order_by, &items, &mut grouping)?;
            stages.extend(group_stages(&grouping));
            if let Some(having) = having
                && having.yes != Filter::All
            {
                stages.push(stage("$match", Value::Object(having.yes.document())));
            }
            push_sort_and_limits(&mut stages, sort, skip, limit);
            if grouping.hidden > 0 {
                let hidden = grouping
                    .outputs
                    .iter()
                    .filter(|(_, name)| name.starts_with('h'))
                    .map(|(_, name)| (name.clone(), Value::Int32(0)));
                stages.push(stage("$project", Value::Object(Document(hidden.collect()))));
            }
            if grouping.keys.is_empty()
                && select.selection.is_none()
                && select.having.is_none()
                && order_by.is_empty()
                && skip.is_none()
                && limit.is_none()
                && matches!(
                    items.as_slice(),
                    [Item {
                        column: Column::Aggregate(Aggregate::CountAll),
                        ..
                    }]
                )
            {
                estimate = Some(Box::new(estimated(&target, &items[0].label)));
            }
        } else {
            let sort = self.document_sort(order_by, &items)?;
            push_sort_and_limits(&mut stages, sort, skip, limit);
            if !wildcard {
                let mut project = vec![("_id".to_string(), Value::Int32(0))];
                for (at, item) in items.iter().enumerate() {
                    let value = match &item.column {
                        Column::Path(path) => Value::String(format!("${}", path.dotted())),
                        Column::Literal(value) => literal_expression(value.clone()),
                        Column::Aggregate(_) => unreachable!("a query with aggregates is grouped"),
                    };
                    project.push((format!("c{at}"), value));
                }
                stages.push(stage("$project", Value::Object(Document(project))));
            }
        }
        if !wildcard {
            columns = items
                .iter()
                .enumerate()
                .map(|(at, item)| OutputColumn {
                    field: format!("c{at}"),
                    label: item.label.clone(),
                })
                .collect();
        }

        let op = Op::Collection {
            target: target.clone(),
            method: Method::Aggregate,
            args: vec![Value::Array(stages)],
            chain: Vec::new(),
        };
        // A LIMIT stops the read early only when no `$group` (or `$facet`)
        // before it has read every document first.
        let limited = limit.is_some() && !grouped;
        let full_scan = match &self.context.collection {
            Some(info) => {
                let mut need = match &select.selection {
                    Some(selection) => self.need(selection),
                    None => Need::Nothing,
                };
                if let Some(key) = self.sort_key.take() {
                    need = Need::Any(vec![need, Need::Field(key)]);
                }
                !limited && !need.met(&|path| indexed(info, path))
            }
            None => false,
        };
        Ok(Translation {
            native: shell::print(&op),
            target,
            columns,
            warnings: std::mem::take(&mut self.warnings),
            notes: self.notes(),
            full_scan,
            estimate,
        })
    }

    /// The collection read, and its alias.
    fn from(&mut self, factor: &TableFactor) -> Result<CollectionRef> {
        let TableFactor::Table {
            name,
            alias,
            args,
            with_hints,
            version,
            with_ordinality,
            partitions,
            json_path,
            sample,
            index_hints,
        } = factor
        else {
            return self.error(self.text.of(factor), "Read from a collection by its name");
        };
        let at = self.text.of(factor);
        if args.is_some()
            || !with_hints.is_empty()
            || version.is_some()
            || *with_ordinality
            || !partitions.is_empty()
            || json_path.is_some()
            || sample.is_some()
            || !index_hints.is_empty()
        {
            return self.error(at, "Read from a collection by its name");
        }
        let mut parts = Vec::new();
        for part in &name.0 {
            match part {
                ObjectNamePart::Identifier(ident) => parts.push(ident),
                ObjectNamePart::Function(_) => {
                    return self.error(at, "Read from a collection by its name");
                }
            }
        }
        let target = match parts.as_slice() {
            [collection] => vec![collection.value.clone()],
            [database, collection]
                if database.quote_style.is_some() || collection.quote_style.is_some() =>
            {
                if !database_name(&database.value) {
                    let at = self.ident_span(database);
                    return self.error(at, format!("{} is not a database's name", database.value));
                }
                vec![database.value.clone(), collection.value.clone()]
            }
            [database, collection] => {
                let (a, b) = (&database.value, &collection.value);
                return self.error(
                    self.text.of(name),
                    format!(
                        "{a}.{b} may be the collection {b} of the database {a} or a collection named {a}.{b}: write \"{a}\".\"{b}\" for the first, \"{a}.{b}\" for the second"
                    ),
                );
            }
            _ => {
                return self.error(
                    self.text.of(name),
                    "Name one collection: orders, \"shop\".\"orders\", or \"name.with.dots\"",
                );
            }
        };
        if target.last().is_some_and(String::is_empty) {
            return self.error(at, "A collection's name is not empty");
        }
        // As in SQL, the collection's own name qualifies its fields
        // (`orders.status`) unless an alias takes its place.
        self.alias = match alias {
            Some(alias) if !alias.columns.is_empty() => {
                return self.error(at, "A collection's alias names no columns");
            }
            Some(alias) => Some(alias.name.value.clone()),
            None => target.last().cloned(),
        };
        let parts: Vec<&str> = target.iter().map(String::as_str).collect();
        Ok(CollectionRef::new(&parts))
    }

    fn ident_span(&self, ident: &Ident) -> Range<usize> {
        self.text
            .span(ident.span)
            .unwrap_or_else(|| self.text.whole())
    }

    /// The SELECT's columns, and whether it is `*`.
    fn items(&mut self, select: &Select) -> Result<(Vec<Item>, bool)> {
        let mut items = Vec::new();
        let mut wildcard = false;
        for item in &select.projection {
            let span = self.text.of(item);
            let (expr, alias) = match item {
                SelectItem::UnnamedExpr(expr) => (expr, None),
                SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.value.clone())),
                SelectItem::ExprWithAliases { .. } => {
                    return self.error(span, "Give a column one name");
                }
                SelectItem::Wildcard(options) => {
                    if !plain_wildcard(options) {
                        return self.error(span, "Write * alone, without options");
                    }
                    wildcard = true;
                    continue;
                }
                SelectItem::QualifiedWildcard(kind, options) => {
                    let named = match kind {
                        SelectItemQualifiedWildcardKind::ObjectName(name) => {
                            match name.0.as_slice() {
                                [ObjectNamePart::Identifier(ident)] => Some(&ident.value),
                                _ => None,
                            }
                        }
                        SelectItemQualifiedWildcardKind::Expr(_) => None,
                    };
                    if named.is_none() || named != self.alias.as_ref() || !plain_wildcard(options) {
                        return self.error(span, "Write * or the collection's alias and .*");
                    }
                    wildcard = true;
                    continue;
                }
            };
            let column = self.column(expr)?;
            items.push(Item {
                column,
                label: alias.clone().unwrap_or_else(|| expr.to_string()),
                alias,
                span,
            });
        }
        if wildcard && !items.is_empty() {
            return self.error(items[0].span.clone(), "Write * alone, or name the fields");
        }
        Ok((items, wildcard))
    }

    fn column(&mut self, expr: &Expr) -> Result<Column> {
        if let Some(path) = self.path(expr)? {
            return Ok(Column::Path(path));
        }
        if let Some(aggregate) = self.aggregate(expr)? {
            return Ok(Column::Aggregate(aggregate));
        }
        if let Some((value, _)) = self.literal(expr)? {
            return Ok(Column::Literal(value));
        }
        let at = self.text.of(expr);
        match expr {
            Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
                self.error(at, "Subqueries are not supported")
            }
            Expr::Function(function) if function.over.is_some() => {
                self.error(at, "Window functions are not supported")
            }
            _ => self.error(at, SELECT_TAKES),
        }
    }

    /// The field an expression names, if it names one: `a`, `o.a.b`, or
    /// `user`, which sqlparser reads as a function without parentheses.
    /// SQL's other such functions (`CURRENT_TIMESTAMP`, `SESSION_USER`…)
    /// are refused: read as fields, they would find nothing.
    fn path(&self, expr: &Expr) -> Result<Option<Path>> {
        let idents: Vec<&Ident> = match expr {
            Expr::Identifier(ident) => vec![ident],
            Expr::CompoundIdentifier(idents) => {
                let mut idents: Vec<&Ident> = idents.iter().collect();
                if idents.len() > 1 && Some(&idents[0].value) == self.alias.as_ref() {
                    idents.remove(0);
                }
                idents
            }
            Expr::Nested(inner) => return self.path(inner),
            Expr::Function(function)
                if matches!(function.args, FunctionArguments::None)
                    && matches!(function.parameters, FunctionArguments::None)
                    && function.over.is_none()
                    && function.filter.is_none() =>
            {
                match function.name.0.as_slice() {
                    [ObjectNamePart::Identifier(ident)]
                        if ident.value.eq_ignore_ascii_case("user") =>
                    {
                        vec![ident]
                    }
                    [ObjectNamePart::Identifier(ident)] => {
                        return self.error(
                            self.ident_span(ident),
                            format!(
                                "{0} is a function of SQL, which Kuery's SQL does not run: for a field of that name write \"{0}\"",
                                ident.value
                            ),
                        );
                    }
                    _ => return Ok(None),
                }
            }
            Expr::CompoundFieldAccess { .. } | Expr::JsonAccess { .. } => {
                return self.error(
                    self.text.of(expr),
                    "Kuery's SQL does not index arrays: a path takes field names only",
                );
            }
            _ => return Ok(None),
        };
        let mut parts = Vec::with_capacity(idents.len());
        for ident in idents {
            if let Some(why) = unreadable(&ident.value) {
                return self.error(self.ident_span(ident), why);
            }
            parts.push(ident.value.clone());
        }
        Ok(Some(Path(parts)))
    }

    /// The aggregate an expression calls, if it calls one.
    fn aggregate(&self, expr: &Expr) -> Result<Option<Aggregate>> {
        let Expr::Function(function) = expr else {
            return Ok(None);
        };
        let name = match function.name.0.as_slice() {
            [ObjectNamePart::Identifier(ident)] => ident.value.to_ascii_uppercase(),
            _ => return Ok(None),
        };
        if !matches!(name.as_str(), "COUNT" | "SUM" | "AVG" | "MIN" | "MAX") {
            return Ok(None);
        }
        let at = self.text.of(expr);
        if function.over.is_some() {
            return self.error(at, "Window functions are not supported");
        }
        if function.filter.is_some()
            || function.null_treatment.is_some()
            || !function.within_group.is_empty()
            || !matches!(function.parameters, FunctionArguments::None)
        {
            return self.error(at, format!("{name} takes one field, without clauses"));
        }
        let FunctionArguments::List(list) = &function.args else {
            return self.error(at, format!("{name} takes one field: {name}(total)"));
        };
        if list.duplicate_treatment == Some(DuplicateTreatment::Distinct) {
            return self.error(at, "DISTINCT is not supported yet: write GROUP BY");
        }
        if !list.clauses.is_empty() {
            return self.error(at, format!("{name} takes one field, without clauses"));
        }
        let arg = match list.args.as_slice() {
            [FunctionArg::Unnamed(arg)] => arg,
            _ => return self.error(at, format!("{name} takes one field: {name}(total)")),
        };
        let path = match arg {
            FunctionArgExpr::Wildcard if name == "COUNT" => return Ok(Some(Aggregate::CountAll)),
            FunctionArgExpr::Expr(expr) => match self.path(expr)? {
                Some(path) => path,
                None => {
                    let at = self.text.of(expr);
                    return self.error(at, format!("{name} takes a field"));
                }
            },
            _ => return self.error(at, format!("{name} takes one field: {name}(total)")),
        };
        Ok(Some(match name.as_str() {
            "COUNT" => Aggregate::Count(path),
            "SUM" => Aggregate::Sum(path),
            "AVG" => Aggregate::Avg(path),
            "MIN" => Aggregate::Min(path),
            _ => Aggregate::Max(path),
        }))
    }

    /// The value an expression writes, if it writes one; NULL is
    /// [`Value::Null`].
    fn literal(&self, expr: &Expr) -> Result<Option<(Value, Range<usize>)>> {
        let at = self.text.of(expr);
        let value = match expr {
            Expr::Nested(inner) => return self.literal(inner),
            Expr::Value(value) => match &value.value {
                SqlValue::Number(digits, _) => self.number(digits, at.clone())?,
                SqlValue::SingleQuotedString(text)
                | SqlValue::NationalStringLiteral(text)
                | SqlValue::EscapedStringLiteral(text)
                | SqlValue::UnicodeStringLiteral(text) => Value::String(text.clone()),
                SqlValue::Boolean(value) => Value::Bool(*value),
                SqlValue::Null => Value::Null,
                SqlValue::Placeholder(_) => {
                    return self.error(at, "Kuery's SQL takes values, not parameters");
                }
                other => {
                    return self.error(at, format!("{other} is not a value Kuery's SQL reads"));
                }
            },
            Expr::UnaryOp {
                op: op @ (UnaryOperator::Minus | UnaryOperator::Plus),
                expr: inner,
            } => match &**inner {
                Expr::Value(value) => match &value.value {
                    SqlValue::Number(digits, _) => {
                        // sqlparser places the number without its sign.
                        let before = self.text.text[..at.start].trim_end();
                        let at = match before.ends_with(['-', '+']) {
                            true => before.len() - 1..at.end,
                            false => at,
                        };
                        let value = match op {
                            UnaryOperator::Minus => {
                                self.number(&format!("-{digits}"), at.clone())?
                            }
                            _ => self.number(digits, at.clone())?,
                        };
                        return Ok(Some((value, at)));
                    }
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            },
            Expr::TypedString(typed) => {
                let at = self.text.with_word_before(at.clone());
                let Some(text) = typed.value.value.clone().into_string() else {
                    return self.error(at, "Write the value in single quotes");
                };
                let constructor = match &typed.data_type {
                    DataType::Decimal(_)
                    | DataType::Numeric(_)
                    | DataType::Dec(_)
                    | DataType::BigDecimal(_)
                    | DataType::BigNumeric(_) => "NumberDecimal",
                    DataType::Timestamp(..) => "ISODate",
                    _ => {
                        return self.error(
                            at,
                            "Write TIMESTAMP '2026-10-28T12:00:00Z' for a date, DECIMAL '9.99' for a decimal",
                        );
                    }
                };
                self.construct(constructor, &Value::String(text), at.clone())?
            }
            Expr::Function(function) => {
                let name = match function.name.0.as_slice() {
                    [ObjectNamePart::Identifier(ident)] => ident.value.as_str(),
                    _ => return Ok(None),
                };
                let Some(constructor) = ["ObjectId", "ISODate", "NumberLong", "NumberDecimal"]
                    .into_iter()
                    .find(|known| known.eq_ignore_ascii_case(name))
                else {
                    return Ok(None);
                };
                let arg = match &function.args {
                    FunctionArguments::List(list)
                        if list.duplicate_treatment.is_none() && list.clauses.is_empty() =>
                    {
                        match list.args.as_slice() {
                            [FunctionArg::Unnamed(FunctionArgExpr::Expr(arg))] => {
                                self.literal(arg)?
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                match arg {
                    Some((value @ (Value::String(_) | Value::Int32(_) | Value::Int64(_)), _)) => {
                        self.construct(constructor, &value, at.clone())?
                    }
                    _ => {
                        return self.error(
                            at,
                            format!("{constructor} takes one value in quotes: {constructor}('…')"),
                        );
                    }
                }
            }
            _ => return Ok(None),
        };
        Ok(Some((value, at)))
    }

    /// A typed value, read by mongosh's grammar from `Constructor(arg)`.
    fn construct(&self, constructor: &str, arg: &Value, at: Range<usize>) -> Result<Value> {
        let text = format!("{constructor}({})", literal::render(arg, Style::Compact));
        literal::parse_value(&text).or_else(|error| self.error(at, error.message))
    }

    /// A number as written: a whole one an Int32 or an Int64, refused past
    /// that rather than rounded into a Double; one with a point or an
    /// exponent a Double, refused when a Double cannot hold it.
    fn number(&self, text: &str, at: Range<usize>) -> Result<Value> {
        let value = literal::number(text);
        let whole = !text.contains(['.', 'e', 'E']);
        if whole && matches!(value, Value::Double(_)) {
            return self.error(
                at,
                format!(
                    "{text} does not fit in a 64-bit integer, and as a Double it would lose digits: write DECIMAL '{text}'"
                ),
            );
        }
        if matches!(value, Value::Double(double) if !double.is_finite()) {
            return self.error(
                at,
                format!("{text} is too large for a Double: write DECIMAL '{text}'"),
            );
        }
        Ok(value)
    }

    /// GROUP BY's keys, and the SELECT's columns as outputs of the groups.
    fn grouping(&mut self, group_by: &[Expr], items: &[Item]) -> Result<Grouping> {
        let mut keys: Vec<Path> = Vec::new();
        for expr in group_by {
            let path = match self.selected(expr, items)? {
                Some((
                    _,
                    Item {
                        column: Column::Path(path),
                        ..
                    },
                )) => Some(path.clone()),
                Some(_) => None,
                None => self.path(expr)?,
            };
            match path {
                Some(path) if !keys.contains(&path) => keys.push(path),
                Some(_) => {}
                None => return self.error(self.text.of(expr), "GROUP BY takes fields"),
            }
        }
        let mut grouping = Grouping {
            keys,
            aggregates: Vec::new(),
            outputs: Vec::new(),
            hidden: 0,
        };
        for (at, item) in items.iter().enumerate() {
            let output = match &item.column {
                Column::Path(path) => match grouping.keys.iter().position(|key| key == path) {
                    Some(key) => Output::Key(key),
                    None => {
                        return self.error(
                            item.span.clone(),
                            format!(
                                "{} is neither in GROUP BY nor inside an aggregate",
                                item.label
                            ),
                        );
                    }
                },
                Column::Literal(value) => Output::Literal(value.clone()),
                Column::Aggregate(aggregate) => Output::Aggregate(grouping.aggregate(aggregate)),
            };
            grouping.outputs.push((output, format!("c{at}")));
        }
        Ok(grouping)
    }

    /// The SELECT column an expression points at by its position
    /// (`ORDER BY 2`) or its alias.
    fn selected<'i>(&self, expr: &Expr, items: &'i [Item]) -> Result<Option<(usize, &'i Item)>> {
        match expr {
            Expr::Value(value) => match &value.value {
                SqlValue::Number(digits, _) => {
                    let at = self.text.of(expr);
                    match digits.parse::<usize>() {
                        Ok(n) if (1..=items.len()).contains(&n) => Ok(Some((n - 1, &items[n - 1]))),
                        _ => self.error(
                            at,
                            format!(
                                "{digits} is not a column of the SELECT: they go from 1 to {}",
                                items.len()
                            ),
                        ),
                    }
                }
                _ => Ok(None),
            },
            Expr::Identifier(ident) => Ok(items
                .iter()
                .enumerate()
                .find(|(_, item)| item.alias.as_deref() == Some(ident.value.as_str()))),
            _ => Ok(None),
        }
    }

    /// `$sort` of a query that is not grouped: by the documents' fields.
    fn document_sort(
        &mut self,
        order_by: &[sqlparser::ast::OrderByExpr],
        items: &[Item],
    ) -> Result<Option<Document>> {
        let mut sort = Document::default();
        for order in order_by {
            let at = self.text.of(&order.expr);
            let path = match self.selected(&order.expr, items)? {
                Some((
                    _,
                    Item {
                        column: Column::Path(path),
                        ..
                    },
                )) => path.clone(),
                Some(_) => return self.error(at, "Order by a field: a value orders nothing"),
                None => match self.path(&order.expr)? {
                    Some(path) => path,
                    None => return self.error(at, "ORDER BY takes fields"),
                },
            };
            let direction = self.direction(order)?;
            let dotted = path.dotted();
            if sort.get(&dotted).is_some() {
                return self.error(at, format!("ORDER BY names {dotted} twice"));
            }
            if sort.0.is_empty() {
                self.sort_key = Some(dotted.clone());
            }
            sort.0.push((dotted, Value::Int32(direction)));
        }
        Ok((!sort.0.is_empty()).then_some(sort))
    }

    /// `$sort` of a grouped query: by the outputs of the groups.
    fn group_sort(
        &mut self,
        order_by: &[sqlparser::ast::OrderByExpr],
        items: &[Item],
        grouping: &mut Grouping,
    ) -> Result<Option<Document>> {
        let mut sort = Document::default();
        for order in order_by {
            let at = self.text.of(&order.expr);
            let name = match self.selected(&order.expr, items)? {
                Some((
                    _,
                    Item {
                        column: Column::Literal(_),
                        ..
                    },
                )) => {
                    return self.error(at, "Order by a field: a value orders nothing");
                }
                Some((n, _)) => format!("c{n}"),
                None => match self.group_output(&order.expr, grouping)? {
                    Some((name, _)) => name,
                    None => {
                        return self.error(
                            at,
                            "ORDER BY takes a field of GROUP BY, an aggregate, or a column of the SELECT",
                        );
                    }
                },
            };
            let direction = self.direction(order)?;
            if sort.get(&name).is_some() {
                return self.error(at, format!("ORDER BY names {} twice", order.expr));
            }
            sort.0.push((name, Value::Int32(direction)));
        }
        Ok((!sort.0.is_empty()).then_some(sort))
    }

    /// The output of the groups an expression names (an aggregate, or a
    /// field of GROUP BY), and whether it is known to be scalar.
    fn group_output(
        &mut self,
        expr: &Expr,
        grouping: &mut Grouping,
    ) -> Result<Option<(String, bool)>> {
        if let Some(aggregate) = self.aggregate(expr)? {
            let scalar = aggregate.scalar();
            let at = grouping.aggregate(&aggregate);
            return Ok(Some((grouping.name(Output::Aggregate(at)), scalar)));
        }
        if let Some(path) = self.path(expr)? {
            return match grouping.keys.iter().position(|key| *key == path) {
                Some(key) => Ok(Some((grouping.name(Output::Key(key)), false))),
                None => self.error(
                    self.text.of(expr),
                    format!(
                        "{} is not in GROUP BY: group by it, or name it inside an aggregate",
                        path.dotted()
                    ),
                ),
            };
        }
        Ok(None)
    }

    /// 1 for ascending, -1 for descending; a NULLS placement only where it
    /// is MongoDB's.
    fn direction(&self, order: &sqlparser::ast::OrderByExpr) -> Result<i32> {
        let at = self.text.of(&order.expr);
        if order.with_fill.is_some() {
            return self.error(at, "WITH FILL is not supported");
        }
        let descending = match order.options.sort {
            None | Some(OrderBySort::Asc) => false,
            Some(OrderBySort::Desc) => true,
            Some(_) => return self.error(at, "Order ASC or DESC"),
        };
        match (descending, order.options.nulls_first) {
            (false, Some(false)) => self.error(
                at,
                "MongoDB puts null and missing first when ascending: NULLS LAST is not supported",
            ),
            (true, Some(true)) => self.error(
                at,
                "MongoDB puts null and missing last when descending: NULLS FIRST is not supported",
            ),
            (false, _) => Ok(1),
            (true, _) => Ok(-1),
        }
    }

    /// OFFSET and LIMIT.
    fn limits(&self, query: &Query) -> Result<(Option<i64>, Option<i64>)> {
        let Some(clause) = &query.limit_clause else {
            return Ok((None, None));
        };
        match clause {
            LimitClause::LimitOffset {
                limit,
                offset,
                limit_by,
            } => {
                if let Some(by) = limit_by.first() {
                    return self.error(self.text.of(by), "LIMIT BY is not supported");
                }
                let limit = match limit {
                    Some(expr) => match self.whole(expr, "LIMIT")? {
                        0 => {
                            return self.error(
                                self.text.of(expr),
                                "LIMIT 0 returns no rows; MongoDB takes a limit from 1",
                            );
                        }
                        n => Some(n),
                    },
                    None => None,
                };
                let offset = match offset {
                    Some(offset) => Some(self.whole(&offset.value, "OFFSET")?),
                    None => None,
                };
                Ok((offset.filter(|n| *n > 0), limit))
            }
            LimitClause::OffsetCommaLimit { offset, .. } => {
                self.error(self.text.of(offset), "Write LIMIT n OFFSET m")
            }
        }
    }

    /// A whole number of rows.
    fn whole(&self, expr: &Expr, clause: &str) -> Result<i64> {
        if let Expr::Value(value) = expr
            && let SqlValue::Number(digits, _) = &value.value
            && let Ok(n) = digits.parse::<i64>()
        {
            return Ok(n);
        }
        self.error(self.text.of(expr), format!("{clause} takes a whole number"))
    }

    /// The pair of filters a predicate compiles to.
    fn predicate(&mut self, expr: &Expr, scope: &mut Scope) -> Result<Pair> {
        let at = self.text.of(expr);
        match expr {
            Expr::Nested(inner) => self.predicate(inner, scope),
            Expr::UnaryOp {
                op: UnaryOperator::Not,
                expr: inner,
            } => Ok(self.predicate(inner, scope)?.not()),
            Expr::BinaryOp {
                left,
                op: BinaryOperator::And,
                right,
            } => {
                let (left, right) = (self.predicate(left, scope)?, self.predicate(right, scope)?);
                Ok(Pair {
                    yes: Filter::and([left.yes, right.yes]),
                    no: Filter::or([left.no, right.no]),
                })
            }
            Expr::BinaryOp {
                left,
                op: BinaryOperator::Or,
                right,
            } => {
                let (left, right) = (self.predicate(left, scope)?, self.predicate(right, scope)?);
                Ok(Pair {
                    yes: Filter::or([left.yes, right.yes]),
                    no: Filter::and([left.no, right.no]),
                })
            }
            Expr::BinaryOp { left, op, right } => {
                let Some(op) = comparison(op) else {
                    return self.error(at, format!("{op} is not supported in Kuery's SQL"));
                };
                self.comparison(left, op, right, scope)
            }
            Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
                let operand = self.operand_or_refuse(inner, scope)?;
                let path = &operand.path;
                // An array is a value, so IS NULL is false for it, never
                // unknown: only the prefixes before the field guard both
                // sides. TRUE keeps the field's own guard, as `{a: null}`
                // matches `[1, null]`; FALSE takes in any array.
                let mut prefixes = operand.guards.clone();
                let no = match prefixes.last() == Some(path) {
                    true => {
                        prefixes.pop();
                        Filter::or([
                            Filter::is(path, "$type", Value::String("array".into())),
                            Filter::is(path, "$ne", Value::Null),
                        ])
                    }
                    false => Filter::is(path, "$ne", Value::Null),
                };
                let pair = Pair {
                    yes: guarded(&operand.guards, Filter::is(path, "$eq", Value::Null)),
                    no: guarded(&prefixes, no),
                };
                Ok(match expr {
                    Expr::IsNull(_) => pair,
                    _ => pair.not(),
                })
            }
            Expr::InList {
                expr: inner,
                list,
                negated,
            } => {
                let operand = self.operand_or_refuse(inner, scope)?;
                let mut values = Vec::new();
                let mut null = false;
                for item in list {
                    match self.literal(item)? {
                        Some((Value::Null, _)) => null = true,
                        Some((value, span)) => {
                            self.hint(&operand, &value, span);
                            values.push(value);
                        }
                        None => {
                            return self.error(self.text.of(item), "IN takes a list of values");
                        }
                    }
                }
                let yes = match values.is_empty() {
                    true => Filter::None,
                    false => Filter::is(&operand.path, "$in", Value::Array(values.clone())),
                };
                let no = match null {
                    true => Filter::None,
                    false => {
                        values.push(Value::Null);
                        Filter::is(&operand.path, "$nin", Value::Array(values))
                    }
                };
                let pair = scalar(&operand.guards, yes, no);
                Ok(if *negated { pair.not() } else { pair })
            }
            Expr::Between {
                expr: inner,
                negated,
                low,
                high,
            } => {
                let operand = self.operand_or_refuse(inner, scope)?;
                let low = self.bound(low, &operand)?;
                let high = self.bound(high, &operand)?;
                let path = &operand.path;
                let pair = scalar(
                    &operand.guards,
                    Filter::and([
                        Filter::is(path, "$gte", low.clone()),
                        Filter::is(path, "$lte", high.clone()),
                    ]),
                    Filter::or([Filter::is(path, "$lt", low), Filter::is(path, "$gt", high)]),
                );
                Ok(if *negated { pair.not() } else { pair })
            }
            Expr::Like {
                negated,
                any,
                expr: inner,
                pattern,
                escape_char,
            }
            | Expr::ILike {
                negated,
                any,
                expr: inner,
                pattern,
                escape_char,
            } => {
                if *any {
                    return self.error(at, "LIKE ANY is not supported");
                }
                let insensitive = matches!(expr, Expr::ILike { .. });
                let operand = self.operand_or_refuse(inner, scope)?;
                let pattern = self.like(pattern, escape_char.as_deref(), insensitive)?;
                self.like = true;
                let path = &operand.path;
                let pair = scalar(
                    &operand.guards,
                    Filter::is(path, "$regex", pattern.clone()),
                    Filter::and([
                        Filter::is(path, "$type", Value::String("string".into())),
                        Filter::is(path, "$not", pattern),
                    ]),
                );
                Ok(if *negated { pair.not() } else { pair })
            }
            Expr::Function(function)
                if matches!(function.name.0.as_slice(), [ObjectNamePart::Identifier(name)] if name.value.eq_ignore_ascii_case("missing"))
                    && !matches!(function.args, FunctionArguments::None) =>
            {
                if !matches!(scope, Scope::Documents) {
                    return self.error(at, "missing() reads the documents: use it in WHERE");
                }
                let arg = match &function.args {
                    FunctionArguments::List(list)
                        if list.duplicate_treatment.is_none() && list.clauses.is_empty() =>
                    {
                        match list.args.as_slice() {
                            [FunctionArg::Unnamed(FunctionArgExpr::Expr(arg))] => Some(arg),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                let Some(operand) = (match arg {
                    Some(arg) => self.operand(arg, scope)?,
                    None => None,
                }) else {
                    return self.error(at, "missing() takes one field: missing(coupon)");
                };
                // A field holding an array is there: only the prefixes
                // before it make the answer unknown.
                let mut guards = operand.guards.clone();
                if guards.last() == Some(&operand.path) {
                    guards.pop();
                }
                Ok(scalar(
                    &guards,
                    Filter::is(&operand.path, "$exists", Value::Bool(false)),
                    Filter::is(&operand.path, "$exists", Value::Bool(true)),
                ))
            }
            Expr::Value(value) => match &value.value {
                SqlValue::Boolean(true) => Ok(Pair {
                    yes: Filter::All,
                    no: Filter::None,
                }),
                SqlValue::Boolean(false) => Ok(Pair {
                    yes: Filter::None,
                    no: Filter::All,
                }),
                SqlValue::Null => Ok(Pair {
                    yes: Filter::None,
                    no: Filter::None,
                }),
                _ => self.error(at, "A condition compares a field with a value"),
            },
            Expr::InSubquery { .. } | Expr::Exists { .. } | Expr::Subquery(_) => {
                self.error(at, "Subqueries are not supported")
            }
            Expr::Collate { .. } => self.error(
                at,
                "COLLATE is not supported: MongoDB compares by the collection's collation",
            ),
            Expr::Function(function) if function.over.is_some() => {
                self.error(at, "Window functions are not supported")
            }
            _ => match self.operand(expr, scope)? {
                // A field alone is a boolean: true, false, or unknown.
                Some(operand) => Ok(scalar(
                    &operand.guards,
                    Filter::is(&operand.path, "$eq", Value::Bool(true)),
                    Filter::is(&operand.path, "$eq", Value::Bool(false)),
                )),
                None => match expr {
                    Expr::Function(_) => self.error(
                        at,
                        match scope {
                            Scope::Documents => "Functions in WHERE are not supported",
                            Scope::Groups { .. } => {
                                "HAVING takes aggregates and fields of GROUP BY"
                            }
                        },
                    ),
                    _ => self.error(at, format!("{expr} is not supported in Kuery's SQL")),
                },
            },
        }
    }

    /// `field op value`, or `value op field`.
    fn comparison(
        &mut self,
        left: &Expr,
        op: Comparison,
        right: &Expr,
        scope: &mut Scope,
    ) -> Result<Pair> {
        let (operand, op, value) = match (self.operand(left, scope)?, self.operand(right, scope)?) {
            (Some(_), Some(_)) => return self.two_fields(left, right),
            (Some(operand), None) => (operand, op, right),
            (None, Some(operand)) => (operand, op.flipped(), left),
            (None, None) => {
                return match (self.literal(left)?, self.literal(right)?) {
                    (Some(_), Some(_)) => self.error(
                        self.text.of(left).start..self.text.of(right).end,
                        "Compare a field with a value",
                    ),
                    _ => self.not_an_operand(
                        if self.literal(left)?.is_none() {
                            left
                        } else {
                            right
                        },
                        scope,
                    ),
                };
            }
        };
        let Some((value, span)) = self.literal(value)? else {
            return self.not_an_operand(value, scope);
        };
        if value == Value::Null {
            return self.error(
                span,
                match op {
                    Comparison::Eq => "= NULL is never true: write IS NULL",
                    Comparison::NotEq => "<> NULL is never true: write IS NOT NULL",
                    _ => "A comparison with NULL is never true: write IS NULL or IS NOT NULL",
                },
            );
        }
        self.hint(&operand, &value, span);
        let path = &operand.path;
        let (yes, no) = match op {
            Comparison::Eq | Comparison::NotEq => (
                Filter::is(path, "$eq", value.clone()),
                Filter::is(path, "$nin", Value::Array(vec![value, Value::Null])),
            ),
            Comparison::Lt => (
                Filter::is(path, "$lt", value.clone()),
                Filter::is(path, "$gte", value),
            ),
            Comparison::LtEq => (
                Filter::is(path, "$lte", value.clone()),
                Filter::is(path, "$gt", value),
            ),
            Comparison::Gt => (
                Filter::is(path, "$gt", value.clone()),
                Filter::is(path, "$lte", value),
            ),
            Comparison::GtEq => (
                Filter::is(path, "$gte", value.clone()),
                Filter::is(path, "$lt", value),
            ),
        };
        let pair = scalar(&operand.guards, yes, no);
        Ok(match op {
            Comparison::NotEq => pair.not(),
            _ => pair,
        })
    }

    /// Why two fields cannot be compared; when the second is a word in
    /// double quotes no sampled field has, that it is text meant.
    fn two_fields<T>(&self, left: &Expr, right: &Expr) -> Result<T> {
        let at = self.text.of(left).start..self.text.of(right).end;
        if let Expr::Identifier(ident) = right
            && ident.quote_style == Some('"')
        {
            let sampled =
                self.context.schema.as_ref().is_some_and(|schema| {
                    schema.fields.iter().any(|field| field.path == ident.value)
                });
            if !sampled {
                return self.error(
                    self.ident_span(ident),
                    format!(
                        "\"{0}\" is a field in SQL: for text write '{0}'",
                        ident.value
                    ),
                );
            }
        }
        self.error(at, "Comparing two fields is not supported")
    }

    /// Why an expression is neither a field nor a value.
    fn not_an_operand<T>(&self, expr: &Expr, scope: &Scope) -> Result<T> {
        let at = self.text.of(expr);
        match expr {
            Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
                self.error(at, "Subqueries are not supported")
            }
            Expr::Collate { .. } => self.error(
                at,
                "COLLATE is not supported: MongoDB compares by the collection's collation",
            ),
            Expr::Function(_) if matches!(scope, Scope::Documents) => {
                if self.aggregate(expr)?.is_some() {
                    self.error(at, "Aggregates go in HAVING, not WHERE")
                } else {
                    self.error(at, "Functions in WHERE are not supported")
                }
            }
            _ => self.error(
                at,
                match scope {
                    Scope::Documents => "A condition compares a field with a value",
                    Scope::Groups { .. } => {
                        "HAVING compares aggregates and fields of GROUP BY with values"
                    }
                },
            ),
        }
    }

    fn operand_or_refuse(&mut self, expr: &Expr, scope: &mut Scope) -> Result<Operand> {
        match self.operand(expr, scope)? {
            Some(operand) => Ok(operand),
            None => self.not_an_operand(expr, scope),
        }
    }

    /// The field a predicate tests, in its scope.
    fn operand(&mut self, expr: &Expr, scope: &mut Scope) -> Result<Option<Operand>> {
        let at = self.text.of(expr);
        match scope {
            Scope::Documents => {
                if self.aggregate(expr)?.is_some() {
                    return self.error(at, "Aggregates go in HAVING, not WHERE");
                }
                let Some(path) = self.path(expr)? else {
                    return Ok(None);
                };
                let guards: Vec<String> = path.prefixes().filter(|p| p != "_id").collect();
                self.warn_arrays(&guards, at.clone());
                Ok(Some(Operand {
                    path: path.dotted(),
                    guards,
                }))
            }
            Scope::Groups { grouping, items } => {
                if matches!(expr, Expr::Identifier(_))
                    && let Some((_, item)) = self.selected(expr, items)?
                {
                    let n = items.iter().position(|i| std::ptr::eq(i, item)).unwrap();
                    let name = format!("c{n}");
                    let scalar = match &item.column {
                        Column::Aggregate(aggregate) => aggregate.scalar(),
                        _ => false,
                    };
                    return Ok(Some(Operand {
                        guards: if scalar {
                            Vec::new()
                        } else {
                            vec![name.clone()]
                        },
                        path: name,
                    }));
                }
                Ok(self
                    .group_output(expr, grouping)?
                    .map(|(name, scalar)| Operand {
                        guards: if scalar {
                            Vec::new()
                        } else {
                            vec![name.clone()]
                        },
                        path: name,
                    }))
            }
        }
    }

    /// A bound of BETWEEN.
    fn bound(&mut self, expr: &Expr, operand: &Operand) -> Result<Value> {
        match self.literal(expr)? {
            Some((Value::Null, span)) => self.error(span, "BETWEEN with NULL is never true"),
            Some((value, span)) => {
                self.hint(operand, &value, span);
                Ok(value)
            }
            None => self.error(self.text.of(expr), "BETWEEN takes values"),
        }
    }

    /// A LIKE pattern as the regular expression that matches the same
    /// whole strings.
    fn like(&self, pattern: &Expr, escape: Option<&Expr>, insensitive: bool) -> Result<Value> {
        let at = self.text.of(pattern);
        let Some((Value::String(pattern), _)) = self.literal(pattern)? else {
            return self.error(at, "LIKE takes a pattern in quotes");
        };
        let escape = match escape {
            None => None,
            Some(expr) => match self.literal(expr)? {
                Some((Value::String(escape), _)) if escape.chars().count() == 1 => {
                    escape.chars().next()
                }
                _ => {
                    return self.error(self.text.of(expr), "ESCAPE takes one character in quotes");
                }
            },
        };
        let mut regex = String::from("\\A");
        let mut chars = pattern.chars();
        while let Some(c) = chars.next() {
            match c {
                c if Some(c) == escape => match chars.next() {
                    Some(next) => push_escaped(&mut regex, next),
                    None => {
                        return self.error(at, "The pattern ends with its escape character");
                    }
                },
                '%' => regex.push_str(".*"),
                '_' => regex.push('.'),
                c => push_escaped(&mut regex, c),
            }
        }
        regex.push_str("\\z");
        let flags = if insensitive { "is" } else { "s" };
        literal::regex(&regex, flags).or_else(|why| self.error(at, why))
    }

    /// Warns of the arrays a sample found where a predicate needs scalars.
    fn warn_arrays(&mut self, guards: &[String], span: Range<usize>) {
        let Some(schema) = &self.context.schema else {
            return;
        };
        for path in guards {
            if self.warned.contains(path) {
                continue;
            }
            let Some(field) = sampled(&schema.fields, path) else {
                continue;
            };
            let arrays = field
                .types
                .iter()
                .find(|(kind, _)| *kind == "Array")
                .map_or(0, |(_, n)| *n);
            if arrays == 0 || schema.documents == 0 {
                continue;
            }
            let share = (arrays * 100).div_ceil(schema.documents).min(100);
            self.warned.push(path.clone());
            self.warnings.push(Warning {
                span: span.clone(),
                message: format!(
                    "{path} is an array in {share}% of the sample: where it is one, this condition is neither true nor false"
                ),
            });
        }
    }

    /// Warns of a value the sample says is likely another type: a 24-digit
    /// hexadecimal string for a field that holds ObjectIds, a number with a
    /// point for one that holds Decimal128.
    fn hint(&mut self, operand: &Operand, value: &Value, span: Range<usize>) {
        let Some(schema) = &self.context.schema else {
            return;
        };
        if let Value::Double(_) = value {
            // `9.99` is a Double, which no Decimal128 9.99 equals.
            let decimals = sampled(&schema.fields, &operand.path)
                .is_some_and(|field| field.types.iter().any(|(kind, _)| *kind == "Decimal128"));
            if decimals {
                let written: String = self.text.text[span.clone()].split_whitespace().collect();
                self.warnings.push(Warning {
                    span,
                    message: format!(
                        "{} holds Decimal128 in the sample, and {written} is a Double, which may not equal it: did you mean DECIMAL '{written}'?",
                        operand.path
                    ),
                });
            }
            return;
        }
        let Value::String(text) = value else {
            return;
        };
        if text.len() != 24 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
            return;
        }
        let object_ids = sampled(&schema.fields, &operand.path)
            .and_then(|field| field.types.first())
            .is_some_and(|(kind, _)| *kind == "ObjectId");
        if object_ids {
            self.warnings.push(Warning {
                span,
                message: format!(
                    "{} looks like an ObjectId: did you mean ObjectId('{text}')?",
                    operand.path
                ),
            });
        }
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        let collation =
            self.context
                .collection
                .as_ref()
                .and_then(|info| match info.options.get("collation") {
                    Some(Value::Object(collation)) => collation.get("locale").cloned(),
                    _ => None,
                });
        if let Some(locale) = collation {
            let locale = match locale {
                Value::String(locale) => locale,
                other => literal::render(&other, Style::Compact),
            };
            notes.push(match self.like {
                true => format!(
                    "= and ORDER BY follow the collection's collation ({locale}); LIKE does not"
                ),
                false => format!("Compares and orders by the collection's collation ({locale})"),
            });
        }
        notes
    }
}

/// A predicate on a field that must not be (or pass through) an array:
/// both sides carry the guards.
fn scalar(guards: &[String], yes: Filter, no: Filter) -> Pair {
    Pair {
        yes: guarded(guards, yes),
        no: guarded(guards, no),
    }
}

/// `filter`, where none of `guards` holds an array.
fn guarded(guards: &[String], filter: Filter) -> Filter {
    let guards = guards.iter().map(|path| {
        Filter::is(
            path,
            "$not",
            Value::Object(doc([("$type", Value::String("array".into()))])),
        )
    });
    Filter::and(guards.chain([filter]))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comparison {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

impl Comparison {
    /// The same comparison with its sides swapped: `5 < a` is `a > 5`.
    fn flipped(self) -> Comparison {
        match self {
            Comparison::Lt => Comparison::Gt,
            Comparison::LtEq => Comparison::GtEq,
            Comparison::Gt => Comparison::Lt,
            Comparison::GtEq => Comparison::LtEq,
            other => other,
        }
    }
}

fn comparison(op: &BinaryOperator) -> Option<Comparison> {
    Some(match op {
        BinaryOperator::Eq => Comparison::Eq,
        BinaryOperator::NotEq => Comparison::NotEq,
        BinaryOperator::Lt => Comparison::Lt,
        BinaryOperator::LtEq => Comparison::LtEq,
        BinaryOperator::Gt => Comparison::Gt,
        BinaryOperator::GtEq => Comparison::GtEq,
        _ => return None,
    })
}

/// A character of a LIKE pattern as itself in a regular expression.
fn push_escaped(regex: &mut String, c: char) {
    if "\\^$.|?*+()[]{}/".contains(c) {
        regex.push('\\');
    }
    regex.push(c);
}

/// The path of a sample's field named as a filter names it, where no
/// array lies on its way.
fn sampled<'s>(fields: &'s [FieldSchema], path: &str) -> Option<&'s FieldSchema> {
    fields
        .iter()
        .find(|field| field.path == path && !field.parts.contains(&Part::Elements))
}

/// What an index has to serve for a query not to read the whole
/// collection.
#[derive(Debug, Clone, PartialEq)]
enum Need {
    /// Nothing an index narrows: a negation (`NOT`, `<>`, `NOT IN`), a
    /// LIKE without a fixed start, or no filter at all.
    Nothing,
    /// A field an index's first key serves.
    Field(String),
    /// Any one of them: the sides of an AND, or the filter and the order.
    Any(Vec<Need>),
    /// Every one of them: the branches of an OR, each read on its own.
    Every(Vec<Need>),
}

impl Need {
    fn met(&self, indexed: &impl Fn(&str) -> bool) -> bool {
        match self {
            Need::Nothing => false,
            Need::Field(path) => indexed(path),
            Need::Any(needs) => needs.iter().any(|need| need.met(indexed)),
            Need::Every(needs) => !needs.is_empty() && needs.iter().all(|need| need.met(indexed)),
        }
    }
}

impl Lowering<'_> {
    /// What WHERE needs an index for, read from the checked predicate.
    fn need(&self, expr: &Expr) -> Need {
        let field = |expr: &Expr| match self.path(expr) {
            Ok(Some(path)) => Need::Field(path.dotted()),
            _ => Need::Nothing,
        };
        match expr {
            Expr::Nested(inner) => self.need(inner),
            Expr::BinaryOp {
                left,
                op: BinaryOperator::And,
                right,
            } => Need::Any(vec![self.need(left), self.need(right)]),
            Expr::BinaryOp {
                left,
                op: BinaryOperator::Or,
                right,
            } => Need::Every(vec![self.need(left), self.need(right)]),
            Expr::BinaryOp { left, op, right } => match comparison(op) {
                None | Some(Comparison::NotEq) => Need::Nothing,
                Some(_) => match field(left) {
                    Need::Nothing => field(right),
                    found => found,
                },
            },
            Expr::IsNull(inner) => field(inner),
            Expr::InList {
                expr: inner,
                negated: false,
                ..
            }
            | Expr::Between {
                expr: inner,
                negated: false,
                ..
            } => field(inner),
            Expr::Like {
                negated: false,
                any: false,
                expr: inner,
                pattern,
                escape_char,
            } if self.fixed_start(pattern, escape_char.as_deref()) => field(inner),
            Expr::Function(function) if matches!(function.name.0.as_slice(), [ObjectNamePart::Identifier(name)] if name.value.eq_ignore_ascii_case("missing")) => {
                match &function.args {
                    FunctionArguments::List(list) => match list.args.as_slice() {
                        [FunctionArg::Unnamed(FunctionArgExpr::Expr(arg))] => field(arg),
                        _ => Need::Nothing,
                    },
                    _ => Need::Nothing,
                }
            }
            // A field alone, `= true`.
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => field(expr),
            _ => Need::Nothing,
        }
    }

    /// Whether a LIKE pattern starts with a character of its own, which
    /// MongoDB reads off an index as a prefix (ILIKE's never is).
    fn fixed_start(&self, pattern: &Expr, escape: Option<&Expr>) -> bool {
        let Ok(Some((Value::String(pattern), _))) = self.literal(pattern) else {
            return false;
        };
        let escape = match escape.map(|escape| self.literal(escape)) {
            None => None,
            Some(Ok(Some((Value::String(escape), _)))) => escape.chars().next(),
            Some(_) => return false,
        };
        let mut chars = pattern.chars();
        match chars.next() {
            Some(c) if Some(c) == escape => chars.next().is_some(),
            Some('%' | '_') | None => false,
            Some(_) => true,
        }
    }
}

/// Whether an index serves `path`: its first key, on an index the planner
/// can use for any document.
fn indexed(info: &CollectionInfo, path: &str) -> bool {
    info.indexes.iter().any(|index| {
        !index.hidden
            && !index.sparse
            && index.partial.is_none()
            && index
                .keys
                .0
                .first()
                .is_some_and(|(field, kind)| field == path && !matches!(kind, Value::String(_)))
    })
}

fn stage(name: &str, value: Value) -> Value {
    Value::Object(doc([(name, value)]))
}

fn literal_expression(value: Value) -> Value {
    Value::Object(doc([("$literal", value)]))
}

fn plain_wildcard(options: &sqlparser::ast::WildcardAdditionalOptions) -> bool {
    options.opt_ilike.is_none()
        && options.opt_exclude.is_none()
        && options.opt_except.is_none()
        && options.opt_replace.is_none()
        && options.opt_rename.is_none()
        && options.opt_alias.is_none()
}

/// Whether MongoDB takes `name` for a database.
fn database_name(name: &str) -> bool {
    !name.is_empty() && name.len() < 64 && !name.contains(['/', '\\', '.', ' ', '"', '$', '\0'])
}

fn push_sort_and_limits(
    stages: &mut Vec<Value>,
    sort: Option<Document>,
    skip: Option<i64>,
    limit: Option<i64>,
) {
    if let Some(sort) = sort {
        stages.push(stage("$sort", Value::Object(sort)));
    }
    if let Some(skip) = skip {
        stages.push(stage("$skip", number(skip)));
    }
    if let Some(limit) = limit {
        stages.push(stage("$limit", number(limit)));
    }
}

fn number(n: i64) -> Value {
    match i32::try_from(n) {
        Ok(n) => Value::Int32(n),
        Err(_) => Value::Int64(n),
    }
}

/// `$group` and the `$project` that names its outputs; inside a `$facet`
/// when there is no GROUP BY, so no documents still make one row.
fn group_stages(grouping: &Grouping) -> Vec<Value> {
    let field = |path: &Path| Value::String(format!("${}", path.dotted()));
    let id = match grouping.keys.is_empty() {
        true => Value::Null,
        false => Value::Object(Document(
            grouping
                .keys
                .iter()
                .enumerate()
                .map(|(at, key)| {
                    (
                        format!("k{at}"),
                        Value::Object(doc([(
                            "$ifNull",
                            Value::Array(vec![field(key), Value::Null]),
                        )])),
                    )
                })
                .collect(),
        )),
    };
    let mut group = vec![("_id".to_string(), id)];
    for (at, aggregate) in grouping.aggregates.iter().enumerate() {
        let one = |operator: &str, value: Value| Value::Object(doc([(operator, value)]));
        let accumulator = match aggregate {
            Aggregate::CountAll => one("$sum", Value::Int32(1)),
            Aggregate::Count(path) => one(
                "$sum",
                one(
                    "$cond",
                    Value::Array(vec![
                        one(
                            "$in",
                            Value::Array(vec![
                                one("$type", field(path)),
                                Value::Array(vec![
                                    Value::String("missing".into()),
                                    Value::String("null".into()),
                                ]),
                            ]),
                        ),
                        Value::Int32(0),
                        Value::Int32(1),
                    ]),
                ),
            ),
            Aggregate::Sum(path) => one("$sum", field(path)),
            Aggregate::Avg(path) => one("$avg", field(path)),
            Aggregate::Min(path) => one("$min", field(path)),
            Aggregate::Max(path) => one("$max", field(path)),
        };
        group.push((format!("a{at}"), accumulator));
        if let Aggregate::Sum(path) = aggregate {
            group.push((
                format!("n{at}"),
                one(
                    "$sum",
                    one(
                        "$cond",
                        Value::Array(vec![
                            one("$isNumber", field(path)),
                            Value::Int32(1),
                            Value::Int32(0),
                        ]),
                    ),
                ),
            ));
        }
    }
    let global = grouping.keys.is_empty();
    // An output of `$group`: the field itself, or, from the `$facet`'s one
    // document, the field of its only group (missing when there is none).
    let get = |name: String| match global {
        false => Value::String(format!("${name}")),
        true => Value::Object(doc([("$first", Value::String(format!("$g.{name}")))])),
    };
    let if_null = |value: Value, default: Value| {
        Value::Object(doc([("$ifNull", Value::Array(vec![value, default]))]))
    };
    let mut project = vec![("_id".to_string(), Value::Int32(0))];
    for (output, name) in &grouping.outputs {
        let value = match output {
            Output::Literal(value) => literal_expression(value.clone()),
            Output::Key(at) => Value::String(format!("$_id.k{at}")),
            Output::Aggregate(at) => match &grouping.aggregates[*at] {
                Aggregate::CountAll | Aggregate::Count(_) => match global {
                    false => get(format!("a{at}")),
                    true => if_null(get(format!("a{at}")), Value::Int32(0)),
                },
                Aggregate::Sum(_) => Value::Object(doc([(
                    "$cond",
                    Value::Array(vec![
                        Value::Object(doc([(
                            "$gt",
                            Value::Array(vec![get(format!("n{at}")), Value::Int32(0)]),
                        )])),
                        get(format!("a{at}")),
                        Value::Null,
                    ]),
                )])),
                Aggregate::Avg(_) | Aggregate::Min(_) | Aggregate::Max(_) => match global {
                    false => get(format!("a{at}")),
                    true => if_null(get(format!("a{at}")), Value::Null),
                },
            },
        };
        project.push((name.clone(), value));
    }
    let group = stage("$group", Value::Object(Document(group)));
    let first = match global {
        false => group,
        true => stage(
            "$facet",
            Value::Object(doc([("g", Value::Array(vec![group]))])),
        ),
    };
    vec![first, stage("$project", Value::Object(Document(project)))]
}

/// `estimatedDocumentCount`, for a count of the whole collection.
fn estimated(target: &CollectionRef, label: &str) -> Translation {
    let op = Op::Collection {
        target: target.clone(),
        method: Method::EstimatedDocumentCount,
        args: Vec::new(),
        chain: Vec::new(),
    };
    Translation {
        native: shell::print(&op),
        target: target.clone(),
        columns: vec![OutputColumn {
            field: "count".into(),
            label: label.to_string(),
        }],
        warnings: Vec::new(),
        notes: Vec::new(),
        full_scan: false,
        estimate: None,
    }
}
