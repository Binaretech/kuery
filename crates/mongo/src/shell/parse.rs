//! Reads a script into statements and each statement into an [`Op`], with
//! the literal grammar of [`crate::literal`] for the arguments.

use std::ops::Range;

use kuery_core::document::language::ParseError;
use kuery_core::document::value::{CollectionRef, Document, Value};

use crate::literal::{Kind, Reader, Token, error, lex};
use crate::shell::{Arg, Cursor, DbMethod, JAVASCRIPT, Method, Op, Show};

/// The statements of a script, each with the bytes it spans. Comments and
/// empty statements are left out.
pub fn statements(text: &str) -> Result<Vec<(Range<usize>, Op)>, ParseError> {
    let tokens = lex(text)?;
    split(text, &tokens)?
        .into_iter()
        .map(|group| {
            let tokens = &tokens[group];
            let span = tokens[0].span.start..tokens[tokens.len() - 1].span.end;
            let op = Parser::new(&text[..span.end], tokens).op()?;
            Ok((span, op))
        })
        .collect()
}

/// Groups the tokens by statement: they end at `;`, and at a line end
/// outside brackets unless the next line starts with `.` (or the line ends
/// with one), which goes on with a cursor's chain.
pub(crate) fn split(text: &str, tokens: &[Token]) -> Result<Vec<Range<usize>>, ParseError> {
    let mut groups = Vec::new();
    let mut open: Vec<char> = Vec::new();
    let mut start = 0;
    for (i, token) in tokens.iter().enumerate() {
        if i > start && open.is_empty() && ends_line(text, &tokens[i - 1], token) {
            groups.push(start..i);
            start = i;
        }
        match token.kind {
            Kind::Punct(c @ ('(' | '[' | '{')) => open.push(c),
            Kind::Punct(c @ (')' | ']' | '}')) => {
                let opener = match c {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                if open.pop() != Some(opener) {
                    return Err(error(token.span.clone(), format!("Unexpected {c}")));
                }
            }
            Kind::Punct(';') if open.is_empty() => {
                if i > start {
                    groups.push(start..i);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < tokens.len() {
        groups.push(start..tokens.len());
    }
    Ok(groups)
}

/// Whether a line ends between two tokens and with it the statement.
pub(crate) fn ends_line(text: &str, before: &Token, after: &Token) -> bool {
    let dot = |token: &Token| token.kind == Kind::Punct('.');
    text[before.span.end..after.span.start].contains('\n') && !dot(before) && !dot(after)
}

/// Whether MongoDB takes `name` for a database: not empty, shorter than
/// 64 bytes, and none of `/\. "$` or NUL, which the server refuses on
/// every platform (Windows also refuses `*<>:|?`, which only its own
/// server checks).
fn database_name(name: &str) -> bool {
    !name.is_empty() && name.len() < 64 && !name.contains(['/', '\\', '.', ' ', '"', '$', '\0'])
}

fn javascript(span: Range<usize>) -> ParseError {
    error(span, JAVASCRIPT)
}

/// Reads one statement's tokens.
struct Parser<'a> {
    reader: Reader<'a>,
    tokens: &'a [Token],
}

impl<'a> Parser<'a> {
    fn new(text: &'a str, tokens: &'a [Token]) -> Self {
        Parser {
            reader: Reader::new(text, tokens),
            tokens,
        }
    }

    fn op(mut self) -> Result<Op, ParseError> {
        let first = &self.tokens[0];
        let command = matches!(&first.kind, Kind::Ident(word) if ["use", "show", "db"].contains(&word.as_str()));
        if !command {
            return Err(javascript(first.span.clone()));
        }
        self.refuse_javascript()?;
        let op = match &first.kind {
            Kind::Ident(word) if word == "use" => return self.use_database(),
            Kind::Ident(word) if word == "show" => self.show()?,
            _ => {
                self.reader.at += 1;
                self.database()?
            }
        };
        match self.reader.peek() {
            None => Ok(op),
            Some(token) => Err(javascript(token.span.clone())),
        }
    }

    /// Functions, arrows and assignments are JavaScript wherever they are.
    fn refuse_javascript(&self) -> Result<(), ParseError> {
        for (i, token) in self.tokens.iter().enumerate() {
            let next = self.tokens.get(i + 1).map(|token| &token.kind);
            let function = token.kind == Kind::Ident("function".into())
                && matches!(next, Some(Kind::Punct('(') | Kind::Ident(_)));
            if function || token.kind == Kind::Punct('=') {
                return Err(javascript(token.span.clone()));
            }
        }
        Ok(())
    }

    fn next_ident(&mut self, message: &str) -> Result<(String, Range<usize>), ParseError> {
        match self.reader.peek() {
            Some(Token {
                kind: Kind::Ident(name),
                span,
            }) => {
                self.reader.at += 1;
                Ok((name.clone(), span.clone()))
            }
            _ => Err(error(self.reader.here(), message)),
        }
    }

    /// `use shop`: the rest of the statement is the name.
    fn use_database(self) -> Result<Op, ParseError> {
        let first = &self.tokens[0];
        let rest = &self.tokens[1..];
        let (Some(head), Some(tail)) = (rest.first(), rest.last()) else {
            return Err(error(first.span.clone(), "Name the database: use shop"));
        };
        let span = head.span.start..tail.span.end;
        let (name, quoted) = match (&head.kind, rest.len()) {
            (Kind::Str(name), 1) => (name.clone(), true),
            _ => (self.reader_text(span.clone()).to_string(), false),
        };
        // Unquoted, the name is the text as written: no punctuation that
        // reads as code, which quotes allow.
        let unquoted = |c: char| c.is_whitespace() || "*<>:|?()[]{};,'".contains(c);
        if !database_name(&name) || (!quoted && name.contains(unquoted)) {
            return Err(error(span, format!("{name} is not a database name")));
        }
        Ok(Op::Use(name))
    }

    fn reader_text(&self, span: Range<usize>) -> &str {
        self.reader.text_at(span)
    }

    fn show(&mut self) -> Result<Op, ParseError> {
        self.reader.at += 1;
        let (what, span) = self.next_ident("Write show dbs or show collections")?;
        match what.as_str() {
            "dbs" | "databases" => Ok(Op::Show(Show::Databases)),
            "collections" | "tables" => Ok(Op::Show(Show::Collections)),
            _ => Err(error(
                span,
                format!("Kuery shows dbs and collections, not {what}"),
            )),
        }
    }

    /// After `db`: `.getSiblingDB("x")`, then a collection, a database
    /// method or a command.
    fn database(&mut self) -> Result<Op, ParseError> {
        let mut database = None;
        loop {
            if self.reader.peek_punct('[') {
                self.reader.at += 1;
                let name = self.name_argument("Write db[\"orders\"]")?;
                self.reader
                    .expect(']', "Expected ] after the collection's name")?;
                return self.collection(database, name);
            }
            self.reader.expect('.', "Expected . after db")?;
            let (name, span) = self.next_ident("Expected a collection or a method after db.")?;
            if !self.reader.peek_punct('(') {
                return self.collection(database, name);
            }
            let args = self.arguments()?;
            let call = span.start..self.reader.last_end();
            match name.as_str() {
                "getSiblingDB" => {
                    database = Some(single_string(&name, args, call)?);
                    if self.reader.peek().is_none() {
                        return Err(error(
                            self.reader.here(),
                            "Name a collection after getSiblingDB(…), such as .orders.find()",
                        ));
                    }
                }
                "getCollection" => {
                    let collection = single_string(&name, args, call)?;
                    return self.methods(database, collection);
                }
                "runCommand" | "adminCommand" => {
                    let command = command_argument(&name, args, call)?;
                    return Ok(Op::RunCommand {
                        database,
                        admin: name == "adminCommand",
                        command,
                    });
                }
                "eval" | "loadServerScripts" => return Err(javascript(span)),
                _ => {
                    let Some(method) = DbMethod::named(&name) else {
                        return Err(error(
                            span,
                            format!("db.{name}() is not a command Kuery runs"),
                        ));
                    };
                    let args = checked(method.name(), method.args(), args, call)?;
                    return Ok(Op::Database {
                        database,
                        method,
                        args,
                    });
                }
            }
        }
    }

    /// A string in brackets or a call: a collection's or a database's name.
    fn name_argument(&mut self, message: &str) -> Result<String, ParseError> {
        match self.reader.peek() {
            Some(Token {
                kind: Kind::Str(name),
                ..
            }) => {
                self.reader.at += 1;
                Ok(name.clone())
            }
            _ => Err(error(self.reader.here(), message)),
        }
    }

    /// `db.orders`, or `db.system.views`: names joined by dots until one is
    /// called.
    fn collection(&mut self, database: Option<String>, name: String) -> Result<Op, ParseError> {
        let mut name = name;
        while self.reader.peek_punct('.') {
            let more = self.tokens.get(self.reader.at + 1);
            let called = self
                .tokens
                .get(self.reader.at + 2)
                .is_some_and(|token| token.kind == Kind::Punct('('));
            match more {
                Some(Token {
                    kind: Kind::Ident(part),
                    ..
                }) if !called => {
                    name.push('.');
                    name.push_str(part);
                    self.reader.at += 2;
                }
                _ => break,
            }
        }
        self.methods(database, name)
    }

    /// A collection's method and its chain.
    fn methods(&mut self, database: Option<String>, collection: String) -> Result<Op, ParseError> {
        if self.reader.peek().is_none() {
            return Err(error(
                self.reader.here(),
                format!("Call a method of the collection, such as db.{collection}.find()"),
            ));
        }
        self.reader.expect('.', "Expected . and a method")?;
        let (mut name, mut span) = self.next_ident("Expected a method, such as find")?;
        // `db.orders.explain().find(…)`: explained as `.explain()` after it.
        let mut explain = None;
        if name == "explain" && self.reader.peek_punct('(') {
            let args = self.arguments()?;
            let call = span.start..self.reader.last_end();
            explain = Some(checked("explain", Cursor::Explain.args(), args, call)?);
            self.reader
                .expect('.', "Expected the method to explain, such as .find()")?;
            (name, span) = self.next_ident("Expected the method to explain, such as find")?;
        }
        if name == "mapReduce" {
            return Err(javascript(span));
        }
        let Some(method) = Method::named(&name) else {
            return Err(error(
                span,
                format!("{name} is not a collection method Kuery runs"),
            ));
        };
        if !self.reader.peek_punct('(') {
            return Err(error(self.reader.here(), format!("Write {name}(…)")));
        }
        let args = self.arguments()?;
        let call = span.start..self.reader.last_end();
        let args = checked(method.name(), method.args(), args, call)?;
        let mut chain = self.chain(method)?;
        if let Some(args) = explain {
            if chain.iter().any(|(cursor, _)| *cursor == Cursor::Explain) {
                return Err(error(span, "Explain it once"));
            }
            if !method.explainable() {
                return Err(error(span, format!("{name} cannot be explained")));
            }
            chain.push((Cursor::Explain, args));
        }
        let path = match database {
            Some(database) => vec![database, collection],
            None => vec![collection],
        };
        Ok(Op::Collection {
            target: CollectionRef { path },
            method,
            args,
            chain,
        })
    }

    /// `.sort(…).limit(…)`, then perhaps `.explain()`.
    fn chain(&mut self, method: Method) -> Result<Vec<(Cursor, Vec<Value>)>, ParseError> {
        let mut chain: Vec<(Cursor, Vec<Value>)> = Vec::new();
        while self.reader.peek_punct('.') {
            self.reader.at += 1;
            let (name, span) = self.next_ident("Expected a cursor method, such as sort")?;
            if !self.reader.peek_punct('(') {
                return Err(error(self.reader.here(), format!("Write {name}(…)")));
            }
            if matches!(name.as_str(), "forEach" | "map" | "filter" | "reduce") {
                return Err(javascript(span));
            }
            let args = self.arguments()?;
            let call = span.start..self.reader.last_end();
            // What mongosh prints a cursor with: nothing to run.
            if matches!(name.as_str(), "pretty" | "toArray") && args.is_empty() {
                continue;
            }
            let Some(cursor) = Cursor::named(&name) else {
                return Err(error(
                    span,
                    format!(".{name}() is not a cursor method Kuery runs"),
                ));
            };
            if chain.iter().any(|(cursor, _)| *cursor == Cursor::Explain) {
                return Err(error(span, "explain() comes last"));
            }
            match cursor {
                Cursor::Explain if !method.explainable() => {
                    return Err(error(
                        span,
                        format!("{} cannot be explained", method.name()),
                    ));
                }
                Cursor::Explain => {}
                _ if !method.has_cursor() => {
                    return Err(error(
                        span,
                        format!("{} takes no .{name}(): pass it as an option", method.name()),
                    ));
                }
                _ => {}
            }
            chain.push((cursor, checked(cursor.name(), cursor.args(), args, call)?));
        }
        Ok(chain)
    }

    /// `(a, b, …)`, each with its span.
    fn arguments(&mut self) -> Result<Vec<(Value, Range<usize>)>, ParseError> {
        let open = self.reader.expect('(', "Expected (")?;
        let mut args = Vec::new();
        while !self.reader.peek_punct(')') {
            if self.reader.peek().is_none() {
                return Err(error(
                    open.start..self.reader.here().end,
                    "Expected ) to close the call",
                ));
            }
            args.push(self.reader.value()?);
            if self.reader.peek().is_none() {
                return Err(error(
                    open.start..self.reader.here().end,
                    "Expected ) to close the call",
                ));
            }
            if !self.reader.peek_punct(')') {
                self.reader
                    .expect(',', "Expected , or ) after the argument")?;
            }
        }
        self.reader.expect(')', "Expected )")?;
        Ok(args)
    }
}

/// The arguments as the method takes them.
fn checked(
    name: &str,
    (needed, shape): (usize, &[Arg]),
    args: Vec<(Value, Range<usize>)>,
    call: Range<usize>,
) -> Result<Vec<Value>, ParseError> {
    if args.len() < needed {
        let some = match needed {
            1 => "an argument".to_string(),
            n => format!("{n} arguments"),
        };
        return Err(error(call, format!("{name}() needs {some}")));
    }
    if let Some((_, span)) = args.get(shape.len()) {
        let most = match shape.len() {
            0 => "no arguments".to_string(),
            1 => "one argument".to_string(),
            n => format!("{n} arguments at most"),
        };
        return Err(error(span.clone(), format!("{name}() takes {most}")));
    }
    args.into_iter()
        .zip(shape)
        .enumerate()
        .map(|(i, ((value, span), arg))| {
            let fits = match arg {
                Arg::Document => matches!(value, Value::Object(_)),
                Arg::Array => matches!(value, Value::Array(_)),
                Arg::String => matches!(value, Value::String(_)),
                Arg::Number => {
                    matches!(value, Value::Int32(_) | Value::Int64(_) | Value::Double(_))
                }
                Arg::Any => true,
            };
            match fits {
                true => Ok(value),
                false => Err(error(
                    span,
                    format!("{name}()'s {} argument is {}", ordinal(i), expected(*arg)),
                )),
            }
        })
        .collect()
}

fn ordinal(i: usize) -> &'static str {
    ["first", "second", "third", "fourth"]
        .get(i)
        .copied()
        .unwrap_or("next")
}

fn expected(arg: Arg) -> &'static str {
    match arg {
        Arg::Document => "a document, such as {status: \"paid\"}",
        Arg::Array => "an array, such as [{$match: {}}]",
        Arg::String => "a name in quotes",
        Arg::Number => "a number",
        Arg::Any => "a value",
    }
}

fn single_string(
    name: &str,
    args: Vec<(Value, Range<usize>)>,
    call: Range<usize>,
) -> Result<String, ParseError> {
    match checked(name, (1, &[Arg::String]), args, call)?.pop() {
        Some(Value::String(text)) => Ok(text),
        _ => unreachable!("checked as a string"),
    }
}

/// `{ping: 1}`, or `"ping"` for it.
fn command_argument(
    name: &str,
    args: Vec<(Value, Range<usize>)>,
    call: Range<usize>,
) -> Result<Document, ParseError> {
    let span = args.first().map_or(call.clone(), |(_, span)| span.clone());
    match checked(name, (1, &[Arg::Any]), args, call)?.pop() {
        Some(Value::Object(command)) if !command.0.is_empty() => Ok(command),
        Some(Value::String(command)) if !command.is_empty() => {
            Ok(Document(vec![(command, Value::Int32(1))]))
        }
        _ => Err(error(
            span,
            format!("{name}() takes a command, such as {{ping: 1}}"),
        )),
    }
}
