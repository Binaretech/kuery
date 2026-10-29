//! MongoDB's literals as mongosh writes them, read and printed without
//! evaluating anything. One grammar for the console, the filter bar and the
//! document editor: JSON5 objects and arrays (unquoted keys, single quotes,
//! trailing commas, comments), `/regex/flags`, the type constructors
//! (`ObjectId()`, `ISODate()`, `NumberLong()`, `UUID()`…) and Extended JSON
//! wrappers (`{"$numberLong": "5"}`).
//!
//! Printing and reading back gives the same value for every BSON type,
//! which the tests check over random values. Numbers follow one rule both
//! ways: `42` is an Int32 (an Int64 when it does not fit), `42.0` and
//! `4.2e1` are Doubles, and a Double always prints with a point or an
//! exponent. That is the one place Kuery and mongosh differ: in mongosh,
//! js-bson stores `42.0` as an Int32, because JavaScript has one kind of
//! number; Kuery keeps a Double.
//!
//! A document whose only fields are those of an Extended JSON wrapper
//! (`{$oid: "…"}`) reads as the wrapped type, as in mongosh, unless a key
//! writes its `$` as an escape (`{"\u0024oid": "…"}`): that is how a
//! document that only looks like a wrapper prints, so it reads back as
//! itself.

use std::ops::Range;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bson::oid::ObjectId;
use bson::{Binary, Bson, DateTime, Decimal128, JavaScriptCodeWithScope, Regex, Timestamp};
use kuery_core::document::export::{Ejson, iso_date, to_ejson};
use kuery_core::document::language::{FilterCheck, ParseError, Style};
use kuery_core::document::value::{Document, FieldPath, Value};

use crate::convert;

#[cfg(test)]
mod tests;

/// Reads one value.
pub fn parse_value(text: &str) -> Result<Value, ParseError> {
    let tokens = lex(text)?;
    let mut reader = Reader::new(text, &tokens);
    let (value, _) = reader.value()?;
    reader.end()?;
    Ok(value)
}

/// Reads a document: the editor's text, a filter, a projection.
pub fn parse_document(text: &str) -> Result<Document, ParseError> {
    let tokens = lex(text)?;
    let mut reader = Reader::new(text, &tokens);
    let document = reader.document()?;
    reader.end()?;
    Ok(document)
}

/// Checks a filter or projection bar's text; an empty bar is no filter.
pub fn check_filter(text: &str) -> Result<FilterCheck, ParseError> {
    if text.trim().is_empty() {
        return Ok(FilterCheck { heavy: false });
    }
    let document = parse_document(text)?;
    Ok(FilterCheck {
        heavy: runs_javascript(&Value::Object(document)),
    })
}

/// Operators that make the server run JavaScript.
const JAVASCRIPT: &[&str] = &["$where", "$function", "$accumulator"];

/// Whether a filter, a projection or a pipeline makes the server run
/// JavaScript, at any depth.
pub(crate) fn runs_javascript(value: &Value) -> bool {
    any_key(value, |name| JAVASCRIPT.contains(&name))
}

/// Whether a key of a document anywhere in `value` is one `found` looks
/// for. Walked without recursion, so no nesting overflows the stack.
pub(crate) fn any_key(value: &Value, found: impl Fn(&str) -> bool) -> bool {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(document) => {
                for (name, value) in &document.0 {
                    if found(name) {
                        return true;
                    }
                    pending.push(value);
                }
            }
            Value::Array(items) => pending.extend(items),
            _ => {}
        }
    }
    false
}

/// A value as a literal that reads back the same.
pub fn render(value: &Value, style: Style) -> String {
    let mut out = String::new();
    write_value(&mut out, value, style, 0);
    out
}

/// A filter by one field: `{"items.0.sku": "sku-001"}`. A null asks for
/// its type, since `{field: null}` also finds the documents that lack the
/// field; a missing field is `{field: {$exists: false}}`.
pub fn field_filter(path: &FieldPath, value: Option<&Value>) -> Option<String> {
    let condition =
        |operator: &str, value: Value| Value::Object(Document(vec![(operator.to_string(), value)]));
    let matched = match value {
        None => condition("$exists", Value::Bool(false)),
        Some(Value::Null) => condition("$type", Value::String("null".into())),
        Some(value) => value.clone(),
    };
    let filter = Document(vec![(path.dotted(), matched)]);
    Some(render(&Value::Object(filter), Style::Compact))
}

// Lexer ----------------------------------------------------------------

/// A piece of text: what it is, and where.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    pub(crate) kind: Kind,
    pub(crate) span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kind {
    /// A string, its escapes read.
    Str(String),
    /// A number as written (`42`, `4.2e1`, `0x1F`).
    Number(String),
    Ident(String),
    Regex {
        pattern: String,
        flags: String,
    },
    /// Any other ASCII sign: `{`, `.`, `;`…
    Punct(char),
}

