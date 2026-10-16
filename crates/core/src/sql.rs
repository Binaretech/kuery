//! Safe SQL construction: identifiers and literals are always escaped.
//!
//! All SQL the app generates (browsing a table, editing a row, reading the
//! catalog) goes through here; user text is never concatenated unescaped.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlError {
    /// PostgreSQL does not accept the NUL character in text.
    NulByte,
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlError::NulByte => write!(
                f,
                "The value contains a NUL character, which PostgreSQL does not accept"
            ),
        }
    }
}

impl std::error::Error for SqlError {}

/// The SQL a database speaks, where it differs in what Kuery generates:
/// quoting, casting to text, case-insensitive matching, sorting NULLs,
/// comparing with NULLs, and which parser reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Dialect {
    #[default]
    Postgres,
    Sqlite,
    Mysql,
}

impl Dialect {
    /// `"orders"`, or `` `orders` `` in MySQL.
    pub fn quote_ident(self, name: &str) -> String {
        match self {
            Dialect::Mysql => format!("`{}`", name.replace('`', "``")),
            Dialect::Postgres | Dialect::Sqlite => quote_ident(name),
        }
    }

    /// `"public"."orders"`: the schema is SQLite's attached database
    /// (`main`) and MySQL's database.
    pub fn quote_qualified(self, schema: &str, name: &str) -> String {
        format!("{}.{}", self.quote_ident(schema), self.quote_ident(name))
    }

    /// A text literal. MySQL reads backslashes as escapes unless
    /// `NO_BACKSLASH_ESCAPES` is set, so text with backslashes goes as hex,
    /// which means the same either way.
    pub fn quote_literal(self, value: &str) -> Result<String, SqlError> {
        match self {
            Dialect::Postgres => quote_literal(value),
            Dialect::Sqlite => {
                if value.contains('\0') {
                    return Err(SqlError::NulByte);
                }
                Ok(format!("'{}'", value.replace('\'', "''")))
            }
            Dialect::Mysql => {
                if value.contains('\\') || value.contains('\0') {
                    let hex: String = value.bytes().map(|b| format!("{b:02x}")).collect();
                    Ok(format!("_utf8mb4 x'{hex}'"))
                } else {
                    Ok(format!("'{}'", value.replace('\'', "''")))
                }
            }
        }
    }

    pub fn quote_nullable(self, value: Option<&str>) -> Result<String, SqlError> {
        match value {
            Some(value) => self.quote_literal(value),
            None => Ok("NULL".into()),
        }
    }

    /// `expr` as text, the way its values are shown.
    pub fn as_text(self, expr: &str) -> String {
        match self {
            Dialect::Postgres => format!("{expr}::text"),
            Dialect::Sqlite => format!("cast({expr} as text)"),
            Dialect::Mysql => format!("cast({expr} as char)"),
        }
    }

    /// The case-insensitive `LIKE`: SQLite's and MySQL's (with their usual
    /// collations) already are.
    pub fn like_ignoring_case(self) -> &'static str {
        match self {
            Dialect::Postgres => "ilike",
            Dialect::Sqlite | Dialect::Mysql => "like",
        }
    }

    /// Sorts by `expr` with NULLs last either way.
    pub fn order_nulls_last(self, expr: &str, descending: bool) -> String {
        let direction = if descending { "desc" } else { "asc" };
        match self {
            Dialect::Postgres | Dialect::Sqlite => format!("{expr} {direction} nulls last"),
            Dialect::Mysql => format!("{expr} is null, {expr} {direction}"),
        }
    }

    /// `a` equals `b`, NULL equal to NULL.
    pub fn same(self, a: &str, b: &str) -> String {
        match self {
            Dialect::Postgres => format!("{a} is not distinct from {b}"),
            Dialect::Sqlite => format!("{a} is {b}"),
            Dialect::Mysql => format!("{a} <=> {b}"),
        }
    }

    /// A row with every column at its default.
    pub fn insert_defaults(self, table: &str) -> String {
        match self {
            Dialect::Postgres | Dialect::Sqlite => format!("insert into {table} default values"),
            Dialect::Mysql => format!("insert into {table} () values ()"),
        }
    }

    /// The parser for analysing and splitting its SQL.
    pub fn parser(self) -> Box<dyn sqlparser::dialect::Dialect> {
        match self {
            Dialect::Postgres => Box::new(sqlparser::dialect::PostgreSqlDialect {}),
            Dialect::Sqlite => Box::new(sqlparser::dialect::SQLiteDialect {}),
            Dialect::Mysql => Box::new(sqlparser::dialect::MySqlDialect {}),
        }
    }
}

