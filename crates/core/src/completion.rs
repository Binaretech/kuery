//! Autocompletion for the SQL editor: keywords, tables and views after
//! `FROM`/`JOIN`, and the columns of the tables the statement mentions,
//! through their names or aliases.
//!
//! The text is usually unfinished SQL, so it is not parsed: a small lexer
//! finds the statement under the caret, the word being typed, what qualifies
//! it (`o.` or `sales.`) and the clause it is in. The tables a statement
//! reads come from the whole statement, so `select o.| from orders o` knows
//! what `o` is.

use std::ops::Range;

use crate::catalog::{CatalogColumn, CatalogOverview, TableKind, TableRef};
use crate::sql::Dialect;

/// What completion knows about a database.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    /// Schemas in `search_path` order: their tables are written unqualified.
    pub search_path: Vec<String>,
    pub schemas: Vec<String>,
    pub tables: Vec<Table>,
    /// What quotes identifiers that need it (`"`, or `` ` `` in MySQL).
    pub quote: char,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            search_path: Vec::new(),
            schemas: Vec::new(),
            tables: Vec::new(),
            quote: '"',
        }
    }
}

impl Catalog {
    /// From a catalog's overview and every column, in `dialect`.
    pub fn compose(
        search_path: Vec<String>,
        overview: &CatalogOverview,
        columns: Vec<CatalogColumn>,
        dialect: &Dialect,
    ) -> Self {
        let mut tables: Vec<Table> = overview
            .tables
            .iter()
            .map(|info| Table {
                table: info.table.clone(),
                kind: info.kind,
                columns: Vec::new(),
            })
            .collect();
        for column in columns {
            if let Some(table) = tables.iter_mut().find(|t| t.table == column.table) {
                table.columns.push(Column {
                    name: column.name,
                    data_type: column.data_type,
                });
            }
        }
        Self {
            search_path,
            schemas: overview.schemas.clone(),
            tables,
            quote: dialect.ident_quote,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub table: TableRef,
    pub kind: TableKind,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    pub data_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    Keyword,
    Schema,
    Table,
    View,
    Alias,
    Column,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub label: String,
    /// Text that replaces [`Completions::replace`].
    pub insert: String,
    pub kind: SuggestionKind,
    /// Short description, e.g. the column type.
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completions {
    /// Byte range of the word being typed, including an opening `"`.
    pub replace: Range<usize>,
    /// The word being typed, without quotes.
    pub prefix: String,
    pub suggestions: Vec<Suggestion>,
}

/// Enough for any menu; a big catalog would otherwise list thousands.
const MAX_SUGGESTIONS: usize = 200;

/// Suggestions for the caret at byte `offset` of `sql`. `invoked` is true
/// when the user asked for them; otherwise they come while typing, and only
/// once there is a word, a `.` or a place for a table name.
pub fn complete(sql: &str, offset: usize, catalog: &Catalog, invoked: bool) -> Completions {
    let offset = offset.min(sql.len());
    let empty = Completions {
        replace: offset..offset,
        ..Completions::default()
    };
    let tokens = tokenize(sql);
    if tokens
        .iter()
        .any(|t| matches!(t.kind, Kind::Text | Kind::Comment) && t.contains(offset))
    {
        return empty;
    }

    let statement = statement_at(&tokens, offset);
    let word = statement
        .iter()
        .position(|t| matches!(t.kind, Kind::Word | Kind::Quoted) && t.contains(offset));
    let (replace, prefix, quoted) = match word {
        Some(index) => {
            let token = statement[index];
            match token.kind {
                Kind::Quoted => {
                    let prefix = sql[token.start + 1..offset].to_owned();
                    (token.start..token.end.max(offset), prefix, true)
                }
                _ => (
                    token.start..offset,
                    sql[token.start..offset].to_owned(),
                    false,
                ),
            }
        }
        None => (offset..offset, String::new(), false),
    };
    // A number or an operator under the caret is not a word to complete.
    if word.is_none()
        && statement
            .iter()
            .any(|t| t.start < offset && offset <= t.end && t.kind == Kind::Number)
    {
        return empty;
    }

    let before: Vec<Token> = statement
        .iter()
        .copied()
        .filter(|t| t.end <= replace.start)
        .collect();
    let (head, qualifier) = split_qualifier(sql, &before);
    let references = references(sql, statement, word);
    let context = context(sql, head);

    if !invoked && prefix.is_empty() && qualifier.is_empty() && context != Context::Table {
        return empty;
    }

    let mut builder = Builder {
        prefix: &prefix,
        quote: catalog.quote,
        quoted,
        suggestions: Vec::new(),
    };
    let style = keyword_style(sql, &prefix, statement);
    match qualifier.as_slice() {
        [] => match context {
            Context::None => {}
            Context::Start => builder.keywords(START_KEYWORDS, style),
            Context::Table => builder.tables(catalog),
            Context::AfterTable => builder.keywords(AFTER_TABLE_KEYWORDS, style),
            Context::Expression => {
                builder.columns(catalog, &references);
                builder.keywords(EXPRESSION_KEYWORDS, style);
            }
        },
        [name] => {
            if let Some(table) = qualified_table(catalog, &references, name) {
                builder.columns_of(table);
            } else if catalog.schemas.iter().any(|s| s == name) {
                builder.tables_in(catalog, name);
            }
        }
        [schema, name] => {
            if let Some(table) = find_table(catalog, Some(schema), name) {
                builder.columns_of(table);
            }
        }
        _ => {}
    }
    let mut suggestions = builder.suggestions;
    suggestions.truncate(MAX_SUGGESTIONS);
    Completions {
        replace,
        prefix,
        suggestions,
    }
}

// --- Lexer ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    /// `"quoted identifier"`.
    Quoted,
    Number,
    /// String literal, including dollar-quoted.
    Text,
    Comment,
    Punct(char),
}

#[derive(Debug, Clone, Copy)]
struct Token {
    kind: Kind,
    start: usize,
    end: usize,
    /// Unterminated (or a line comment): a caret at `end` is still inside.
    open: bool,
}

impl Token {
    fn contains(&self, offset: usize) -> bool {
        self.start < offset && (offset < self.end || (offset == self.end && self.open))
            || (offset == self.end && matches!(self.kind, Kind::Word | Kind::Quoted))
    }
}

fn tokenize(sql: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut i = 0;
    while let Some(ch) = sql[i..].chars().next() {
        let rest = &sql[i..];
        if ch.is_whitespace() {
            i += ch.len_utf8();
            continue;
        }
        let (kind, end, open) = if rest.starts_with("--") {
            let end = rest.find('\n').map_or(sql.len(), |n| i + n);
            (Kind::Comment, end, true)
        } else if rest.starts_with("/*") {
            let (end, open) = block_comment_end(sql, i);
            (Kind::Comment, end, open)
        } else if ch == '\'' {
            let (end, open) = quoted_end(sql, i, '\'');
            (Kind::Text, end, open)
        } else if ch == '"' {
            let (end, open) = quoted_end(sql, i, '"');
            (Kind::Quoted, end, open)
        } else if let Some(tag) = dollar_tag(rest) {
            match sql[i + tag.len()..].find(tag) {
                Some(n) => (Kind::Text, i + tag.len() + n + tag.len(), false),
                None => (Kind::Text, sql.len(), true),
            }
        } else if ch.is_alphabetic() || ch == '_' {
            let len: usize = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                .map(char::len_utf8)
                .sum();
            (Kind::Word, i + len, false)
        } else if ch.is_ascii_digit() {
            let len = rest
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'_')
                .count();
            (Kind::Number, i + len, false)
        } else {
            (Kind::Punct(ch), i + ch.len_utf8(), false)
        };
        tokens.push(Token {
            kind,
            start: i,
            end,
            open,
        });
        i = end;
    }
    tokens
}

/// End of the literal or quoted identifier starting at `start`; a doubled
/// quote is an escaped one.
fn quoted_end(sql: &str, start: usize, quote: char) -> (usize, bool) {
    let mut chars = sql[start + 1..].char_indices().peekable();
    while let Some((n, ch)) = chars.next() {
        if ch == quote {
            if chars.peek().map(|&(_, next)| next) == Some(quote) {
                chars.next();
            } else {
                return (start + 1 + n + 1, false);
            }
        }
    }
    (sql.len(), true)
}

/// Block comments nest in PostgreSQL.
fn block_comment_end(sql: &str, start: usize) -> (usize, bool) {
    let mut depth = 0;
    let mut i = start;
    while i < sql.len() {
        let rest = &sql[i..];
        if rest.starts_with("/*") {
            depth += 1;
            i += 2;
        } else if rest.starts_with("*/") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return (i, false);
            }
        } else {
            i += rest.chars().next().map_or(1, char::len_utf8);
        }
    }
    (sql.len(), true)
}