/// Splits text into tokens, leaving out spaces and comments. A `/` that
/// does not start a comment starts a regular expression: there is no
/// division in what Kuery reads.
pub(crate) fn lex(text: &str) -> Result<Vec<Token>, ParseError> {
    let mut lexer = Lexer { text, at: 0 };
    let mut tokens = Vec::new();
    while let Some(c) = lexer.peek() {
        let start = lexer.at;
        let kind = match c {
            c if c.is_whitespace() => {
                lexer.bump();
                continue;
            }
            '/' if lexer.peek_at(1) == Some('/') => {
                while lexer.peek().is_some_and(|c| c != '\n') {
                    lexer.bump();
                }
                continue;
            }
            '/' if lexer.peek_at(1) == Some('*') => {
                match text[start + 2..].find("*/") {
                    Some(end) => lexer.at = start + 2 + end + 2,
                    None => return Err(error(start..text.len(), "This comment never ends")),
                }
                continue;
            }
            '/' => lexer.regex()?,
            '"' | '\'' => Kind::Str(lexer.string()?),
            c if c.is_ascii_digit()
                || (c == '.' && lexer.peek_at(1).is_some_and(|c| c.is_ascii_digit())) =>
            {
                lexer.number()?
            }
            c if starts_name(c) => {
                while lexer.peek().is_some_and(continues_name) {
                    lexer.bump();
                }
                Kind::Ident(text[start..lexer.at].to_string())
            }
            c if c.is_ascii_punctuation() => {
                lexer.bump();
                Kind::Punct(c)
            }
            c => {
                return Err(error(
                    start..start + c.len_utf8(),
                    format!("Unexpected character {c:?}"),
                ));
            }
        };
        tokens.push(Token {
            kind,
            span: start..lexer.at,
        });
    }
    Ok(tokens)
}

fn starts_name(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}

fn continues_name(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

struct Lexer<'a> {
    text: &'a str,
    at: usize,
}

impl Lexer<'_> {
    fn peek(&self) -> Option<char> {
        self.text[self.at..].chars().next()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.text[self.at..].chars().nth(n)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += c.len_utf8();
        Some(c)
    }

    fn digits(&mut self, radix: u32) -> usize {
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_digit(radix)) {
            self.bump();
        }
        self.at - start
    }

    fn number(&mut self) -> Result<Kind, ParseError> {
        let start = self.at;
        let hex = self.peek() == Some('0') && matches!(self.peek_at(1), Some('x' | 'X'));
        if hex {
            self.at += 2;
            if self.digits(16) == 0 {
                return Err(error(start..self.at, "A hexadecimal number needs digits"));
            }
        } else {
            self.digits(10);
            if self.peek() == Some('.') {
                self.bump();
                self.digits(10);
            }
            if matches!(self.peek(), Some('e' | 'E')) {
                self.bump();
                if matches!(self.peek(), Some('+' | '-')) {
                    self.bump();
                }
                if self.digits(10) == 0 {
                    return Err(error(start..self.at, "An exponent needs digits"));
                }
            }
        }
        if self.peek().is_some_and(continues_name) {
            return Err(error(
                start..self.at + 1,
                "A number runs into a name: put a space or a comma between them",
            ));
        }
        Ok(Kind::Number(self.text[start..self.at].to_string()))
    }

    fn string(&mut self) -> Result<String, ParseError> {
        let start = self.at;
        let quote = self.bump();
        let unfinished = |at: usize| error(start..at, "This string never ends");
        let mut value = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => return Err(unfinished(self.at)),
                Some(c) if Some(c) == quote => return Ok(value),
                Some('\\') => {
                    let escape = self.at - 1;
                    match self.bump() {
                        None => return Err(unfinished(self.at)),
                        Some('n') => value.push('\n'),
                        Some('r') => value.push('\r'),
                        Some('t') => value.push('\t'),
                        Some('b') => value.push('\u{8}'),
                        Some('f') => value.push('\u{c}'),
                        Some('v') => value.push('\u{b}'),
                        Some('0') if !self.peek().is_some_and(|c| c.is_ascii_digit()) => {
                            value.push('\0');
                        }
                        Some('x') => value.push(self.code_point(escape, 2)?),
                        Some('u') => value.push(self.unicode(escape)?),
                        // A line continuation.
                        Some('\r') => {
                            if self.peek() == Some('\n') {
                                self.bump();
                            }
                        }
                        Some('\n' | '\u{2028}' | '\u{2029}') => {}
                        Some(c) => value.push(c),
                    }
                }
                Some(c) => value.push(c),
            }
        }
    }

    /// `A`, `\u{1F600}`, or a surrogate pair `😀`.
    fn unicode(&mut self, escape: usize) -> Result<char, ParseError> {
        if self.peek() == Some('{') {
            self.bump();
            let start = self.at;
            self.digits(16);
            let digits = &self.text[start..self.at];
            if self.bump() != Some('}') || digits.is_empty() {
                return Err(error(escape..self.at, "Write a code point as \\u{1F600}"));
            }
            return u32::from_str_radix(digits, 16)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| error(escape..self.at, "This is not a character"));
        }
        let high = self.hex(escape, 4)?;
        if !(0xD800..0xDC00).contains(&high) {
            return char::from_u32(high)
                .ok_or_else(|| error(escape..self.at, "This is not a character"));
        }
        // The second half of the pair.
        if self.bump() != Some('\\') || self.bump() != Some('u') {
            return Err(error(
                escape..self.at,
                "Half of a surrogate pair is not a character",
            ));
        }
        let low = self.hex(escape, 4)?;
        if !(0xDC00..0xE000).contains(&low) {
            return Err(error(
                escape..self.at,
                "Half of a surrogate pair is not a character",
            ));
        }
        char::from_u32(0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00))
            .ok_or_else(|| error(escape..self.at, "This is not a character"))
    }

    fn code_point(&mut self, escape: usize, digits: usize) -> Result<char, ParseError> {
        let code = self.hex(escape, digits)?;
        char::from_u32(code).ok_or_else(|| error(escape..self.at, "This is not a character"))
    }

    fn hex(&mut self, escape: usize, digits: usize) -> Result<u32, ParseError> {
        let start = self.at;
        for _ in 0..digits {
            if !self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                return Err(error(
                    escape..self.at,
                    format!("This escape takes {digits} hexadecimal digits"),
                ));
            }
            self.bump();
        }
        Ok(u32::from_str_radix(&self.text[start..self.at], 16).expect("hexadecimal digits"))
    }

    /// `/pattern/flags`, the pattern as written.
    fn regex(&mut self) -> Result<Kind, ParseError> {
        let start = self.at;
        self.bump();
        let mut pattern = String::new();
        let mut class = false;
        loop {
            match self.bump() {
                None | Some('\n') => {
                    return Err(error(start..self.at, "This regular expression never ends"));
                }
                Some('\\') => {
                    pattern.push('\\');
                    match self.bump() {
                        None | Some('\n') => {
                            return Err(error(
                                start..self.at,
                                "This regular expression never ends",
                            ));
                        }
                        Some(c) => pattern.push(c),
                    }
                }
                Some('/') if !class => break,
                Some(c) => {
                    class = match c {
                        '[' => true,
                        ']' => false,
                        _ => class,
                    };
                    pattern.push(c);
                }
            }
        }
        let flags = self.at;
        while self.peek().is_some_and(continues_name) {
            self.bump();
        }
        Ok(Kind::Regex {
            pattern,
            flags: self.text[flags..self.at].to_string(),
        })
    }
}

