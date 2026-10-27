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
    /// What opens and closes an identifier (`"`, `` ` `` or `[` and `]`); the
    /// closing one is doubled inside.
    pub ident_quotes: (char, char),
    /// A text literal.
    pub literal: fn(&str) -> Result<String, SqlError>,
    /// An expression as text, the way its values are shown.
    pub as_text: fn(&str) -> String,
    /// The case-insensitive `LIKE`.
    pub like_ignoring_case: &'static str,
    /// How sorting puts NULLs last.
    pub nulls_last: NullsLast,
    /// `a` equals `b`, NULL equal to NULL.
    pub same: fn(&str, &str) -> String,
    /// A row with every column at its default; `{}` is the table.
    pub insert_defaults: &'static str,
    /// The parser for analysing and splitting its SQL.
    pub parser: fn() -> Box<dyn sqlparser::dialect::Dialect>,
    /// How the formatter reads it.
    pub formatter: sqlformat::Dialect,
    pub transactions: Transactions,
    /// How a page of rows is asked for.
    pub paging: Paging,
    /// The line that separates batches in a script (`GO`); `None` where
    /// statements are separated by `;`.
    pub batch_separator: Option<&'static str>,
    /// Schema changes (`CREATE`, `ALTER`, `DROP`, `TRUNCATE`…) commit by
    /// themselves: nothing can undo them, so they never run where Kuery
    /// promises an undo.
    pub implicit_commits: bool,
    /// The statement whose single value is a plan the Plan tab can draw,
    /// for an estimate or (`true`) a measured run; `None` where Kuery cannot
    /// draw this database's plans yet.
    pub explain: Option<Explain>,
    /// What cancelling a session's query does when the server cannot stop
    /// just the query; `None` when it can.
    pub cancel_note: Option<&'static str>,
    /// How row edits write and compare columns whose values are shown as
    /// `0x` hex; `None` where binary values are compared as text.
    pub binary: Option<Binary>,
}

/// Binary values, shown as `0x` and uppercase hex (`0x0A1B`, `0x` when
/// empty), the way a dialect writes and compares them.
pub struct Binary {
    /// Whether a column of this declared type holds bytes.
    pub is_type: fn(&str) -> bool,
    /// A value as it is shown, as a literal; `None` if it is not hex.
    pub literal: fn(&str) -> Option<String>,
    /// A column as text, the way its values are shown.
    pub as_text: fn(&str) -> String,
}

/// The EXPLAIN statement for a statement, estimated or (`true`) measured.
pub type Explain = fn(&str, bool) -> Result<String, String>;

/// How a database sorts NULLs after everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullsLast {
    /// `x desc nulls last`.
    Keyword,
    /// `x is null, x desc`: a condition can be sorted on.
    IsNullFirst,
    /// `case when x is null then 1 else 0 end, x desc`, where a condition
    /// cannot be sorted on.
    Case,
}