/// `$$` or `$tag$` at the start of `rest`. `$1` is a parameter, not a tag.
fn dollar_tag(rest: &str) -> Option<&str> {
    let body = rest.strip_prefix('$')?;
    let len = body
        .char_indices()
        .take_while(|&(n, c)| c == '_' || c.is_alphabetic() || (n > 0 && c.is_alphanumeric()))
        .map(|(_, c)| c.len_utf8())
        .sum::<usize>();
    body[len..].starts_with('$').then(|| &rest[..len + 2])
}

/// Tokens of the statement around `offset`.
fn statement_at(tokens: &[Token], offset: usize) -> &[Token] {
    let is_semicolon = |t: &Token| t.kind == Kind::Punct(';');
    let start = tokens
        .iter()
        .rposition(|t| is_semicolon(t) && t.end <= offset)
        .map_or(0, |n| n + 1);
    let end = tokens[start..]
        .iter()
        .position(|t| is_semicolon(t) && t.start >= offset)
        .map_or(tokens.len(), |n| start + n);
    &tokens[start..end]
}

fn word<'a>(sql: &'a str, token: &Token) -> Option<&'a str> {
    (token.kind == Kind::Word).then(|| &sql[token.start..token.end])
}

fn is_keyword(sql: &str, token: &Token, keywords: &[&str]) -> bool {
    word(sql, token).is_some_and(|w| keywords.iter().any(|k| w.eq_ignore_ascii_case(k)))
}

