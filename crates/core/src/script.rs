//! Where each statement of a script is, so the editor can run the one under
//! the cursor. Semicolons inside strings, quoted identifiers, comments and
//! dollar-quoted bodies do not split statements.

use std::ops::Range;

use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Location, Token, Tokenizer, Whitespace};

/// Byte ranges of the statements in `sql`, without the semicolon that ends
/// them. A comment right before a statement belongs to it; text with nothing
/// but comments and whitespace is no statement. `None` if `sql` cannot be
/// tokenized (an unterminated string, for example).
pub fn statements(sql: &str) -> Option<Vec<Range<usize>>> {
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, sql)
        .tokenize_with_location()
        .ok()?;
    let offsets = Offsets::new(sql);
    let mut ranges = Vec::new();
    // Start of the current statement (its first comment or token) and
    // whether it has anything besides comments.
    let mut start: Option<usize> = None;
    let mut has_code = false;
    let mut end = 0;
    for token in &tokens {
        let (from, to) = (offsets.of(token.span.start), offsets.of(token.span.end));
        match &token.token {
            Token::Whitespace(Whitespace::Space | Whitespace::Newline | Whitespace::Tab) => {}
            Token::Whitespace(_) => {
                start.get_or_insert(from);
            }
            Token::SemiColon => {
                if let (Some(start), true) = (start, has_code) {
                    ranges.push(start..end);
                }
                (start, has_code) = (None, false);
            }
            Token::EOF => break,
            _ => {
                start.get_or_insert(from);
                has_code = true;
                end = to;
            }
        }
    }
    if let (Some(start), true) = (start, has_code) {
        ranges.push(start..end);
    }
    Some(ranges)
}

/// The statement to run for a cursor at byte `offset`: the one it is in, or
/// else the last one that ends before it (the cursor sits after its `;`), or
/// the first one.
pub fn statement_at(sql: &str, offset: usize) -> Option<Range<usize>> {
    let ranges = statements(sql)?;
    ranges
        .iter()
        .find(|range| range.start <= offset && offset <= range.end)
        .or_else(|| ranges.iter().rev().find(|range| range.end <= offset))
        .or_else(|| ranges.first())
        .cloned()
}

/// Turns the tokenizer's line and column (1-based, in characters) into byte
/// offsets.
pub(crate) struct Offsets<'a> {
    sql: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> Offsets<'a> {
    pub(crate) fn new(sql: &'a str) -> Self {
        let line_starts = std::iter::once(0)
            .chain(sql.match_indices('\n').map(|(index, _)| index + 1))
            .collect();
        Self { sql, line_starts }
    }

    pub(crate) fn of(&self, location: Location) -> usize {
        let Some(&line_start) = self
            .line_starts
            .get((location.line as usize).saturating_sub(1))
        else {
            return self.sql.len();
        };
        let column = (location.column as usize).saturating_sub(1);
        self.sql[line_start..]
            .char_indices()
            .nth(column)
            .map_or(self.sql.len(), |(index, _)| line_start + index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<&str> {
        statements(sql)
            .unwrap()
            .into_iter()
            .map(|range| &sql[range])
            .collect()
    }

    #[test]
    fn splits_on_semicolons_outside_strings_comments_and_bodies() {
        let sql = "select 'a;b';\n-- totals; by day\nselect \"x;y\" from t;\n\
                   create function f() returns int language sql as $$ select 1; $$;\n\
                   /* trailing; */";
        assert_eq!(
            texts(sql),
            [
                "select 'a;b'",
                "-- totals; by day\nselect \"x;y\" from t",
                "create function f() returns int language sql as $$ select 1; $$",
            ]
        );
    }

    #[test]
    fn the_last_statement_needs_no_semicolon() {
        assert_eq!(texts("select 1;\n\nselect 2\n"), ["select 1", "select 2"]);
        assert!(texts("-- only a comment\n").is_empty());
    }

    #[test]
    fn finds_the_statement_under_the_cursor() {
        let sql = "select 1;\nselect 2;\n\nselect ñ, 3;";
        let at = |offset| &sql[statement_at(sql, offset).unwrap()];
        assert_eq!(at(0), "select 1");
        assert_eq!(at(8), "select 1");
        // Right after a `;`, or on the blank line after it: that statement.
        assert_eq!(at(9), "select 1");
        assert_eq!(at(sql.find("2;").unwrap() + 2), "select 2");
        assert_eq!(at(sql.find("\n\n").unwrap() + 1), "select 2");
        assert_eq!(at(sql.len()), "select ñ, 3");
    }

    #[test]
    fn multibyte_text_keeps_byte_offsets() {
        let sql = "select 'ñandú';\nselect 'été'";
        assert_eq!(texts(sql), ["select 'ñandú'", "select 'été'"]);
    }

    #[test]
    fn unterminated_strings_cannot_be_split() {
        assert!(statements("select 'oops").is_none());
    }
}