pub(crate) fn error(span: Range<usize>, message: impl Into<String>) -> ParseError {
    ParseError {
        span,
        message: message.into(),
    }
}

// Reader ----------------------------------------------------------------

/// Reads literals from tokens; the console's parser reads its commands
/// around them with the same reader.
pub(crate) struct Reader<'a> {
    text: &'a str,
    tokens: &'a [Token],
    pub(crate) at: usize,
    /// Values open around the one read now, up to [`MAX_DEPTH`].
    depth: usize,
}

/// How deep values may nest: far deeper than any query, and shallow
/// enough that reading them (on the UI thread too) never runs out of
/// stack.
pub(crate) const MAX_DEPTH: usize = 200;

/// The Extended JSON wrappers: a document with exactly these fields is the
/// type they wrap.
const WRAPPERS: &[&[&str]] = &[
    &["$oid"],
    &["$numberInt"],
    &["$numberLong"],
    &["$numberDouble"],
    &["$numberDecimal"],
    &["$date"],
    &["$binary"],
    &["$binary", "$type"],
    &["$uuid"],
    &["$timestamp"],
    &["$regularExpression"],
    &["$code"],
    &["$code", "$scope"],
    &["$symbol"],
    &["$minKey"],
    &["$maxKey"],
    &["$dbPointer"],
    &["$undefined"],
];

impl<'a> Reader<'a> {
    pub(crate) fn new(text: &'a str, tokens: &'a [Token]) -> Self {
        Reader {
            text,
            tokens,
            at: 0,
            depth: 0,
        }
    }