/// An identifier as PostgreSQL reads it: unquoted ones fold to lowercase.
fn identifier(sql: &str, token: &Token) -> Option<String> {
    match token.kind {
        Kind::Word => Some(sql[token.start..token.end].to_lowercase()),
        Kind::Quoted if !token.open => {
            Some(sql[token.start + 1..token.end - 1].replace("\"\"", "\""))
        }
        _ => None,
    }
}

// --- Context -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    /// Nothing worth suggesting, e.g. right after `AS`.
    None,
    /// Start of a statement.
    Start,
    /// A table name goes here: after `FROM`, `JOIN`, `UPDATE`, `INTO`…
    Table,
    /// After a table in `FROM`: `WHERE`, `JOIN`, `ORDER BY`…
    AfterTable,
    /// An expression: columns.
    Expression,
}

/// `before` without the trailing `a.` or `a.b.` that qualifies the word
/// being typed, and those names.
fn split_qualifier<'a>(sql: &str, before: &'a [Token]) -> (&'a [Token], Vec<String>) {
    let mut end = before.len();
    let mut names = Vec::new();
    while end >= 2 && before[end - 1].kind == Kind::Punct('.') {
        let Some(name) = identifier(sql, &before[end - 2]) else {
            break;
        };
        names.insert(0, name);
        end -= 2;
    }
    (&before[..end], names)
}

/// Clause keywords: the last one before the caret says what goes there.
const CLAUSES: &[&str] = &[
    "select",
    "from",
    "join",
    "on",
    "where",
    "by",
    "having",
    "limit",
    "offset",
    "update",
    "set",
    "into",
    "values",
    "returning",
    "using",
    "with",
    "union",
    "except",
    "intersect",
    "delete",
    "insert",
    "truncate",
    "table",
    "window",
    "when",
    "then",
    "else",
    "and",
    "or",
    "not",
];

fn context(sql: &str, head: &[Token]) -> Context {
    let Some(previous) = head.last() else {
        return Context::Start;
    };
    if is_keyword(sql, previous, &["as"]) {
        return Context::None;
    }
    if is_keyword(
        sql,
        previous,
        &[
            "from", "join", "update", "into", "table", "truncate", "only",
        ],
    ) {
        return Context::Table;
    }

    let mut depth = 0usize;
    let mut clause = None;
    for token in head.iter().rev() {
        match token.kind {
            Kind::Punct(')') => depth += 1,
            Kind::Punct('(') if depth == 0 => break,
            Kind::Punct('(') => depth -= 1,
            Kind::Word if depth == 0 && is_keyword(sql, token, CLAUSES) => {
                clause = word(sql, token).map(str::to_lowercase);
                break;
            }
            _ => {}
        }
    }
    let Some(clause) = clause else {
        // Inside parentheses with no clause yet: an expression, or the
        // column list of `INSERT INTO t (…)`.
        return if depth == 0 && head.iter().any(|t| t.kind == Kind::Punct('(')) {
            Context::Expression
        } else {
            Context::AfterTable
        };
    };
    match clause.as_str() {
        "from" if previous.kind == Kind::Punct(',') => Context::Table,
        "from" | "join" | "update" | "into" | "table" | "truncate" | "delete" => {
            Context::AfterTable
        }
        "limit" | "offset" | "with" | "union" | "except" | "intersect" | "window" => {
            Context::AfterTable
        }
        "insert" => Context::AfterTable,
        _ => Context::Expression,
    }
}

