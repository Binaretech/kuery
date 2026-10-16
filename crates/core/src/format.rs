//! SQL formatting for the editor ("Format SQL").

use sqlformat::{FormatOptions, Indent, QueryParams};
use sqlparser::tokenizer::{Token, Tokenizer};

use crate::script::Offsets;
use crate::sql::Dialect;

/// Reindents `sql`: one clause per line, two-space indentation, a blank line
/// between statements. Keyword case is left as written, and so are comments,
/// strings and dollar-quoted bodies. SQL that cannot be tokenized comes back
/// unchanged.
pub fn format_sql(sql: &str) -> String {
    format_sql_in(Dialect::Postgres, sql)
}

/// The same, for a database speaking `dialect`.
pub fn format_sql_in(dialect: Dialect, sql: &str) -> String {
    // The formatter would reformat function bodies as if they were plain
    // SQL: swap them for placeholders and put them back afterwards.
    let Some((protected, bodies)) = protect_dollar_quotes(dialect, sql) else {
        return sql.to_string();
    };
    let mut formatted = format_plain(dialect, &protected);
    for (index, body) in bodies.iter().enumerate() {
        formatted = formatted.replacen(&placeholder(index), body, 1);
    }
    if sql.ends_with('\n') && !formatted.ends_with('\n') {
        formatted.push('\n');
    }
    formatted
}

fn placeholder(index: usize) -> String {
    format!("__kuery_body_{index}__")
}

/// `sql` with each dollar-quoted string replaced by a placeholder, and the
/// strings in order.
fn protect_dollar_quotes(dialect: Dialect, sql: &str) -> Option<(String, Vec<String>)> {
    let tokens = Tokenizer::new(&*dialect.parser(), sql)
        .tokenize_with_location()
        .ok()?;
    let offsets = Offsets::new(sql);
    let (mut out, mut bodies, mut copied) = (String::new(), Vec::new(), 0);
    for token in &tokens {
        if let Token::DollarQuotedString(_) = token.token {
            let (from, to) = (offsets.of(token.span.start), offsets.of(token.span.end));
            out.push_str(&sql[copied..from]);
            out.push_str(&placeholder(bodies.len()));
            bodies.push(sql[from..to].to_string());
            copied = to;
        }
    }
    out.push_str(&sql[copied..]);
    Some((out, bodies))
}

fn format_plain(dialect: Dialect, sql: &str) -> String {
    let options = FormatOptions {
        indent: Indent::Spaces(2),
        uppercase: None,
        lines_between_queries: 2,
        dialect: match dialect {
            Dialect::Postgres => sqlformat::Dialect::PostgreSql,
            Dialect::Sqlite | Dialect::Mysql => sqlformat::Dialect::Generic,
        },
        ..FormatOptions::default()
    };
    sqlformat::format(sql, &QueryParams::None, &options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn puts_clauses_on_their_own_lines() {
        assert_eq!(
            format_sql("select id, total from orders where status = 'paid' order by id;\n"),
            "select\n  id,\n  total\nfrom\n  orders\nwhere\n  status = 'paid'\norder by\n  id;\n"
        );
    }

    #[test]
    fn separates_statements_and_keeps_case_and_comments() {
        let formatted = format_sql("-- first\nSELECT 1; select 2");
        assert!(
            formatted.starts_with("-- first\nSELECT\n  1;\n\nselect\n  2"),
            "{formatted}"
        );
    }

    #[test]
    fn leaves_strings_and_dollar_quoted_bodies_alone() {
        let formatted = format_sql(
            "create function f() returns int language sql as $$ select   1 $$; \
             select 'a,   b'",
        );
        assert!(formatted.contains("$$ select   1 $$"), "{formatted}");
        assert!(formatted.contains("'a,   b'"), "{formatted}");
        assert!(!formatted.contains("__kuery_body_"), "{formatted}");
    }

    #[test]
    fn returns_sql_it_cannot_tokenize_unchanged() {
        assert_eq!(format_sql("select 'oops"), "select 'oops");
    }
}
