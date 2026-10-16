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

/// The SQL a database speaks, as data: each driver hands one to Kuery, and
/// everything Kuery writes or reads itself follows it (quoting, casting to
/// text, matching, sorting NULLs, comparing with NULL, transactions, which
/// parser reads its SQL). [`POSTGRES`], [`SQLITE`] and [`MYSQL`] are the
/// ones the drivers here use; a new driver can bring its own.
pub struct Dialect {
    /// For messages, such as `PostgreSQL`.
    pub name: &'static str,
    /// What quotes identifiers (`"` or `` ` ``); doubled inside one.
    pub ident_quote: char,
    /// A text literal.
    pub literal: fn(&str) -> Result<String, SqlError>,
    /// An expression as text, the way its values are shown.
    pub as_text: fn(&str) -> String,
    /// The case-insensitive `LIKE`.
    pub like_ignoring_case: &'static str,
    /// Whether `NULLS LAST` exists; without it, sorting puts `x is null` first.
    pub nulls_last: bool,
    /// `a` equals `b`, NULL equal to NULL.
    pub same: fn(&str, &str) -> String,
    /// A row with every column at its default; `{}` is the table.
    pub insert_defaults: &'static str,
    /// The parser for analysing and splitting its SQL.
    pub parser: fn() -> Box<dyn sqlparser::dialect::Dialect>,
    /// How the formatter reads it.
    pub formatter: sqlformat::Dialect,
    pub transactions: Transactions,
    /// Schema changes (`CREATE`, `ALTER`, `DROP`, `TRUNCATE`…) commit by
    /// themselves: nothing can undo them, so they never run where Kuery
    /// promises an undo.
    pub implicit_commits: bool,
    /// The statement whose single value is a plan the Plan tab can draw,
    /// for an estimate or (`true`) a measured run; `None` where Kuery cannot
    /// draw this database's plans yet.
    pub explain: Option<Explain>,
}

/// The EXPLAIN statement for a statement, estimated or (`true`) measured.
pub type Explain = fn(&str, bool) -> Result<String, String>;

/// How Kuery opens and closes transactions and savepoints.
pub struct Transactions {
    /// Starts a transaction that can only read (and takes its snapshot).
    pub begin_read_only: &'static [&'static str],
    /// Ends it, undoing anything.
    pub end_read_only: &'static [&'static str],
    pub begin: &'static str,
    /// Keywords before a savepoint's name.
    pub savepoint: &'static str,
    pub rollback_to: &'static str,
    pub release: &'static str,
}

impl PartialEq for Dialect {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Dialect {}

impl std::fmt::Debug for Dialect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

impl Dialect {
    /// `"orders"`, or `` `orders` `` in MySQL.
    pub fn quote_ident(&self, name: &str) -> String {
        let quote = self.ident_quote;
        let doubled = format!("{quote}{quote}");
        format!("{quote}{}{quote}", name.replace(quote, &doubled))
    }

    /// `"public"."orders"`: the schema is SQLite's attached database
    /// (`main`) and MySQL's database.
    pub fn quote_qualified(&self, schema: &str, name: &str) -> String {
        format!("{}.{}", self.quote_ident(schema), self.quote_ident(name))
    }

    pub fn quote_literal(&self, value: &str) -> Result<String, SqlError> {
        (self.literal)(value)
    }

    pub fn quote_nullable(&self, value: Option<&str>) -> Result<String, SqlError> {
        match value {
            Some(value) => self.quote_literal(value),
            None => Ok("NULL".into()),
        }
    }

    pub fn as_text(&self, expr: &str) -> String {
        (self.as_text)(expr)
    }

    pub fn like_ignoring_case(&self) -> &'static str {
        self.like_ignoring_case
    }

    /// Sorts by `expr` with NULLs last either way.
    pub fn order_nulls_last(&self, expr: &str, descending: bool) -> String {
        let direction = if descending { "desc" } else { "asc" };
        if self.nulls_last {
            format!("{expr} {direction} nulls last")
        } else {
            format!("{expr} is null, {expr} {direction}")
        }
    }