// --- Tables the statement reads -----------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct Reference {
    /// `None` for a subquery or a function in `FROM`.
    table: Option<(Option<String>, String)>,
    alias: Option<String>,
}

/// Words that end a table reference instead of naming its alias.
const NOT_ALIASES: &[&str] = &[
    "where",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "natural",
    "on",
    "using",
    "group",
    "order",
    "having",
    "limit",
    "offset",
    "fetch",
    "for",
    "set",
    "values",
    "returning",
    "union",
    "except",
    "intersect",
    "window",
    "select",
    "default",
    "overriding",
    "tablesample",
    "as",
    "lateral",
    "only",
];

/// Ends the table list of a `FROM`.
const FROM_ENDS: &[&str] = &[
    "where",
    "on",
    "using",
    "group",
    "order",
    "having",
    "limit",
    "offset",
    "union",
    "except",
    "intersect",
    "window",
    "returning",
    "select",
    "set",
    "values",
    "fetch",
    "for",
];

/// Every table reference in `statement`, at any depth. `skip` is the word
/// being typed, which is not a name yet.
fn references(sql: &str, statement: &[Token], skip: Option<usize>) -> Vec<Reference> {
    let tokens: Vec<Token> = statement
        .iter()
        .enumerate()
        .filter(|&(n, t)| Some(n) != skip && t.kind != Kind::Comment)
        .map(|(_, t)| *t)
        .collect();
    let mut references = Vec::new();
    let mut in_from = false;
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        // Only `FROM` lists can call functions: `INTO t (` opens a column list.
        let (starts, calls) = if is_keyword(sql, token, &["from", "join"]) {
            in_from = true;
            (true, true)
        } else if is_keyword(sql, token, &["update", "into"]) {
            (true, false)
        } else if is_keyword(sql, token, FROM_ENDS) {
            in_from = false;
            (false, false)
        } else {
            (in_from && token.kind == Kind::Punct(','), true)
        };
        i += 1;
        if starts && let Some((reference, next)) = reference(sql, &tokens, i, calls) {
            references.push(reference);
            i = next;
        }
    }
    references
}

/// The table reference starting at `tokens[i]` and the index after it.
/// `calls`: a name followed by `(` is a function, not a table.
fn reference(sql: &str, tokens: &[Token], mut i: usize, calls: bool) -> Option<(Reference, usize)> {
    while tokens
        .get(i)
        .is_some_and(|t| is_keyword(sql, t, &["only", "lateral"]))
    {
        i += 1;
    }
    let table = if tokens.get(i)?.kind == Kind::Punct('(') {
        i = after_parentheses(tokens, i);
        None
    } else {
        let first = identifier(sql, tokens.get(i)?)?;
        if is_keyword(sql, &tokens[i], NOT_ALIASES) {
            return None;
        }
        i += 1;
        let qualified = tokens.get(i).is_some_and(|t| t.kind == Kind::Punct('.'));
        let name = match tokens.get(i + 1) {
            Some(next) if qualified => identifier(sql, next),
            _ => None,
        };
        let table = match name {
            Some(name) => {
                i += 2;
                (Some(first), name)
            }
            None => (None, first),
        };
        // A function call: `generate_series(1, 10) g`.
        if calls && tokens.get(i).is_some_and(|t| t.kind == Kind::Punct('(')) {
            i = after_parentheses(tokens, i);
            None
        } else {
            Some(table)
        }
    };
    if tokens.get(i).is_some_and(|t| is_keyword(sql, t, &["as"])) {
        i += 1;
    }
    let alias = tokens
        .get(i)
        .filter(|t| !is_keyword(sql, t, NOT_ALIASES))
        .and_then(|t| identifier(sql, t));
    if alias.is_some() {
        i += 1;
    }
    Some((Reference { table, alias }, i))
}

/// Index after the parenthesis that closes the one at `tokens[open]`.
fn after_parentheses(tokens: &[Token], open: usize) -> usize {
    let mut depth = 0;
    for (n, token) in tokens.iter().enumerate().skip(open) {
        match token.kind {
            Kind::Punct('(') => depth += 1,
            Kind::Punct(')') => {
                depth -= 1;
                if depth == 0 {
                    return n + 1;
                }
            }
            _ => {}
        }
    }
    tokens.len()
}

