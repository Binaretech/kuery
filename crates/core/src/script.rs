//! Where each statement of a script is, so the editor can run the one under
//! the cursor. Semicolons inside strings, quoted identifiers, comments and
//! dollar-quoted bodies do not split statements. For a database with a batch
//! separator (`GO` in T-SQL) the unit is the batch instead: statements run
//! together and only a separator line splits.

use std::ops::Range;

use sqlparser::tokenizer::{Location, Token, Tokenizer, Whitespace};

use crate::sql::Dialect;

/// Byte ranges of the statements in `sql`, without the semicolon that ends
/// them. A comment right before a statement belongs to it; text with nothing
/// but comments and whitespace is no statement. `None` if `sql` cannot be
/// tokenized (an unterminated string, for example).
pub fn statements(sql: &str) -> Option<Vec<Range<usize>>> {
    statements_in(&crate::sql::POSTGRES, sql)
}

/// The same, for a database speaking `dialect` (MySQL's `#` comments and
/// backslash escapes, say). With a batch separator, one range per batch: the
/// separator lines are left out and `;` does not split.
pub fn statements_in(dialect: &Dialect, sql: &str) -> Option<Vec<Range<usize>>> {
    let tokens = Tokenizer::new(&*dialect.parser(), sql)
        .tokenize_with_location()
        .ok()?;
    let offsets = Offsets::new(sql);
    let mut ranges = Vec::new();
    // Start of the current statement (its first comment or token) and
    // whether it has anything besides comments.
    let mut start: Option<usize> = None;
    let mut has_code = false;
    let mut end = 0;
    // The end of the separator line just seen: its `;` and comment are
    // part of it.
    let mut separator_end = 0;
    for token in &tokens {
        let (from, to) = (offsets.of(token.span.start), offsets.of(token.span.end));
        if from < separator_end {
            continue;
        }
        if let (Some(separator), Token::Word(word)) = (dialect.batch_separator, &token.token)
            && word.quote_style.is_none()
            && word.value.eq_ignore_ascii_case(separator)
            && let Some(line_end) = separator_line(sql, from, to)
        {
            if let (Some(start), true) = (start, has_code) {
                ranges.push(start..end);
            }
            (start, has_code) = (None, false);
            separator_end = line_end;
            continue;
        }
        match &token.token {
            Token::Whitespace(Whitespace::Space | Whitespace::Newline | Whitespace::Tab) => {}
            Token::Whitespace(_) => {
                start.get_or_insert(from);
            }
            // Inside a batch a `;` ends a statement, not the unit.
            Token::SemiColon if dialect.batch_separator.is_some() => {
                if has_code {
                    end = to;
                }
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

/// Whether the word at `from..to` is a separator line (alone on its line,
/// but for a `;` or a `--` comment after it, as SSMS allows); where the line
/// ends if so. `GO n` (repeat the batch) is not supported.
fn separator_line(sql: &str, from: usize, to: usize) -> Option<usize> {
    let blank = |text: &str| text.chars().all(|c| c != '\n' && c.is_whitespace());
    let line_start = sql[..from].rfind('\n').map_or(0, |index| index + 1);
    let line_end = sql[to..].find('\n').map_or(sql.len(), |index| to + index);
    let rest = sql[to..line_end].trim_start();
    let rest = rest.strip_prefix(';').unwrap_or(rest).trim_start();
    let rest_is_blank = rest.is_empty() || rest.starts_with("--");
    (blank(&sql[line_start..from]) && rest_is_blank).then_some(line_end)
}

/// The statement to run for a cursor at byte `offset`: the one it is in, or
/// else the last one that ends before it (the cursor sits after its `;`), or
/// the first one.
pub fn statement_at(sql: &str, offset: usize) -> Option<Range<usize>> {
    statement_at_in(&crate::sql::POSTGRES, sql, offset)
}

/// The same, for a database speaking `dialect`.
pub fn statement_at_in(dialect: &Dialect, sql: &str, offset: usize) -> Option<Range<usize>> {
    let ranges = statements_in(dialect, sql)?;
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
    fn go_lines_split_t_sql_scripts_into_batches() {
        use crate::sql::MSSQL;
        let sql = "declare @n int = 1;\nselect @n;\nGO\nselect 2\n  go  \n-- GO in a comment\nselect 'GO'";
        let batches: Vec<&str> = statements_in(&MSSQL, sql)
            .unwrap()
            .into_iter()
            .map(|range| &sql[range])
            .collect();
        assert_eq!(
            batches,
            [
                "declare @n int = 1;\nselect @n;",
                "select 2",
                "-- GO in a comment\nselect 'GO'"
            ]
        );
        assert_eq!(
            statements_in(&MSSQL, "select 1; select 2").unwrap().len(),
            1
        );
        assert_eq!(
            statements_in(&crate::sql::POSTGRES, "select 1; select 2")
                .unwrap()
                .len(),
            2
        );
        // Only a line holding nothing but GO splits; empty batches vanish.
        let sql = "select 1 as go\nGO\nGO\nselect [go]\nselect 'a\nGO\nb'\n/*\nGO\n*/";
        let batches: Vec<&str> = statements_in(&MSSQL, sql)
            .unwrap()
            .into_iter()
            .map(|range| &sql[range])
            .collect();
        assert_eq!(
            batches,
            ["select 1 as go", "select [go]\nselect 'a\nGO\nb'"]
        );
    }

    #[test]
    fn a_go_line_may_end_with_a_semicolon_or_a_comment() {
        use crate::sql::MSSQL;
        let sql = "select 1\nGO -- first; done\nselect 2\ngo;\nselect 3\nGO ; -- x\nselect 4\nGO x";
        let batches: Vec<&str> = statements_in(&MSSQL, sql)
            .unwrap()
            .into_iter()
            .map(|range| &sql[range])
            .collect();
        assert_eq!(
            batches,
            ["select 1", "select 2", "select 3", "select 4\nGO x"]
        );
    }

    #[test]
    fn the_cursor_picks_a_go_batch() {
        use crate::sql::MSSQL;
        let sql = "select 1;\nselect 2\nGO\nselect 3";
        let at = |offset| &sql[statement_at_in(&MSSQL, sql, offset).unwrap()];
        assert_eq!(at(0), "select 1;\nselect 2");
        assert_eq!(at(sql.find("2").unwrap()), "select 1;\nselect 2");
        assert_eq!(at(sql.len()), "select 3");
    }

    #[test]
    fn unterminated_strings_cannot_be_split() {
        assert!(statements("select 'oops").is_none());
    }
}