    pub fn same(&self, a: &str, b: &str) -> String {
        (self.same)(a, b)
    }

    pub fn insert_defaults(&self, table: &str) -> String {
        self.insert_defaults.replace("{}", table)
    }

    pub fn parser(&self) -> Box<dyn sqlparser::dialect::Dialect> {
        (self.parser)()
    }

    pub fn savepoint(&self, name: &str) -> String {
        format!("{} {name}", self.transactions.savepoint)
    }

    pub fn rollback_to(&self, name: &str) -> String {
        format!("{} {name}", self.transactions.rollback_to)
    }

    pub fn release(&self, name: &str) -> String {
        format!("{} {name}", self.transactions.release)
    }
}

pub static POSTGRES: Dialect = Dialect {
    name: "PostgreSQL",
    ident_quote: '"',
    literal: quote_literal,
    as_text: |expr| format!("{expr}::text"),
    like_ignoring_case: "ilike",
    nulls_last: true,
    same: |a, b| format!("{a} is not distinct from {b}"),
    insert_defaults: "insert into {} default values",
    parser: || Box::new(sqlparser::dialect::PostgreSqlDialect {}),
    formatter: sqlformat::Dialect::PostgreSql,
    transactions: Transactions {
        // `select 1` takes the snapshot: from then on PostgreSQL does not
        // allow switching the transaction to read-write.
        begin_read_only: &["begin transaction read only", "select 1"],
        end_read_only: &["rollback"],
        begin: "begin transaction read write",
        savepoint: "savepoint",
        rollback_to: "rollback to savepoint",
        release: "release savepoint",
    },
    implicit_commits: false,
    explain: Some(crate::plan::explain_sql),
};

pub static SQLITE: Dialect = Dialect {
    name: "SQLite",
    ident_quote: '"',
    literal: |value| {
        if value.contains('\0') {
            return Err(SqlError::NulByte);
        }
        Ok(format!("'{}'", value.replace('\'', "''")))
    },
    as_text: |expr| format!("cast({expr} as text)"),
    like_ignoring_case: "like",
    nulls_last: true,
    same: |a, b| format!("{a} is {b}"),
    insert_defaults: "insert into {} default values",
    parser: || Box::new(sqlparser::dialect::SQLiteDialect {}),
    formatter: sqlformat::Dialect::Generic,
    transactions: Transactions {
        // SQLite has no read-only transactions: query_only refuses changes.
        begin_read_only: &["pragma query_only = on", "begin"],
        end_read_only: &["rollback", "pragma query_only = off"],
        begin: "begin immediate",
        savepoint: "savepoint",
        rollback_to: "rollback to",
        release: "release",
    },
    implicit_commits: false,
    explain: None,
};

pub static MYSQL: Dialect = Dialect {
    name: "MySQL",
    ident_quote: '`',
    // Backslashes are escapes unless NO_BACKSLASH_ESCAPES is set: text with
    // one goes as hex, which means the same either way.
    literal: |value| {
        if value.contains('\\') || value.contains('\0') {
            let hex: String = value.bytes().map(|b| format!("{b:02x}")).collect();
            Ok(format!("_utf8mb4 x'{hex}'"))
        } else {
            Ok(format!("'{}'", value.replace('\'', "''")))
        }
    },
    as_text: |expr| format!("cast({expr} as char)"),
    like_ignoring_case: "like",
    nulls_last: false,
    same: |a, b| format!("{a} <=> {b}"),
    insert_defaults: "insert into {} () values ()",
    parser: || Box::new(sqlparser::dialect::MySqlDialect {}),
    formatter: sqlformat::Dialect::Generic,
    transactions: Transactions {
        begin_read_only: &["start transaction read only"],
        end_read_only: &["rollback"],
        begin: "start transaction read write",
        savepoint: "savepoint",
        rollback_to: "rollback to savepoint",
        release: "release savepoint",
    },
    implicit_commits: true,
    explain: None,
};

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
        let (pg, lite, my) = (&POSTGRES, &SQLITE, &MYSQL);
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