/// The table `name` means: in `schema` if given, else the first match along
/// the search path, else in any schema.
fn find_table<'a>(catalog: &'a Catalog, schema: Option<&str>, name: &str) -> Option<&'a Table> {
    let named = |t: &&Table| t.table.name == name;
    match schema {
        Some(schema) => catalog
            .tables
            .iter()
            .find(|t| t.table.schema == schema && t.table.name == name),
        None => catalog
            .search_path
            .iter()
            .find_map(|schema| {
                catalog
                    .tables
                    .iter()
                    .filter(named)
                    .find(|t| &t.table.schema == schema)
            })
            .or_else(|| catalog.tables.iter().find(named)),
    }
}

fn resolve<'a>(catalog: &'a Catalog, reference: &Reference) -> Option<&'a Table> {
    let (schema, name) = reference.table.as_ref()?;
    find_table(catalog, schema.as_deref(), name)
}

/// The table behind `name.`: an alias, a table the statement reads, or any
/// table on the search path.
fn qualified_table<'a>(
    catalog: &'a Catalog,
    references: &[Reference],
    name: &str,
) -> Option<&'a Table> {
    if let Some(reference) = references.iter().find(|r| r.alias.as_deref() == Some(name)) {
        return resolve(catalog, reference);
    }
    references
        .iter()
        .filter(|r| r.alias.is_none())
        .find(|r| r.table.as_ref().is_some_and(|(_, table)| table == name))
        .and_then(|r| resolve(catalog, r))
        .or_else(|| {
            catalog.search_path.iter().find_map(|schema| {
                catalog
                    .tables
                    .iter()
                    .find(|t| &t.table.schema == schema && t.table.name == name)
            })
        })
}

// --- Suggestions ---------------------------------------------------------

struct Builder<'a> {
    prefix: &'a str,
    /// What quotes identifiers.
    quote: char,
    /// The word started with `"`: identifiers are inserted quoted.
    quoted: bool,
    suggestions: Vec<Suggestion>,
}

impl Builder<'_> {
    fn matches(&self, candidate: &str) -> bool {
        candidate
            .to_lowercase()
            .starts_with(&self.prefix.to_lowercase())
    }

    fn push(&mut self, label: String, insert: String, kind: SuggestionKind, detail: String) {
        self.suggestions.push(Suggestion {
            label,
            insert,
            kind,
            detail,
        });
    }

    fn identifier(&self, name: &str) -> String {
        if self.quoted || needs_quotes(name) {
            let quote = self.quote;
            format!(
                "{quote}{}{quote}",
                name.replace(quote, &format!("{quote}{quote}"))
            )
        } else {
            name.to_owned()
        }
    }

    fn keywords(&mut self, keywords: &[&str], style: Style) {
        if self.quoted {
            return;
        }
        for keyword in keywords {
            if self.matches(keyword) {
                let text = match style {
                    Style::Upper => keyword.to_string(),
                    Style::Lower => keyword.to_lowercase(),
                };
                self.push(
                    text.clone(),
                    text,
                    SuggestionKind::Keyword,
                    "keyword".into(),
                );
            }
        }
    }

    /// Tables and views: those on the search path by name, the rest
    /// schema-qualified; then schemas.
    fn tables(&mut self, catalog: &Catalog) {
        let on_path = |t: &Table| catalog.search_path.contains(&t.table.schema);
        for table in catalog.tables.iter().filter(|t| on_path(t)) {
            if self.matches(&table.table.name) {
                self.push_table(table, false);
            }
        }
        for table in catalog.tables.iter().filter(|t| !on_path(t)) {
            if self.matches(&table.table.name) || self.matches(&table.table.to_string()) {
                self.push_table(table, true);
            }
        }
        for schema in &catalog.schemas {
            if self.matches(schema) {
                self.push(
                    schema.clone(),
                    self.identifier(schema),
                    SuggestionKind::Schema,
                    "schema".into(),
                );
            }
        }
    }

    fn tables_in(&mut self, catalog: &Catalog, schema: &str) {
        for table in catalog.tables.iter().filter(|t| t.table.schema == schema) {
            if self.matches(&table.table.name) {
                self.push_table(table, false);
            }
        }
    }

    fn push_table(&mut self, table: &Table, qualified: bool) {
        let (label, insert) = if qualified {
            (
                table.table.to_string(),
                format!(
                    "{}.{}",
                    self.identifier(&table.table.schema),
                    self.identifier(&table.table.name)
                ),
            )
        } else {
            (table.table.name.clone(), self.identifier(&table.table.name))
        };
        let kind = match table.kind {
            TableKind::View | TableKind::MaterializedView => SuggestionKind::View,
            _ => SuggestionKind::Table,
        };
        let detail = format!(
            "{} · {}",
            table.kind.label().to_lowercase(),
            table.table.schema
        );
        self.push(label, insert, kind, detail);
    }

    fn columns_of(&mut self, table: &Table) {
        for column in &table.columns {
            if self.matches(&column.name) {
                self.push(
                    column.name.clone(),
                    self.identifier(&column.name),
                    SuggestionKind::Column,
                    column.data_type.clone(),
                );
            }
        }
    }

    /// Columns of every table the statement reads (each name once), then
    /// the aliases and table names that can qualify them.
    fn columns(&mut self, catalog: &Catalog, references: &[Reference]) {
        let tables: Vec<(&Reference, &Table)> = references
            .iter()
            .filter_map(|r| resolve(catalog, r).map(|t| (r, t)))
            .collect();
        let mut seen = Vec::new();
        for (_, table) in &tables {
            for column in &table.columns {
                if seen.contains(&&column.name) || !self.matches(&column.name) {
                    continue;
                }
                seen.push(&column.name);
                self.push(
                    column.name.clone(),
                    self.identifier(&column.name),
                    SuggestionKind::Column,
                    format!("{} · {}", column.data_type, table.table.name),
                );
            }
        }
        for (reference, table) in &tables {
            let (name, kind, detail) = match &reference.alias {
                Some(alias) => (
                    alias,
                    SuggestionKind::Alias,
                    format!("alias · {}", table.table.name),
                ),
                None => (
                    &table.table.name,
                    SuggestionKind::Table,
                    table.kind.label().to_lowercase(),
                ),
            };
            if self.matches(name) && !self.suggestions.iter().any(|s| &s.label == name) {
                self.push(name.clone(), self.identifier(name), kind, detail);
            }
        }
    }
}

