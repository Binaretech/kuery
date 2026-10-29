//! What completion offers where the cursor is, read from the text before
//! it: collections after `db.`, methods after `db.orders.`, operators in a
//! key that starts with `$` (stages in a pipeline), and fields in other
//! keys.

use kuery_core::document::language::CompletionContext;

use crate::literal::{Kind, Token, lex};
use crate::shell::parse::ends_line;

/// What the text before byte `offset` asks completion for.
pub fn context(text: &str, offset: usize) -> CompletionContext {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &text[..offset];
    // Inside a string or a comment: nothing.
    let Ok(tokens) = lex(before) else {
        return CompletionContext::Nothing;
    };
    let (prefix, tokens) = match tokens.split_last() {
        Some((
            Token {
                kind: Kind::Ident(word),
                span,
            },
            rest,
        )) if span.end == offset => (word.clone(), rest),
        _ => (String::new(), tokens.as_slice()),
    };
    let gap_start = tokens.last().map_or(0, |token| token.span.end);
    let gap_end = offset - prefix.len();
    if before[gap_start..gap_end].contains('/') {
        return CompletionContext::Nothing;
    }
    let statement = &tokens[statement_start(before, tokens)..];
    let Some(open) = brackets(statement) else {
        return CompletionContext::Nothing;
    };
    if open.is_empty() {
        return after_db(statement, prefix);
    }
    key(statement, &open, prefix)
}

/// Where the statement the cursor is in starts, among `tokens`.
fn statement_start(text: &str, tokens: &[Token]) -> usize {
    let mut depth = 0usize;
    let mut start = 0;
    for (i, token) in tokens.iter().enumerate() {
        if i > start && depth == 0 && ends_line(text, &tokens[i - 1], token) {
            start = i;
        }
        match token.kind {
            Kind::Punct('(' | '[' | '{') => depth += 1,
            Kind::Punct(')' | ']' | '}') => depth = depth.saturating_sub(1),
            Kind::Punct(';') if depth == 0 => start = i + 1,
            _ => {}
        }
    }
    start
}

/// The brackets still open at the end, each with its token's index;
/// `None` when they do not match.
fn brackets(tokens: &[Token]) -> Option<Vec<(char, usize)>> {
    let mut open = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        match token.kind {
            Kind::Punct(c @ ('(' | '[' | '{')) => open.push((c, i)),
            Kind::Punct(c @ (')' | ']' | '}')) => {
                let opener = match c {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                if open.pop().map(|(c, _)| c) != Some(opener) {
                    return None;
                }
            }
            _ => {}
        }
    }
    Some(open)
}

fn punct(token: Option<&Token>, c: char) -> bool {
    token.is_some_and(|token| token.kind == Kind::Punct(c))
}

fn ident(token: Option<&Token>) -> Option<&str> {
    match token.map(|token| &token.kind) {
        Some(Kind::Ident(name)) => Some(name),
        _ => None,
    }
}

/// Outside brackets: `db.` and `db.orders.`.
fn after_db(statement: &[Token], prefix: String) -> CompletionContext {
    let Some(rest) = after_database(statement) else {
        return CompletionContext::Nothing;
    };
    let kinds: Vec<&Kind> = rest.iter().map(|token| &token.kind).collect();
    match kinds.as_slice() {
        [Kind::Punct('.')] => CompletionContext::Collection { prefix },
        [Kind::Punct('.'), Kind::Ident(collection), Kind::Punct('.')] => {
            CompletionContext::Method {
                collection: collection.clone(),
                prefix,
            }
        }
        [
            Kind::Punct('.'),
            Kind::Ident(get),
            Kind::Punct('('),
            Kind::Str(collection),
            Kind::Punct(')'),
            Kind::Punct('.'),
        ] if get == "getCollection" => CompletionContext::Method {
            collection: collection.clone(),
            prefix,
        },
        _ => CompletionContext::Nothing,
    }
}

/// What follows `db`, or `db.getSiblingDB("x")`, at the start of a
/// statement.
fn after_database(statement: &[Token]) -> Option<&[Token]> {
    if ident(statement.first()) != Some("db") {
        return None;
    }
    let kinds: Vec<&Kind> = statement.iter().take(6).map(|token| &token.kind).collect();
    match kinds.as_slice() {
        [
            _,
            Kind::Punct('.'),
            Kind::Ident(sibling),
            Kind::Punct('('),
            Kind::Str(_),
            Kind::Punct(')'),
        ] if sibling == "getSiblingDB" => Some(&statement[6..]),
        _ => Some(&statement[1..]),
    }
}

/// Inside brackets: a key, after `{` or `,` in a document.
fn key(statement: &[Token], open: &[(char, usize)], prefix: String) -> CompletionContext {
    let Some(&('{', _)) = open.last() else {
        return CompletionContext::Nothing;
    };
    let last = statement.last();
    if !(punct(last, '{') || punct(last, ',')) {
        return CompletionContext::Nothing;
    }
    if !prefix.starts_with('$') {
        return CompletionContext::Field {
            collection: collection(statement),
            prefix,
        };
    }
    match in_pipeline(statement, open) {
        true => CompletionContext::Stage { prefix },
        false => CompletionContext::Operator { prefix },
    }
}

/// Whether the document the key is in is a stage of a pipeline:
/// `aggregate([{`, or `pipeline: [{` (in `$lookup`, `$unionWith`,
/// `createView`).
fn in_pipeline(statement: &[Token], open: &[(char, usize)]) -> bool {
    let [.., ('[', array), ('{', _)] = open else {
        return false;
    };
    let before = |back: usize| array.checked_sub(back).and_then(|i| statement.get(i));
    let called = punct(before(1), '(') && ident(before(2)) == Some("aggregate");
    let named = punct(before(1), ':')
        && match before(2).map(|token| &token.kind) {
            Some(Kind::Ident(name) | Kind::Str(name)) => name == "pipeline",
            _ => false,
        };
    called || named
}

/// The collection a statement works on: `db.orders…` or
/// `db.getCollection("orders")…`.
fn collection(statement: &[Token]) -> Option<String> {
    let rest = after_database(statement)?;
    let kinds: Vec<&Kind> = rest.iter().take(4).map(|token| &token.kind).collect();
    match kinds.as_slice() {
        [
            Kind::Punct('.'),
            Kind::Ident(get),
            Kind::Punct('('),
            Kind::Str(collection),
        ] if get == "getCollection" => Some(collection.clone()),
        [Kind::Punct('.'), Kind::Ident(collection), ..] => Some(collection.clone()),
        _ => None,
    }
}
