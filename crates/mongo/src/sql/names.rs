//! Where things are in a statement, and what its names name: sqlparser's
//! lines and columns as byte offsets, field paths, and the hints a parse
//! error gets.

use std::ops::Range;

use kuery_core::document::language::ParseError;
use sqlparser::ast::Spanned;
use sqlparser::keywords::ALL_KEYWORDS;
use sqlparser::tokenizer::{Location, Span};

/// A statement's text, which turns sqlparser's positions (lines and
/// columns from 1, in characters) into byte offsets.
pub(super) struct Text<'a> {
    pub(super) text: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> Text<'a> {
    pub(super) fn new(text: &'a str) -> Text<'a> {
        let line_starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(at, _)| at + 1))
            .collect();
        Text { text, line_starts }
    }

    fn offset(&self, location: Location) -> usize {
        let Some(&start) = (location.line as usize)
            .checked_sub(1)
            .and_then(|line| self.line_starts.get(line))
        else {
            return self.text.len();
        };
        let column = (location.column as usize).saturating_sub(1);
        self.text[start..]
            .char_indices()
            .nth(column)
            .map_or(self.text.len(), |(at, _)| start + at)
    }

    /// The bytes a span covers; `None` for the empty span sqlparser gives
    /// what it does not place.
    pub(super) fn span(&self, span: Span) -> Option<Range<usize>> {
        if span.start.line == 0 {
            return None;
        }
        let start = self.offset(span.start);
        let end = self.offset(span.end).max(start);
        let end = self.call_end(start).map_or(end, |close| close.max(end));
        Some(self.balanced(start..end))
    }

    /// The range grown over the brackets it opens or closes without the
    /// other half: sqlparser leaves them out of subqueries and indexes.
    fn balanced(&self, mut range: Range<usize>) -> Range<usize> {
        for (opener, closer) in [('(', ')'), ('[', ']')] {
            let (mut open, mut unopened) = (0usize, 0usize);
            let mut quote = None;
            for c in self.text[range.clone()].chars() {
                match (quote, c) {
                    (Some(q), c) if c == q => quote = None,
                    (Some(_), _) => {}
                    (None, '\'' | '"') => quote = Some(c),
                    (None, c) if c == opener => open += 1,
                    (None, c) if c == closer && open > 0 => open -= 1,
                    (None, c) if c == closer => unopened += 1,
                    _ => {}
                }
            }
            while open > 0 {
                let rest = &self.text[range.end..];
                let skipped = rest.len() - rest.trim_start().len();
                if !rest[skipped..].starts_with(closer) {
                    break;
                }
                range.end += skipped + 1;
                open -= 1;
            }
            while unopened > 0 {
                let before = self.text[..range.start].trim_end();
                if !before.ends_with(opener) {
                    break;
                }
                range.start = before.len() - 1;
                unopened -= 1;
            }
        }
        range
    }

    /// Where a function call that starts at `start` ends (after its `)`):
    /// sqlparser places a call by its name and arguments only, and a
    /// diagnostic reads better under the whole call.
    fn call_end(&self, start: usize) -> Option<usize> {
        let rest = &self.text[start..];
        let name = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if name == 0 {
            return None;
        }
        let open = name + rest[name..].len() - rest[name..].trim_start().len();
        if !rest[open..].starts_with('(') {
            return None;
        }
        let mut depth = 0;
        let mut quote = None;
        for (at, c) in rest[open..].char_indices() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), _) => {}
                (None, '\'' | '"') => quote = Some(c),
                (None, '(') => depth += 1,
                (None, ')') => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(start + open + at + 1);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// The range grown back over the word before it: sqlparser places a
    /// typed value (`DECIMAL '9.99'`) by its text alone.
    pub(super) fn with_word_before(&self, range: Range<usize>) -> Range<usize> {
        let before = self.text[..range.start].trim_end();
        let word = before
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
            .last()
            .map_or(before.len(), |(at, _)| at);
        word..range.end
    }

    /// The whole statement, without the blanks around it.
    pub(super) fn whole(&self) -> Range<usize> {
        let start = self.text.len() - self.text.trim_start().len();
        start..self.text.trim_end().len().max(start)
    }

    /// Where a node is, or the whole statement when sqlparser does not say.
    pub(super) fn of(&self, node: &impl Spanned) -> Range<usize> {
        self.span(node.span()).unwrap_or_else(|| self.whole())
    }

    pub(super) fn error(&self, span: Range<usize>, message: impl Into<String>) -> ParseError {
        ParseError {
            span,
            message: message.into(),
        }
    }
}