    /// The text under a span of the reader's tokens.
    pub(crate) fn text_at(&self, span: Range<usize>) -> &'a str {
        &self.text[span]
    }

    /// Where the last token read ends.
    pub(crate) fn last_end(&self) -> usize {
        self.at
            .checked_sub(1)
            .map_or(0, |last| self.tokens[last].span.end)
    }

    pub(crate) fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.at)
    }

    pub(crate) fn peek_punct(&self, c: char) -> bool {
        self.peek()
            .is_some_and(|token| token.kind == Kind::Punct(c))
    }

    /// Where the text ends, for what is missing there.
    fn eof(&self) -> Range<usize> {
        self.text.len()..self.text.len()
    }

    /// The next token's span, or the end.
    pub(crate) fn here(&self) -> Range<usize> {
        self.peek()
            .map_or_else(|| self.eof(), |token| token.span.clone())
    }

    pub(crate) fn expect(&mut self, c: char, message: &str) -> Result<Range<usize>, ParseError> {
        if self.peek_punct(c) {
            self.at += 1;
            return Ok(self.tokens[self.at - 1].span.clone());
        }
        Err(error(self.here(), message))
    }

    /// Nothing may follow.
    pub(crate) fn end(&self) -> Result<(), ParseError> {
        match self.peek() {
            None => Ok(()),
            Some(token) => Err(error(token.span.clone(), "Unexpected text after the value")),
        }
    }

    /// A document, and nothing else.
    pub(crate) fn document(&mut self) -> Result<Document, ParseError> {
        let (value, span) = self.value()?;
        match value {
            Value::Object(document) => Ok(document),
            _ => Err(error(
                span,
                "Expected a document, such as {status: \"paid\"}",
            )),
        }
    }

    /// One value, and where it was. Every nested value is read through
    /// here, which keeps their depth to [`MAX_DEPTH`].
    pub(crate) fn value(&mut self) -> Result<(Value, Range<usize>), ParseError> {
        if self.depth >= MAX_DEPTH {
            return Err(error(
                self.here(),
                format!("Values nest deeper than {MAX_DEPTH} levels here"),
            ));
        }
        self.depth += 1;
        let read = self.nested();
        self.depth -= 1;
        read
    }

    fn nested(&mut self) -> Result<(Value, Range<usize>), ParseError> {
        let Some(token) = self.peek() else {
            return Err(error(self.eof(), "Expected a value"));
        };
        let span = token.span.clone();
        match &token.kind {
            Kind::Punct('{') => self.object(),
            Kind::Punct('[') => self.array(),
            Kind::Punct(sign @ ('-' | '+')) => self.signed(*sign),
            Kind::Str(text) => {
                self.at += 1;
                Ok((Value::String(text.clone()), span))
            }
            Kind::Number(text) => {
                self.at += 1;
                Ok((number(text), span))
            }
            Kind::Regex { pattern, flags } => {
                self.at += 1;
                let regex =
                    regex(pattern, flags).map_err(|message| error(span.clone(), message))?;
                Ok((regex, span))
            }
            Kind::Ident(name) => {
                self.at += 1;
                self.named(name, span)
            }
            Kind::Punct(_) => Err(error(span, "Expected a value")),
        }
    }

    /// `-5`, `+1.5`, `-Infinity`.
    fn signed(&mut self, sign: char) -> Result<(Value, Range<usize>), ParseError> {
        let start = self.here().start;
        self.at += 1;
        let Some(token) = self.peek() else {
            return Err(error(
                start..self.text.len(),
                "Expected a number after the sign",
            ));
        };
        let span = start..token.span.end;
        let negative = sign == '-';
        let value = match &token.kind {
            Kind::Number(text) if negative => number(&format!("-{text}")),
            Kind::Number(text) => number(text),
            Kind::Ident(name) if name == "Infinity" => Value::Double(match negative {
                true => f64::NEG_INFINITY,
                false => f64::INFINITY,
            }),
            _ => return Err(error(span, "Expected a number after the sign")),
        };
        self.at += 1;
        Ok((value, span))
    }

    fn object(&mut self) -> Result<(Value, Range<usize>), ParseError> {
        let open = self.expect('{', "Expected {")?;
        let mut fields: Vec<(String, Value)> = Vec::new();
        let mut escaped = false;
        while !self.peek_punct('}') {
            let Some(token) = self.peek() else {
                return Err(error(
                    open.start..self.text.len(),
                    "This document never ends: add }",
                ));
            };
            let name = match &token.kind {
                Kind::Str(name) | Kind::Ident(name) | Kind::Number(name) => name.clone(),
                _ => return Err(error(token.span.clone(), "Expected a field name")),
            };
            // `"\u0024oid"`: a field named `$oid`, not a wrapper's key.
            if name.starts_with('$') && self.text_at(token.span.clone())[1..].starts_with('\\') {
                escaped = true;
            }
            if fields.iter().any(|(seen, _)| *seen == name) {
                return Err(error(
                    token.span.clone(),
                    format!("The field {name} appears twice"),
                ));
            }
            self.at += 1;
            self.expect(':', "Expected : after the field name")?;
            let (value, _) = self.value()?;
            fields.push((name, value));
            if self.peek().is_none() {
                return Err(error(
                    open.start..self.text.len(),
                    "This document never ends: add }",
                ));
            }
            if !self.peek_punct('}') {
                self.expect(',', "Expected , or } after the value")?;
            }
        }
        let close = self.expect('}', "Expected }")?;
        let span = open.start..close.end;
        let document = Document(fields);
        if escaped || !is_wrapper(&document) {
            return Ok((Value::Object(document), span));
        }
        let json = to_ejson(&Value::Object(document), Ejson::Relaxed);
        let bson = Bson::try_from(json).map_err(|problem| {
            error(span.clone(), format!("Not valid Extended JSON: {problem}"))
        })?;
        Ok((convert::value(&bson), span))
    }

    fn array(&mut self) -> Result<(Value, Range<usize>), ParseError> {
        let open = self.expect('[', "Expected [")?;
        let mut items = Vec::new();
        while !self.peek_punct(']') {
            if self.peek().is_none() {
                return Err(error(
                    open.start..self.text.len(),
                    "This array never ends: add ]",
                ));
            }
            items.push(self.value()?.0);
            if self.peek().is_none() {
                return Err(error(
                    open.start..self.text.len(),
                    "This array never ends: add ]",
                ));
            }
            if !self.peek_punct(']') {
                self.expect(',', "Expected , or ] after the value")?;
            }
        }
        let close = self.expect(']', "Expected ]")?;
        Ok((Value::Array(items), open.start..close.end))
    }

    /// A name: a keyword, or a constructor such as `ObjectId("…")`.
    fn named(
        &mut self,
        name: &str,
        span: Range<usize>,
    ) -> Result<(Value, Range<usize>), ParseError> {
        if self.peek_punct('(') {
            return self.call(name, span);
        }
        let value = match name {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            "null" => Value::Null,
            "undefined" => convert::value(&Bson::Undefined),
            "NaN" => Value::Double(f64::NAN),
            "Infinity" => Value::Double(f64::INFINITY),
            "new" => {
                let Some(Token {
                    kind: Kind::Ident(constructor),
                    span: called,
                }) = self.peek()
                else {
                    return Err(error(
                        self.here(),
                        "Expected a type after new, such as new Date()",
                    ));
                };
                self.at += 1;
                if !self.peek_punct('(') {
                    return Err(error(called.clone(), format!("Write new {constructor}(…)")));
                }
                let (value, called) = self.call(&new(constructor), called.clone())?;
                return Ok((value, span.start..called.end));
            }
            _ => {
                return Err(error(
                    span,
                    format!("{name} is not a value: put text in quotes, such as \"{name}\""),
                ));
            }
        };
        Ok((value, span))
    }

    /// `Name(arguments)`.
    fn call(
        &mut self,
        name: &str,
        span: Range<usize>,
    ) -> Result<(Value, Range<usize>), ParseError> {
        let open = self.expect('(', "Expected (")?;
        let mut args = Vec::new();
        while !self.peek_punct(')') {
            if self.peek().is_none() {
                return Err(error(
                    open.start..self.text.len(),
                    "Expected ) to close the call",
                ));
            }
            args.push(self.value()?);
            if !self.peek_punct(')') {
                self.expect(',', "Expected , or ) after the argument")?;
            }
        }
        let close = self.expect(')', "Expected )")?;
        let span = span.start..close.end;
        let value =
            construct(self.text, name, &args).map_err(|message| error(span.clone(), message))?;
        Ok((value, span))
    }
}