/// Whether PostgreSQL needs `name` quoted: uppercase, spaces, a leading
/// digit or a reserved word.
fn needs_quotes(name: &str) -> bool {
    let mut chars = name.chars();
    let plain = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$');
    !plain || RESERVED.contains(&name)
}

/// PostgreSQL's reserved words, which are not valid bare identifiers.
const RESERVED: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "authorization",
    "binary",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "collation",
    "column",
    "concurrently",
    "constraint",
    "create",
    "cross",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "freeze",
    "from",
    "full",
    "grant",
    "group",
    "having",
    "ilike",
    "in",
    "initially",
    "inner",
    "intersect",
    "into",
    "is",
    "isnull",
    "join",
    "lateral",
    "leading",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "natural",
    "not",
    "notnull",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "outer",
    "overlaps",
    "placing",
    "primary",
    "references",
    "returning",
    "right",
    "select",
    "session_user",
    "similar",
    "some",
    "symmetric",
    "system_user",
    "table",
    "tablesample",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "variadic",
    "verbose",
    "when",
    "where",
    "window",
    "with",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Upper,
    Lower,
}

/// Keywords follow the case of the word being typed, else of the
/// statement's first keyword, else uppercase.
fn keyword_style(sql: &str, prefix: &str, statement: &[Token]) -> Style {
    let sample = if prefix.chars().any(char::is_alphabetic) {
        Some(prefix)
    } else {
        statement.iter().find_map(|t| word(sql, t))
    };
    match sample {
        Some(text) if text.chars().any(char::is_lowercase) => Style::Lower,
        _ => Style::Upper,
    }
}

const START_KEYWORDS: &[&str] = &[
    "SELECT",
    "INSERT INTO",
    "UPDATE",
    "DELETE FROM",
    "WITH",
    "EXPLAIN",
    "EXPLAIN ANALYZE",
    "CREATE",
    "ALTER",
    "DROP",
    "TRUNCATE",
    "VALUES",
    "TABLE",
    "SHOW",
    "SET",
    "COMMENT ON",
    "GRANT",
    "REVOKE",
    "CALL",
    "COPY",
    "ANALYZE",
    "VACUUM",
];