/// A field, by the names on its way down: `address.city`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Path(pub(super) Vec<String>);

impl Path {
    /// As MongoDB names it in a filter: `address.city`.
    pub(super) fn dotted(&self) -> String {
        self.0.join(".")
    }

    /// The paths on its way down, shortest first, itself last: `a`, `a.b`.
    pub(super) fn prefixes(&self) -> impl Iterator<Item = String> + '_ {
        (1..=self.0.len()).map(|n| self.0[..n].join("."))
    }
}

/// Why a step of a path cannot be read by MongoDB's dotted paths, if it
/// cannot.
pub(super) fn unreadable(name: &str) -> Option<String> {
    if name.is_empty() {
        Some("A field's name is not empty".into())
    } else if name.starts_with('$') {
        Some(format!(
            "{name} starts with $, which MongoDB reads as an operator, not a field"
        ))
    } else if name.contains('.') {
        Some(format!(
            "\"{name}\" has a dot in its name, which a path cannot reach: read it in mongosh"
        ))
    } else if name.contains('\0') {
        Some("A field's name has no NUL character".into())
    } else if name.chars().all(|c| c.is_ascii_digit()) {
        Some(format!(
            "{name} reads as an array index, and Kuery's SQL does not index arrays"
        ))
    } else {
        None
    }
}

/// Whether sqlparser reads `word` as a keyword.
pub(super) fn reserved(word: &str) -> bool {
    let upper = word.to_ascii_uppercase();
    ALL_KEYWORDS.binary_search(&upper.as_str()).is_ok()
}

/// A parse error at the place sqlparser names (`… at Line: 1, Column:
/// 24`), or at the end when it names none, with a hint for the usual
/// causes.
pub(super) fn parse_error(text: &Text, message: &str) -> ParseError {
    let (message, location) = match message.rsplit_once(" at Line: ") {
        Some((message, place)) => {
            let mut numbers = place
                .split(", Column: ")
                .map(|n| n.trim().parse::<u64>().ok());
            match (numbers.next().flatten(), numbers.next().flatten()) {
                (Some(line), Some(column)) => (message, Some(Location { line, column })),
                _ => (message, None),
            }
        }
        None => (message, None),
    };
    let found = message
        .rsplit_once("found: ")
        .map(|(_, found)| found.trim())
        .filter(|found| *found != "EOF");
    let span = match location {
        Some(location) => {
            let start = text.offset(location);
            let length = found.map_or(1, |found| found.chars().count().max(1));
            let end = text.text[start..]
                .char_indices()
                .nth(length)
                .map_or(text.text.len(), |(at, _)| start + at);
            start..end
        }
        None => {
            let end = text.whole().end;
            let start = text.text[..end]
                .char_indices()
                .next_back()
                .map_or(end, |(at, _)| at);
            start..end
        }
    };
    let mut message = message.trim().to_string();
    // A reserved word read as one stops sqlparser at the token after it.
    let before = text.text[..span.start].trim_end();
    let word = before
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
        .map_or(0, |at| {
            at + before[at..].chars().next().map_or(1, char::len_utf8)
        });
    let previous = &before[word..];
    if !previous.is_empty() && reserved(previous) && !text.text[..word].trim_end().ends_with('.') {
        let message = format!(
            "{message}; if {previous} is a field, write it in double quotes: \"{previous}\""
        );
        return text.error(word..before.len(), message);
    }
    if let Some(found) = found {
        if found.starts_with('.') && found[1..].starts_with(|c: char| c.is_ascii_digit()) {
            message = format!(
                "Kuery's SQL does not index arrays: a path takes field names only ({message})"
            );
        } else if found.chars().all(|c| c.is_alphanumeric() || c == '_') && reserved(found) {
            message =
                format!("{message}; if {found} is a field, write it in double quotes: \"{found}\"");
        }
    }
    text.error(span, message)
}
