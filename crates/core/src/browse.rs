//! Browsing a table's data: server-side filtering, sorting and pagination.
//!
//! The table is never fetched in full: each page is a `SELECT … LIMIT … OFFSET`
//! generated here, with escaped identifiers and values.

use std::fmt;

use crate::catalog::TableRef;
use crate::sql::{Dialect, SqlError, escape_like};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Equals,
    NotEquals,
    Greater,
    GreaterOrEqual,
    Less,
    LessOrEqual,
    Contains,
    StartsWith,
    OneOf,
    IsNull,
    IsNotNull,
}

impl FilterOp {
    pub const ALL: [FilterOp; 11] = [
        FilterOp::Equals,
        FilterOp::NotEquals,
        FilterOp::Greater,
        FilterOp::GreaterOrEqual,
        FilterOp::Less,
        FilterOp::LessOrEqual,
        FilterOp::Contains,
        FilterOp::StartsWith,
        FilterOp::OneOf,
        FilterOp::IsNull,
        FilterOp::IsNotNull,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FilterOp::Equals => "equals",
            FilterOp::NotEquals => "not equal to",
            FilterOp::Greater => "greater than",
            FilterOp::GreaterOrEqual => "greater than or equal to",
            FilterOp::Less => "less than",
            FilterOp::LessOrEqual => "less than or equal to",
            FilterOp::Contains => "contains",
            FilterOp::StartsWith => "starts with",
            FilterOp::OneOf => "is one of",
            FilterOp::IsNull => "is NULL",
            FilterOp::IsNotNull => "is not NULL",
        }
    }

    pub fn needs_value(self) -> bool {
        !matches!(self, FilterOp::IsNull | FilterOp::IsNotNull)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub column: String,
    pub op: FilterOp,
    /// For [`FilterOp::OneOf`], comma-separated values.
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Ascending,
    Descending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sort {
    pub column: String,
    pub direction: Direction,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Browse {
    pub table: TableRef,
    /// The table's columns, in order; used to validate filters and sorting.
    pub columns: Vec<String>,
    pub filters: Vec<Filter>,
    pub sort: Vec<Sort>,
    pub page_size: u64,
    pub page: u64,
    pub dialect: &'static Dialect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowseError {
    UnknownColumn(String),
    MissingValue(String),
    Sql(SqlError),
}

impl fmt::Display for BrowseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BrowseError::UnknownColumn(name) => {
                write!(f, "Column \"{name}\" does not exist in the table")
            }
            BrowseError::MissingValue(name) => write!(f, "The filter on \"{name}\" needs a value"),
            BrowseError::Sql(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for BrowseError {}

impl From<SqlError> for BrowseError {
    fn from(error: SqlError) -> Self {
        BrowseError::Sql(error)
    }
}

impl Browse {
    pub fn new(table: TableRef, columns: Vec<String>) -> Self {
        Self {
            table,
            columns,
            filters: Vec::new(),
            sort: Vec::new(),
            page_size: 100,
            page: 0,
            dialect: &crate::sql::POSTGRES,
        }
    }

    /// The same, in another database's SQL.
    pub fn in_dialect(mut self, dialect: &'static Dialect) -> Self {
        self.dialect = dialect;
        self
    }

    /// `SELECT` for the current page.
    ///
    /// Without an explicit sort it orders by `default_order` (the primary key)
    /// so pages are stable.
    pub fn page_sql(&self, default_order: &[String]) -> Result<String, BrowseError> {
        let mut sql = format!(
            "select * from {}{}",
            self.dialect
                .quote_qualified(&self.table.schema, &self.table.name),
            self.where_clause()?
        );

        let order: Vec<String> = if self.sort.is_empty() {
            default_order
                .iter()
                .map(|c| self.column(c))
                .collect::<Result<_, _>>()?
        } else {
            self.sort
                .iter()
                .map(|sort| {
                    let column = self.column(&sort.column)?;
                    Ok(self
                        .dialect
                        .order_nulls_last(&column, sort.direction == Direction::Descending))
                })
                .collect::<Result<_, BrowseError>>()?
        };
        if !order.is_empty() {
            sql.push_str(" order by ");
            sql.push_str(&order.join(", "));
        }

        let page_size = self.page_size.max(1);
        sql.push_str(
            &self
                .dialect
                .page(!order.is_empty(), page_size, self.page * page_size),
        );
        Ok(sql)
    }

    /// `SELECT count(*)` with the same filters.
    pub fn count_sql(&self) -> Result<String, BrowseError> {
        Ok(format!(
            "select count(*) from {}{}",
            self.dialect
                .quote_qualified(&self.table.schema, &self.table.name),
            self.where_clause()?
        ))
    }

    fn where_clause(&self) -> Result<String, BrowseError> {
        if self.filters.is_empty() {
            return Ok(String::new());
        }
        let conditions = self
            .filters
            .iter()
            .map(|filter| self.condition(filter))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(format!(" where {}", conditions.join(" and ")))
    }

    fn condition(&self, filter: &Filter) -> Result<String, BrowseError> {
        let column = self.column(&filter.column)?;
        let value = filter.value.trim();
        if filter.op.needs_value() && value.is_empty() {
            return Err(BrowseError::MissingValue(filter.column.clone()));
        }
        // Untyped literals are cast to the column's type, so `status = 'paid'`
        // works with enums, dates or uuids.
        let dialect = self.dialect;
        let comparison = |op: &str| -> Result<String, BrowseError> {
            Ok(format!("{column} {op} {}", dialect.quote_literal(value)?))
        };
        let pattern = |pattern: String| -> Result<String, BrowseError> {
            Ok(format!(
                "{} {} {} escape {}",
                dialect.as_text(&column),
                dialect.like_ignoring_case(),
                dialect.quote_literal(&pattern)?,
                dialect.quote_literal("\\")?
            ))
        };
        match filter.op {
            FilterOp::Equals => comparison("="),
            FilterOp::NotEquals => comparison("<>"),
            FilterOp::Greater => comparison(">"),
            FilterOp::GreaterOrEqual => comparison(">="),
            FilterOp::Less => comparison("<"),
            FilterOp::LessOrEqual => comparison("<="),
            FilterOp::Contains => pattern(format!("%{}%", escape_like(value))),
            FilterOp::StartsWith => pattern(format!("{}%", escape_like(value))),
            FilterOp::OneOf => {
                let values = value
                    .split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(|value| dialect.quote_literal(value))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!("{column} in ({})", values.join(", ")))
            }
            FilterOp::IsNull => Ok(format!("{column} is null")),
            FilterOp::IsNotNull => Ok(format!("{column} is not null")),
        }
    }

    fn column(&self, name: &str) -> Result<String, BrowseError> {
        if self.columns.iter().any(|c| c == name) {
            Ok(self.dialect.quote_ident(name))
        } else {
            Err(BrowseError::UnknownColumn(name.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_only;

    fn orders() -> Browse {
        Browse::new(
            TableRef::new("public", "orders"),
            vec!["id".into(), "status".into(), "total".into(), "note".into()],
        )
    }

    fn filter(column: &str, op: FilterOp, value: &str) -> Filter {
        Filter {
            column: column.into(),
            op,
            value: value.into(),
        }
    }

    #[test]
    fn pages_in_primary_key_order_by_default() {
        let mut browse = orders();
        assert_eq!(
            browse.page_sql(&["id".into()]).unwrap(),
            r#"select * from "public"."orders" order by "id" limit 100 offset 0"#
        );
        browse.page = 3;
        browse.page_size = 50;
        assert_eq!(
            browse.page_sql(&[]).unwrap(),
            r#"select * from "public"."orders" limit 50 offset 150"#
        );
    }

    #[test]
    fn browses_in_each_dialect() {
        let mut browse = Browse::new(
            TableRef::new("shop", "orders"),
            vec!["id".into(), "status".into()],
        )
        .in_dialect(&crate::sql::MYSQL);
        browse.filters = vec![filter("status", FilterOp::Contains, "pa")];
        browse.sort = vec![Sort {
            column: "id".into(),
            direction: Direction::Descending,
        }];
        assert_eq!(
            browse.page_sql(&[]).unwrap(),
            "select * from `shop`.`orders` where cast(`status` as char) like '%pa%' escape _utf8mb4 x'5c' \
             order by `id` is null, `id` desc limit 100 offset 0"
        );
        browse.dialect = &crate::sql::SQLITE;
        assert_eq!(
            browse.count_sql().unwrap(),
            "select count(*) from \"shop\".\"orders\" where cast(\"status\" as text) like '%pa%' escape '\\'"
        );
    }

    #[test]
    fn pages_with_offset_fetch_in_t_sql() {
        let mut browse = Browse::new(
            TableRef::new("dbo", "orders"),
            vec!["id".into(), "total".into()],
        )
        .in_dialect(&crate::sql::MSSQL);
        assert_eq!(
            browse.page_sql(&[]).unwrap(),
            "select * from [dbo].[orders] order by (select null) offset 0 rows fetch next 100 rows only"
        );
        browse.sort = vec![Sort {
            column: "total".into(),
            direction: Direction::Descending,
        }];
        assert_eq!(
            browse.page_sql(&[]).unwrap(),
            "select * from [dbo].[orders] order by case when [total] is null then 1 else 0 end, [total] desc offset 0 rows fetch next 100 rows only"
        );
    }

    #[test]
    fn filters_and_sorts() {
        let mut browse = orders();
        browse.filters = vec![
            filter("status", FilterOp::OneOf, "paid, refunded"),
            filter("total", FilterOp::GreaterOrEqual, "100"),
            filter("note", FilterOp::IsNull, ""),
        ];
        browse.sort = vec![Sort {
            column: "total".into(),
            direction: Direction::Descending,
        }];
        assert_eq!(
            browse.page_sql(&["id".into()]).unwrap(),
            r#"select * from "public"."orders" where "status" in ('paid', 'refunded') and "total" >= '100' and "note" is null order by "total" desc nulls last limit 100 offset 0"#
        );
        assert_eq!(
            browse.count_sql().unwrap(),
            r#"select count(*) from "public"."orders" where "status" in ('paid', 'refunded') and "total" >= '100' and "note" is null"#
        );
    }

    #[test]
    fn escapes_values_and_like_patterns() {
        let mut browse = orders();
        browse.filters = vec![filter("note", FilterOp::Contains, "50%' or 1=1 --")];
        assert_eq!(
            browse.count_sql().unwrap(),
            r#"select count(*) from "public"."orders" where "note"::text ilike E'%50\\%'' or 1=1 --%' escape E'\\'"#
        );
    }

    #[test]
    fn generated_sql_passes_the_read_only_check() {
        let mut browse = orders();
        browse.filters = vec![
            filter("note", FilterOp::StartsWith, "a_b"),
            filter("status", FilterOp::Equals, "x'; delete from orders; --"),
        ];
        read_only::check(&browse.page_sql(&["id".into()]).unwrap()).unwrap();
        read_only::check(&browse.count_sql().unwrap()).unwrap();
    }

    #[test]
    fn rejects_unknown_columns_and_missing_values() {
        let mut browse = orders();
        browse.filters = vec![filter("\"; drop table x; --", FilterOp::Equals, "1")];
        assert!(matches!(
            browse.count_sql(),
            Err(BrowseError::UnknownColumn(_))
        ));

        browse.filters = vec![filter("status", FilterOp::Equals, "  ")];
        assert_eq!(
            browse.count_sql(),
            Err(BrowseError::MissingValue("status".into()))
        );

        browse.filters.clear();
        browse.sort = vec![Sort {
            column: "missing".into(),
            direction: Direction::Ascending,
        }];
        assert!(matches!(
            browse.page_sql(&[]),
            Err(BrowseError::UnknownColumn(_))
        ));
    }
}