/// `orders` → `"orders"`, `a"b` → `"a""b"`. Always quoted, so uppercase
/// letters and reserved words are preserved.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `"public"."orders"`.
pub fn quote_qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

/// Text literal. Uses `E'…'` when there are backslashes so the result is the
/// same under any value of `standard_conforming_strings`.
pub fn quote_literal(value: &str) -> Result<String, SqlError> {
    if value.contains('\0') {
        return Err(SqlError::NulByte);
    }
    let escaped = value.replace('\'', "''");
    Ok(if value.contains('\\') {
        format!("E'{}'", escaped.replace('\\', "\\\\"))
    } else {
        format!("'{escaped}'")
    })
}

/// Literal or `NULL`.
pub fn quote_nullable(value: Option<&str>) -> Result<String, SqlError> {
    match value {
        Some(value) => quote_literal(value),
        None => Ok("NULL".into()),
    }
}

/// Escapes `%`, `_` and `\` so `value` can be used inside a LIKE pattern with
/// `ESCAPE '\'`.
pub fn escape_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_identifiers() {
        assert_eq!(quote_ident("orders"), "\"orders\"");
        assert_eq!(quote_ident("Order Items"), "\"Order Items\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_qualified("public", "orders"), "\"public\".\"orders\"");
    }

    #[test]
    fn quotes_literals() {
        assert_eq!(quote_literal("paid").unwrap(), "'paid'");
        assert_eq!(quote_literal("O'Brien").unwrap(), "'O''Brien'");
        assert_eq!(
            quote_literal("'; drop table orders; --").unwrap(),
            "'''; drop table orders; --'"
        );
        assert_eq!(quote_literal(r"C:\tmp").unwrap(), r"E'C:\\tmp'");
        assert_eq!(quote_literal(r"\'").unwrap(), r"E'\\'''");
        assert_eq!(quote_literal("a\0b"), Err(SqlError::NulByte));
        assert_eq!(quote_nullable(None).unwrap(), "NULL");
    }

    #[test]
    fn speaks_each_dialect() {
        let (pg, lite, my) = (Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql);
        assert_eq!(my.quote_qualified("shop", "a`b"), "`shop`.`a``b`");
        assert_eq!(
            lite.quote_qualified("main", "orders"),
            "\"main\".\"orders\""
        );
        assert_eq!(
            lite.quote_literal(r"O'Brien \ C:\tmp").unwrap(),
            r"'O''Brien \ C:\tmp'"
        );
        assert_eq!(my.quote_literal("O'Brien").unwrap(), "'O''Brien'");
        assert_eq!(my.quote_literal(r"a\b").unwrap(), "_utf8mb4 x'615c62'");
        assert_eq!(pg.quote_literal(r"a\b").unwrap(), r"E'a\\b'");
        assert_eq!(lite.as_text("\"n\""), "cast(\"n\" as text)");
        assert_eq!(my.order_nulls_last("`n`", true), "`n` is null, `n` desc");
        assert_eq!(lite.same("a", "b"), "a is b");
        assert_eq!(my.same("a", "b"), "a <=> b");
        assert_eq!(my.insert_defaults("`t`"), "insert into `t` () values ()");
    }

    #[test]
    fn escapes_like_patterns() {
        assert_eq!(escape_like("50%_off\\"), "50\\%\\_off\\\\");
    }
}