/// The constructor `new X(…)` stands for: `Date` only reads as a date with
/// `new`, as in JavaScript.
fn new(constructor: &str) -> String {
    match constructor {
        "Date" => "new Date".into(),
        other => other.into(),
    }
}

fn is_wrapper(document: &Document) -> bool {
    WRAPPERS.iter().any(|keys| {
        keys.len() == document.0.len()
            && document
                .0
                .iter()
                .all(|(name, _)| keys.contains(&name.as_str()))
    })
}

/// A number's text as a value: `42` is an Int32, an Int64 when it does not
/// fit, and a Double beyond that; with a point or an exponent, a Double.
fn number(text: &str) -> Value {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(digits) => (true, digits),
        None => (false, text),
    };
    if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        return match i128::from_str_radix(hex, 16) {
            Ok(n) => integer(if negative { -n } else { n }),
            Err(_) => Value::Double(f64::INFINITY),
        };
    }
    if digits.contains(['.', 'e', 'E']) {
        return Value::Double(text.parse().unwrap_or(f64::NAN));
    }
    match text.parse::<i128>() {
        Ok(n) => integer(n),
        Err(_) => Value::Double(text.parse().unwrap_or(f64::NAN)),
    }
}

fn integer(n: i128) -> Value {
    if let Ok(n) = i32::try_from(n) {
        Value::Int32(n)
    } else if let Ok(n) = i64::try_from(n) {
        Value::Int64(n)
    } else {
        Value::Double(n as f64)
    }
}

/// A regular expression; its flags are stored sorted, as BSON wants them.
fn regex(pattern: &str, flags: &str) -> Result<Value, String> {
    let mut sorted: Vec<char> = flags.chars().collect();
    sorted.sort_unstable();
    sorted.dedup();
    let regex = Regex {
        pattern: pattern
            .to_string()
            .try_into()
            .map_err(|_| "A regular expression cannot hold a NUL character".to_string())?,
        options: sorted
            .into_iter()
            .collect::<String>()
            .try_into()
            .map_err(|_| "Regular expression flags are letters".to_string())?,
    };
    Ok(convert::value(&Bson::RegularExpression(regex)))
}

