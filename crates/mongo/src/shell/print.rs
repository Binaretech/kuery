//! An [`Op`] as the statement that reads back as it.

use kuery_core::document::language::Style;
use kuery_core::document::value::Value;

use crate::literal::render;
use crate::shell::{Op, Show};

/// The statement, on one line, that [`super::statements`] reads back as
/// `op`.
pub fn print(op: &Op) -> String {
    match op {
        Op::Use(database) => match plain(database) {
            true => format!("use {database}"),
            false => format!("use {}", text(database)),
        },
        Op::Show(Show::Databases) => "show dbs".into(),
        Op::Show(Show::Collections) => "show collections".into(),
        Op::Collection {
            target,
            method,
            args,
            chain,
        } => {
            let (database, collection) = match target.path.as_slice() {
                [database, collection] => (Some(database.as_str()), collection.as_str()),
                [collection] => (None, collection.as_str()),
                _ => (None, ""),
            };
            let mut out = db(database);
            out.push('.');
            out.push_str(&collection_ref(collection));
            out.push_str(&call(method.name(), args));
            for (cursor, args) in chain {
                out.push_str(&call(cursor.name(), args));
            }
            out
        }
        Op::Database {
            database,
            method,
            args,
        } => db(database.as_deref()) + &call(method.name(), args),
        Op::RunCommand {
            database,
            admin,
            command,
        } => {
            let name = match admin {
                true => "adminCommand",
                false => "runCommand",
            };
            db(database.as_deref()) + &call(name, &[Value::Object(command.clone())])
        }
    }
}

/// What follows `db.` for a collection: its name where it reads so,
/// `getCollection("order-lines")` otherwise.
pub fn collection_ref(name: &str) -> String {
    match plain(name) {
        true => name.to_string(),
        false => format!("getCollection({})", text(name)),
    }
}

fn db(database: Option<&str>) -> String {
    match database {
        Some(database) => format!("db.getSiblingDB({})", text(database)),
        None => "db".into(),
    }
}

/// `.name(a, b)`.
fn call(name: &str, args: &[Value]) -> String {
    let args: Vec<String> = args.iter().map(|arg| render(arg, Style::Compact)).collect();
    format!(".{name}({})", args.join(", "))
}

fn text(text: &str) -> String {
    render(&Value::String(text.into()), Style::Compact)
}

/// Whether a collection's name reads after `db.` as it is, and a
/// database's after `use` without quotes (`a-0b` would not: `0b` reads as
/// a number).
fn plain(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