const AFTER_TABLE_KEYWORDS: &[&str] = &[
    "AS",
    "WHERE",
    "JOIN",
    "INNER JOIN",
    "LEFT JOIN",
    "RIGHT JOIN",
    "FULL JOIN",
    "CROSS JOIN",
    "ON",
    "USING",
    "GROUP BY",
    "HAVING",
    "ORDER BY",
    "LIMIT",
    "OFFSET",
    "SET",
    "VALUES",
    "DEFAULT VALUES",
    "SELECT",
    "FROM",
    "RETURNING",
    "UNION",
    "UNION ALL",
    "EXCEPT",
    "INTERSECT",
    "FOR UPDATE",
];

const EXPRESSION_KEYWORDS: &[&str] = &[
    "AND",
    "OR",
    "NOT",
    "IN",
    "IS NULL",
    "IS NOT NULL",
    "LIKE",
    "ILIKE",
    "BETWEEN",
    "EXISTS",
    "AS",
    "DISTINCT",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "NULL",
    "TRUE",
    "FALSE",
    "CAST",
    "FROM",
    "WHERE",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "LIMIT",
    "ASC",
    "DESC",
    "NULLS FIRST",
    "NULLS LAST",
    "SELECT",
    "RETURNING",
    "DEFAULT",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn table(schema: &str, name: &str, kind: TableKind, columns: &[(&str, &str)]) -> Table {
        Table {
            table: TableRef::new(schema, name),
            kind,
            columns: columns
                .iter()
                .map(|(name, data_type)| Column {
                    name: (*name).into(),
                    data_type: (*data_type).into(),
                })
                .collect(),
        }
    }

    fn catalog() -> Catalog {
        Catalog {
            search_path: vec!["public".into()],
            schemas: vec!["public".into(), "sales".into()],
            tables: vec![
                table(
                    "public",
                    "customers",
                    TableKind::Table,
                    &[("id", "integer"), ("name", "text"), ("user", "text")],
                ),
                table(
                    "public",
                    "orders",
                    TableKind::Table,
                    &[
                        ("id", "integer"),
                        ("customer_id", "integer"),
                        ("total", "numeric"),
                    ],
                ),
                table(
                    "public",
                    "Order Items",
                    TableKind::Table,
                    &[("qty", "integer")],
                ),
                table(
                    "public",
                    "order_totals",
                    TableKind::View,
                    &[("sum", "numeric")],
                ),
                table("sales", "invoices", TableKind::Table, &[("number", "text")]),
            ],
            quote: '"',
        }
    }

    /// `|` marks the caret.
    fn run(sql: &str, invoked: bool) -> Completions {
        let offset = sql.find('|').expect("caret");
        let sql = sql.replacen('|', "", 1);
        complete(&sql, offset, &catalog(), invoked)
    }

    fn inserts(sql: &str) -> Vec<String> {
        run(sql, false)
            .suggestions
            .into_iter()
            .map(|s| s.insert)
            .collect()
    }

    fn invoked(sql: &str) -> Vec<String> {
        run(sql, true)
            .suggestions
            .into_iter()
            .map(|s| s.insert)
            .collect()
    }

    #[test]
    fn statement_keywords_follow_the_typed_case() {
        assert_eq!(inserts("sel|"), ["select"]);
        assert_eq!(inserts("SEL|"), ["SELECT"]);
        assert_eq!(inserts("del|"), ["delete from"]);
    }

    #[test]
    fn tables_after_from_without_typing() {
        let completions = run("select * from |", false);
        let labels: Vec<_> = completions.suggestions.iter().map(|s| &s.label).collect();
        assert_eq!(
            labels,
            [
                "customers",
                "orders",
                "Order Items",
                "order_totals",
                "sales.invoices",
                "public",
                "sales"
            ]
        );
        assert_eq!(completions.replace, 14..14);
    }

    #[test]
    fn tables_filtered_by_prefix() {
        let completions = run("select * from or|", false);
        assert_eq!(completions.replace, 14..16);
        assert_eq!(completions.prefix, "or");
        let inserts: Vec<_> = completions
            .suggestions
            .into_iter()
            .map(|s| s.insert)
            .collect();
        assert_eq!(inserts, ["orders", "\"Order Items\"", "order_totals"]);
    }

    #[test]
    fn tables_outside_the_search_path_are_qualified() {
        assert_eq!(inserts("select * from inv|"), ["sales.invoices"]);
        assert_eq!(inserts("select * from sales.|"), ["invoices"]);
        assert_eq!(inserts("select * from sales.in|"), ["invoices"]);
    }

    #[test]
    fn tables_after_join_update_into_and_commas() {
        assert_eq!(inserts("select * from orders join cu|"), ["customers"]);
        assert_eq!(inserts("select * from orders o, cu|"), ["customers"]);
        assert_eq!(inserts("update cu|"), ["customers"]);
        assert_eq!(inserts("insert into cu|"), ["customers"]);
        assert_eq!(inserts("delete from cu|"), ["customers"]);
    }

    #[test]
    fn columns_through_an_alias_declared_later() {
        assert_eq!(
            inserts("select o.| from orders o"),
            ["id", "customer_id", "total"]
        );
        assert_eq!(inserts("select o.cu| from orders as o"), ["customer_id"]);
    }

    #[test]
    fn columns_through_a_table_name_or_qualified_name() {
        assert_eq!(inserts("select orders.t| from orders"), ["total"]);
        assert_eq!(inserts("select public.orders.t| from orders"), ["total"]);
        assert_eq!(inserts("select s.n| from sales.invoices s"), ["number"]);
    }

    #[test]
    fn columns_in_join_conditions() {
        assert_eq!(
            inserts("select * from orders o join customers c on c.|"),
            ["id", "name", "\"user\""]
        );
    }

    #[test]
    fn unqualified_columns_come_from_every_table_read() {
        let completions = run(
            "select * from orders o join customers c on o.customer_id = c.id where n|",
            false,
        );
        let first = &completions.suggestions[0];
        assert_eq!(first.insert, "name");
        assert_eq!(first.kind, SuggestionKind::Column);
        assert_eq!(first.detail, "text · customers");
        assert!(completions.suggestions.iter().any(|s| s.insert == "not"));
    }

    #[test]
    fn columns_are_listed_once_with_aliases_after() {
        let all = invoked("select | from orders o join customers c on true");
        assert_eq!(
            all[..7],
            ["id", "customer_id", "total", "name", "\"user\"", "o", "c"]
        );
        assert!(all.contains(&"FROM".to_owned()) || all.contains(&"from".to_owned()));
    }

    #[test]
    fn nothing_until_typing_in_expressions() {
        assert!(inserts("select | from orders").is_empty());
        assert!(inserts("select * from orders where id = 1 and |").is_empty());
        assert!(!invoked("select | from orders").is_empty());
    }

    #[test]
    fn keywords_after_a_table() {
        let after = inserts("select * from orders o|");
        assert!(after.contains(&"on".to_owned()));
        assert!(after.contains(&"order by".to_owned()));
        assert!(!after.contains(&"orders".to_owned()));
        assert!(inserts("select * from orders |").is_empty());
    }

    #[test]
    fn update_set_and_insert_column_lists() {
        assert_eq!(inserts("update customers set na|"), ["name"]);
        assert_eq!(inserts("insert into orders (cust|"), ["customer_id"]);
    }

    #[test]
    fn statement_under_the_caret_only() {
        let all = invoked("select * from orders; select | from customers");
        assert!(all.contains(&"name".to_owned()));
        assert!(!all.contains(&"total".to_owned()));
        assert_eq!(inserts("select 1;sel|"), ["select"]);
    }

    #[test]
    fn nothing_inside_strings_comments_numbers_or_after_as() {
        assert!(invoked("select 'fr|").is_empty());
        assert!(invoked("select 'a', 'fr|' from orders").is_empty());
        assert!(invoked("select 1 -- fr|").is_empty());
        assert!(invoked("select /* fr| */ 1").is_empty());
        assert!(invoked("select $$ fr| $$").is_empty());
        assert!(invoked("select 12|").is_empty());
        assert!(inserts("select name as n| from customers").is_empty());
    }

    #[test]
    fn quoted_words_insert_quoted_identifiers() {
        let completions = run("select * from \"Or|", false);
        assert_eq!(completions.replace, 14..17);
        let quoted: Vec<_> = completions
            .suggestions
            .iter()
            .map(|s| s.insert.as_str())
            .collect();
        assert_eq!(
            quoted,
            ["\"orders\"", "\"Order Items\"", "\"order_totals\""]
        );
        assert_eq!(
            inserts("select * from \"Order Items\" i where i.q|"),
            ["qty"]
        );
    }

    #[test]
    fn subqueries_and_functions_are_not_tables() {
        assert!(inserts("select s.| from (select 1) s").is_empty());
        assert!(inserts("select g.| from generate_series(1, 3) g").is_empty());
        assert_eq!(
            inserts("select * from (select 1) s join orders o on o.t|"),
            ["total"]
        );
    }

    #[test]
    fn multibyte_text_before_the_caret() {
        assert_eq!(inserts("select 'café' from cu|"), ["customers"]);
    }
}