/// A constructor's value from its arguments.
fn construct(text: &str, name: &str, args: &[(Value, Range<usize>)]) -> Result<Value, String> {
    let values: Vec<&Value> = args.iter().map(|(value, _)| value).collect();
    let bson = match (name, values.as_slice()) {
        ("ObjectId", []) => Bson::ObjectId(ObjectId::new()),
        ("ObjectId", [Value::String(hex)]) => Bson::ObjectId(
            ObjectId::parse_str(hex.trim())
                .map_err(|_| "An ObjectId is 24 hexadecimal digits".to_string())?,
        ),
        ("ISODate" | "new Date", []) => Bson::DateTime(DateTime::now()),
        ("ISODate" | "new Date", [Value::String(date)]) => Bson::DateTime(DateTime::from_millis(
            iso_millis(date.trim()).ok_or_else(|| {
                format!("{date} is not a date: write it as 2026-10-28T12:00:00Z")
            })?,
        )),
        ("new Date", [millis]) => Bson::DateTime(DateTime::from_millis(
            whole(millis).ok_or("new Date() takes milliseconds since 1970 or a date in quotes")?,
        )),
        ("NumberInt" | "Int32", [n]) => Bson::Int32(
            whole_or_text(n)
                .and_then(|n| i32::try_from(n).ok())
                .ok_or("NumberInt() takes a whole number that fits in 32 bits")?,
        ),
        ("NumberLong" | "Long", [n]) => Bson::Int64(whole_or_text(n).ok_or(
            "NumberLong() takes a whole number; write one beyond 2^53 in quotes: NumberLong(\"9007199254740993\")",
        )?),
        ("NumberDecimal" | "Decimal128", [n]) => {
            let digits = match n {
                Value::String(digits) => digits.trim().to_string(),
                _ => text[args[0].1.clone()]
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect(),
            };
            Bson::Decimal128(
                digits
                    .parse::<Decimal128>()
                    .map_err(|_| "NumberDecimal() takes a number in quotes: NumberDecimal(\"9.99\")".to_string())?,
            )
        }
        ("Double", [n]) => Bson::Double(match n {
            Value::Int32(n) => f64::from(*n),
            Value::Int64(n) => *n as f64,
            Value::Double(x) => *x,
            Value::String(x) => x.trim().parse().map_err(|_| "Double() takes a number".to_string())?,
            _ => return Err("Double() takes a number".into()),
        }),
        ("UUID", []) => Bson::Binary(Binary::from_uuid(bson::Uuid::new())),
        ("UUID", [Value::String(uuid)]) => Bson::Binary(Binary::from_uuid(
            bson::Uuid::parse_str(uuid.trim())
                .map_err(|_| "A UUID is 32 hexadecimal digits, such as 3b241101-e2bb-4255-8caf-4136c566a962".to_string())?,
        )),
        ("BinData", [subtype, Value::String(base64)]) => {
            let subtype = whole(subtype)
                .and_then(|n| u8::try_from(n).ok())
                .ok_or("BinData() takes a subtype from 0 to 255")?;
            let bytes = STANDARD
                .decode(base64.trim())
                .map_err(|_| "BinData() takes its bytes in base64".to_string())?;
            return Ok(Value::Binary { subtype, bytes });
        }
        ("Timestamp", [Value::Object(parts)]) => timestamp(parts.get("t"), parts.get("i"))?,
        ("Timestamp", [time, increment]) => timestamp(Some(time), Some(increment))?,
        ("MinKey", []) => Bson::MinKey,
        ("MaxKey", []) => Bson::MaxKey,
        ("RegExp" | "BSONRegExp", [Value::String(pattern)]) => return regex(pattern, ""),
        ("RegExp" | "BSONRegExp", [Value::String(pattern), Value::String(flags)]) => {
            return regex(pattern, flags);
        }
        ("Code", [Value::String(code)]) => Bson::JavaScriptCode(code.clone()),
        ("Code", [Value::String(code), Value::Object(scope)]) => {
            Bson::JavaScriptCodeWithScope(JavaScriptCodeWithScope {
                code: code.clone(),
                scope: convert::bson_document(scope)?,
            })
        }
        (
            "ObjectId" | "ISODate" | "new Date" | "NumberInt" | "Int32" | "NumberLong" | "Long"
            | "NumberDecimal" | "Decimal128" | "Double" | "UUID" | "BinData" | "Timestamp"
            | "MinKey" | "MaxKey" | "RegExp" | "BSONRegExp" | "Code",
            _,
        ) => return Err(usage(name).to_string()),
        _ => return Err(format!("{name}() is not a type Kuery reads")),
    };
    Ok(convert::value(&bson))
}

/// How a constructor is called, for a call that got it wrong.
fn usage(name: &str) -> &'static str {
    match name {
        "ObjectId" => "Write ObjectId(\"65f0c0ffee0000000000beef\")",
        "ISODate" => "Write ISODate(\"2026-10-28T12:00:00Z\")",
        "new Date" => "Write new Date(\"2026-10-28T12:00:00Z\") or new Date(milliseconds)",
        "NumberInt" | "Int32" => "Write NumberInt(42)",
        "NumberLong" | "Long" => "Write NumberLong(\"9007199254740993\")",
        "NumberDecimal" | "Decimal128" => "Write NumberDecimal(\"9.99\")",
        "Double" => "Write Double(42)",
        "UUID" => "Write UUID(\"3b241101-e2bb-4255-8caf-4136c566a962\")",
        "BinData" => "Write BinData(0, \"base64\")",
        "Timestamp" => "Write Timestamp({t: 1761654896, i: 1})",
        "MinKey" => "Write MinKey()",
        "MaxKey" => "Write MaxKey()",
        "Code" => "Write Code(\"function () {…}\")",
        _ => "Write RegExp(\"pattern\", \"flags\")",
    }
}