/// How a database limits a query to a page of rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paging {
    /// `limit 100 offset 0`.
    LimitOffset,
    /// `offset 0 rows fetch next 100 rows only`, which needs an `order by`.
    OffsetFetch,
}

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
    /// Frees a savepoint; `None` where savepoints cannot be released.
    pub release: Option<&'static str>,
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
        let (open, close) = self.ident_quotes;
        let doubled = format!("{close}{close}");
        format!("{open}{}{close}", name.replace(close, &doubled))
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

    /// Sorts by `expr` with NULLs last either way.
    pub fn order_nulls_last(&self, expr: &str, descending: bool) -> String {
        let direction = if descending { "desc" } else { "asc" };
        match self.nulls_last {
            NullsLast::Keyword => format!("{expr} {direction} nulls last"),
            NullsLast::IsNullFirst => format!("{expr} is null, {expr} {direction}"),
            NullsLast::Case => {
                format!("case when {expr} is null then 1 else 0 end, {expr} {direction}")
            }
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

    /// The statement that frees a savepoint, if the database has one.
    pub fn release(&self, name: &str) -> Option<String> {
        self.transactions
            .release
            .map(|release| format!("{release} {name}"))
    }

    /// The clause that limits a query to `limit` rows after `offset`.
    /// `ordered` says the query already has an `order by`, which
    /// `OffsetFetch` needs.
    pub fn page(&self, ordered: bool, limit: u64, offset: u64) -> String {
        match self.paging {
            Paging::LimitOffset => format!(" limit {limit} offset {offset}"),
            Paging::OffsetFetch => {
                let order = if ordered {
                    ""
                } else {
                    " order by (select null)"
                };
                format!("{order} offset {offset} rows fetch next {limit} rows only")
            }
        }
    }

    /// `select * … ` for the first `n` rows of `qualified`, for a new query tab.
    pub fn first_rows(&self, qualified: &str, n: u64) -> String {
        match self.paging {
            Paging::LimitOffset => format!("select *\nfrom {qualified}\nlimit {n};\n"),
            Paging::OffsetFetch => format!("select top ({n}) *\nfrom {qualified};\n"),
        }
    }

    /// Statements as one script that this database runs.
    pub fn join_script(&self, statements: &[&str]) -> String {
        match self.batch_separator {
            Some(separator) => statements.join(&format!("\n{separator}\n")),
            None => statements.join("; "),
        }
    }
}

pub static POSTGRES: Dialect = Dialect {
    name: "PostgreSQL",
    ident_quotes: ('"', '"'),
    literal: quote_literal,
    as_text: |expr| format!("{expr}::text"),
    like_ignoring_case: "ilike",
    nulls_last: NullsLast::Keyword,
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
        release: Some("release savepoint"),
    },
    paging: Paging::LimitOffset,
    batch_separator: None,
    implicit_commits: false,
    explain: Some(crate::plan::explain_sql),
    cancel_note: None,
    binary: None,
};

pub static SQLITE: Dialect = Dialect {
    name: "SQLite",
    ident_quotes: ('"', '"'),
    literal: |value| {
        if value.contains('\0') {
            return Err(SqlError::NulByte);
        }
        Ok(format!("'{}'", value.replace('\'', "''")))
    },
    as_text: |expr| format!("cast({expr} as text)"),
    like_ignoring_case: "like",
    nulls_last: NullsLast::Keyword,
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
        release: Some("release"),
    },
    paging: Paging::LimitOffset,
    batch_separator: None,
    implicit_commits: false,
    explain: None,
    cancel_note: None,
    binary: None,
};

/// MySQL's bytes: `BINARY`, `VARBINARY`, the `BLOB`s, `BIT` and the spatial
/// types, written as `x'…'` and compared through their bytes (`hex` alone
/// writes `BIT` values as numbers, without leading zeros).
const MYSQL_BINARY: Binary = Binary {
    is_type: |data_type| {
        let name = data_type
            .split(['(', ' '])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        matches!(
            name.as_str(),
            "binary"
                | "varbinary"
                | "tinyblob"
                | "blob"
                | "mediumblob"
                | "longblob"
                | "bit"
                | "geometry"
                | "point"
                | "linestring"
                | "polygon"
                | "multipoint"
                | "multilinestring"
                | "multipolygon"
                | "geometrycollection"
                | "geomcollection"
        )
    },
    literal: |value| {
        let hex = value.strip_prefix("0x")?;
        let valid = hex.len().is_multiple_of(2) && hex.bytes().all(|b| b.is_ascii_hexdigit());
        valid.then(|| format!("x'{hex}'"))
    },
    as_text: |expr| format!("concat('0x', hex(cast({expr} as binary)))"),
};

pub static MYSQL: Dialect = Dialect {
    name: "MySQL",
    ident_quotes: ('`', '`'),
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
    nulls_last: NullsLast::IsNullFirst,
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
        release: Some("release savepoint"),
    },
    paging: Paging::LimitOffset,
    batch_separator: None,
    implicit_commits: true,
    explain: None,
    cancel_note: None,
    binary: Some(MYSQL_BINARY),
};

