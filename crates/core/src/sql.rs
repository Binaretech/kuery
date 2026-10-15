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
    fn escapes_like_patterns() {
        assert_eq!(escape_like("50%_off\\"), "50\\%\\_off\\\\");
    }
}