fn timestamp(time: Option<&Value>, increment: Option<&Value>) -> Result<Bson, String> {
    let part = |value: Option<&Value>| value.and_then(whole).and_then(|n| u32::try_from(n).ok());
    match (part(time), part(increment)) {
        (Some(time), Some(increment)) => Ok(Bson::Timestamp(Timestamp { time, increment })),
        _ => Err(
            "Timestamp() takes seconds and an increment: Timestamp({t: 1761654896, i: 1})".into(),
        ),
    }
}

/// A whole number, from a number that holds one exactly.
fn whole(value: &Value) -> Option<i64> {
    match value {
        Value::Int32(n) => Some(i64::from(*n)),
        Value::Int64(n) => Some(*n),
        Value::Double(x) if x.fract() == 0.0 && x.abs() <= 9_007_199_254_740_992.0 => {
            Some(*x as i64)
        }
        _ => None,
    }
}

fn whole_or_text(value: &Value) -> Option<i64> {
    match value {
        Value::String(text) => text.trim().parse().ok(),
        other => whole(other),
    }
}

/// Milliseconds since 1970 of an ISO 8601 date: `2026-10-28`, with a time
/// (`T12:00`, `T12:00:00.123`), and a zone (`Z`, `+02:00`); without a zone
/// it is UTC, as in mongosh.
fn iso_millis(text: &str) -> Option<i64> {
    let mut iso = Iso {
        bytes: text.as_bytes(),
        at: 0,
    };
    let year = iso.digits(4)?;
    iso.eat(b'-').then_some(())?;
    let month = iso.digits(2)?;
    iso.eat(b'-').then_some(())?;
    let day = iso.digits(2)?;
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    let mut millis = days_from_civil(year, month, day) * 86_400_000;
    if iso.done() {
        return Some(millis);
    }
    (iso.eat(b'T') || iso.eat(b't') || iso.eat(b' ')).then_some(())?;
    let hour = iso.digits(2)?;
    iso.eat(b':').then_some(())?;
    let minute = iso.digits(2)?;
    let (mut second, mut fraction) = (0, 0);
    if iso.eat(b':') {
        second = iso.digits(2)?;
        if iso.eat(b'.') {
            let start = iso.at;
            while iso.bytes.get(iso.at).is_some_and(u8::is_ascii_digit) {
                iso.at += 1;
            }
            let digits = std::str::from_utf8(&iso.bytes[start..iso.at]).ok()?;
            if digits.is_empty() {
                return None;
            }
            fraction = format!("{digits:0<3}")[..3].parse().ok()?;
        }
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    millis += hour * 3_600_000 + minute * 60_000 + second * 1000 + fraction;
    if iso.eat(b'Z') || iso.eat(b'z') || iso.done() {
        return iso.done().then_some(millis);
    }
    let sign = match (iso.eat(b'+'), iso.eat(b'-')) {
        (true, _) => 1,
        (_, true) => -1,
        _ => return None,
    };
    let hours = iso.digits(2)?;
    iso.eat(b':');
    let minutes = match iso.done() {
        true => 0,
        false => iso.digits(2)?,
    };
    iso.done()
        .then_some(millis - sign * (hours * 3_600_000 + minutes * 60_000))
}

/// The text of an ISO date, read left to right.
struct Iso<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Iso<'_> {
    fn digits(&mut self, count: usize) -> Option<i64> {
        let part = self.bytes.get(self.at..self.at + count)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        self.at += count;
        std::str::from_utf8(part).ok()?.parse().ok()
    }

    fn eat(&mut self, byte: u8) -> bool {
        let found = self.bytes.get(self.at) == Some(&byte);
        if found {
            self.at += 1;
        }
        found
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to a calendar date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

// Printer ---------------------------------------------------------------

/// The first and last millisecond `ISODate("…")` prints: years 0 to 9999.
const ISO_DATES: std::ops::RangeInclusive<i64> = -62_167_219_200_000..=253_402_300_799_999;

fn write_value(out: &mut String, value: &Value, style: Style, depth: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Value::Int32(n) => out.push_str(&n.to_string()),
        Value::Int64(n) => out.push_str(&format!("NumberLong(\"{n}\")")),
        Value::Double(x) => out.push_str(&double(*x)),
        Value::Decimal(text) => out.push_str(&format!("NumberDecimal({})", quote(text))),
        Value::String(text) => out.push_str(&quote(text)),
        Value::Binary { subtype: 4, bytes } if bytes.len() == 16 => {
            out.push_str(&format!("UUID(\"{}\")", uuid(bytes)));
        }
        Value::Binary { subtype, bytes } => {
            out.push_str(&format!(
                "BinData({subtype}, \"{}\")",
                STANDARD.encode(bytes)
            ));
        }
        Value::DateTime(millis) if ISO_DATES.contains(millis) => {
            out.push_str(&format!("ISODate(\"{}\")", iso_date(*millis)));
        }
        Value::DateTime(millis) => out.push_str(&format!("new Date({millis})")),
        Value::Array(items) => {
            let items = items.iter().map(|item| (None, item));
            write_list(out, ('[', ']'), items, style, depth);
        }
        Value::Object(document) => {
            // Its keys would read as a wrapper's: their `$` is escaped.
            let lookalike = is_wrapper(document);
            let fields = document.0.iter().map(|(name, value)| {
                let key = match lookalike && name.starts_with('$') {
                    true => format!("\"\\u0024{}", &quote(&name[1..])[1..]),
                    false => key(name),
                };
                (Some(key), value)
            });
            write_list(out, ('{', '}'), fields, style, depth);
        }
        Value::Ext(ext) => out.push_str(&ext.display),
    }
}

fn write_list<'v>(
    out: &mut String,
    (open, close): (char, char),
    items: impl ExactSizeIterator<Item = (Option<String>, &'v Value)>,
    style: Style,
    depth: usize,
) {
    out.push(open);
    if items.len() == 0 {
        out.push(close);
        return;
    }
    for (i, (name, value)) in items.enumerate() {
        match (style, i) {
            (Style::Compact, 0) => {}
            (Style::Compact, _) => out.push_str(", "),
            (Style::Pretty, 0) => out.push('\n'),
            (Style::Pretty, _) => out.push_str(",\n"),
        }
        if style == Style::Pretty {
            out.push_str(&"  ".repeat(depth + 1));
        }
        if let Some(key) = name {
            out.push_str(&key);
            out.push_str(": ");
        }
        write_value(out, value, style, depth + 1);
    }
    if style == Style::Pretty {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
    }
    out.push(close);
}

/// A field name as a key, bare when it can be: `status`, `"ship.city"`.
pub fn key(name: &str) -> String {
    let mut chars = name.chars();
    let bare = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    match bare {
        true => name.to_string(),
        false => quote(name),
    }
}

/// Text in double quotes, escaped so it reads back the same.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c < ' ' || c == '\u{7f}' || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `42.0`, `1e300`, `NaN`, `-Infinity`: always reads back as a Double.
fn double(x: f64) -> String {
    match x {
        x if x.is_nan() => "NaN".into(),
        f64::INFINITY => "Infinity".into(),
        f64::NEG_INFINITY => "-Infinity".into(),
        x => format!("{x:?}"),
    }
}

fn uuid(bytes: &[u8]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// How a type only MongoDB has reads, as the literal that gives it back.
pub(crate) fn ext_display(bson: &Bson) -> String {
    match bson {
        Bson::ObjectId(id) => format!("ObjectId(\"{}\")", id.to_hex()),
        Bson::Timestamp(Timestamp { time, increment }) => {
            format!("Timestamp({{t: {time}, i: {increment}}})")
        }
        Bson::RegularExpression(regex) => {
            let (pattern, flags) = (regex.pattern.as_str(), regex.options.as_str());
            match regex_literal(pattern, flags) {
                true => format!("/{pattern}/{flags}"),
                false if flags.is_empty() => format!("RegExp({})", quote(pattern)),
                false => format!("RegExp({}, {})", quote(pattern), quote(flags)),
            }
        }
        Bson::MinKey => "MinKey()".into(),
        Bson::MaxKey => "MaxKey()".into(),
        Bson::JavaScriptCode(code) => format!("Code({})", quote(code)),
        Bson::JavaScriptCodeWithScope(JavaScriptCodeWithScope { code, scope }) => format!(
            "Code({}, {})",
            quote(code),
            render(&Value::Object(convert::document(scope)), Style::Compact)
        ),
        // Symbol, DBPointer, Undefined: as their Extended JSON, which is
        // also what mongosh reads them from.
        other => {
            let mut out = String::new();
            write_json(&mut out, &other.clone().into_canonical_extjson());
            out
        }
    }
}

/// Whether `/pattern/flags` reads back as this pattern and these flags.
fn regex_literal(pattern: &str, flags: &str) -> bool {
    if pattern.is_empty()
        || pattern.starts_with('*')
        || !flags.chars().all(|c| c.is_ascii_alphabetic())
    {
        return false;
    }
    let text = format!("/{pattern}/{flags}");
    matches!(
        lex(&text).as_deref(),
        Ok([Token { kind: Kind::Regex { pattern: read, flags: read_flags }, .. }])
            if read == pattern && read_flags == flags
    )
}

fn write_json(out: &mut String, json: &serde_json::Value) {
    match json {
        serde_json::Value::Object(fields) => {
            out.push('{');
            for (i, (name, value)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&key(name));
                out.push_str(": ");
                write_json(out, value);
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_json(out, item);
            }
            out.push(']');
        }
        serde_json::Value::String(text) => out.push_str(&quote(text)),
        other => out.push_str(&other.to_string()),
    }
}