pub static MSSQL: Dialect = Dialect {
    name: "SQL Server",
    ident_quotes: ('[', ']'),
    // N'…' keeps characters outside the code page; backslashes are plain.
    literal: |value| {
        if value.contains('\0') {
            return Err(SqlError::NulByte);
        }
        Ok(format!("N'{}'", value.replace('\'', "''")))
    },
    // ISO 8601 for dates and times; the plain conversion for what has no
    // style 126 (try_convert gives NULL there).
    as_text: |expr| {
        format!("coalesce(try_convert(nvarchar(max), {expr}, 126), convert(nvarchar(max), {expr}))")
    },
    like_ignoring_case: "like",
    nulls_last: NullsLast::Case,
    same: |a, b| format!("({a} = {b} or ({a} is null and {b} is null))"),
    insert_defaults: "insert into {} default values",
    parser: || Box::new(sqlparser::dialect::MsSqlDialect {}),
    formatter: sqlformat::Dialect::Generic,
    transactions: Transactions {
        begin_read_only: &["begin transaction kuery_read"],
        end_read_only: &["if @@trancount > 0 rollback transaction"],
        begin: "begin transaction",
        savepoint: "save transaction",
        rollback_to: "rollback transaction",
        // T-SQL has no RELEASE: a savepoint just stays until the transaction ends.
        release: None,
    },
    paging: Paging::OffsetFetch,
    batch_separator: Some("GO"),
    implicit_commits: false,
    explain: Some(crate::plan::showplan_sql),
    cancel_note: Some(
        "SQL Server can only end the whole session: its open transaction rolls back, its \
         locks are released and its application loses the connection.",
    ),
    binary: None,
};

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
        assert_eq!(POSTGRES.quote_ident("orders"), "\"orders\"");
        assert_eq!(POSTGRES.quote_ident("Order Items"), "\"Order Items\"");
        assert_eq!(POSTGRES.quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(
            POSTGRES.quote_qualified("public", "orders"),
            "\"public\".\"orders\""
        );
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
    fn speaks_t_sql() {
        let ms = &MSSQL;
        assert_eq!(ms.quote_qualified("dbo", "a]b"), "[dbo].[a]]b]");
        assert_eq!(ms.quote_ident("Order Items"), "[Order Items]");
        assert_eq!(ms.quote_literal("ñandú's").unwrap(), "N'ñandú''s'");
        assert_eq!(ms.quote_literal(r"C:\tmp").unwrap(), r"N'C:\tmp'");
        assert_eq!(ms.quote_literal("a\0b"), Err(SqlError::NulByte));
        assert_eq!(
            ms.page(false, 100, 0),
            " order by (select null) offset 0 rows fetch next 100 rows only"
        );
        assert_eq!(
            ms.page(true, 50, 150),
            " offset 150 rows fetch next 50 rows only"
        );
        assert_eq!(POSTGRES.page(true, 50, 150), " limit 50 offset 150");
        assert_eq!(POSTGRES.page(false, 100, 0), " limit 100 offset 0");
        assert_eq!(
            ms.first_rows("[dbo].[t]", 100),
            "select top (100) *\nfrom [dbo].[t];\n"
        );
        assert_eq!(
            POSTGRES.first_rows("\"public\".\"t\"", 100),
            "select *\nfrom \"public\".\"t\"\nlimit 100;\n"
        );
        assert_eq!(ms.release("s"), None);
        assert_eq!(
            POSTGRES.release("s").as_deref(),
            Some("release savepoint s")
        );
        assert_eq!(ms.join_script(&["a", "b"]), "a\nGO\nb");
        assert_eq!(MYSQL.join_script(&["a", "b"]), "a; b");
        assert_eq!(
            ms.order_nulls_last("[n]", true),
            "case when [n] is null then 1 else 0 end, [n] desc"
        );
        assert_eq!(
            ms.as_text("[n]"),
            "coalesce(try_convert(nvarchar(max), [n], 126), convert(nvarchar(max), [n]))"
        );
    }

    #[test]
    fn escapes_like_patterns() {
        assert_eq!(escape_like("50%_off\\"), "50\\%\\_off\\\\");
    }
}
