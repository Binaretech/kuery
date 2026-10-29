//! What each console statement does, read from its [`Op`]: a read, a data
//! change, a schema change, or (for whatever Kuery does not know) a write
//! of another kind. A read-only session runs only reads; the protocol guard
//! ([`crate::wire::guard`]) checks the command again, without this.

use kuery_core::analysis::StatementKind;
use kuery_core::document::language::{Danger, OpInfo, ParseError, Statement};
use kuery_core::document::value::{CollectionRef, Value};

use crate::literal::{any_key, runs_javascript};
use crate::shell::{self, Cursor, DbMethod, Method, Op, Show};

/// Splits a script into statements and says what each does. A `use` in
/// the script names the database of the statements after it.
pub fn analyze(text: &str) -> Result<Vec<Statement>, ParseError> {
    let mut current: Option<String> = None;
    let statements = shell::statements(text)?;
    Ok(statements
        .into_iter()
        .map(|(span, op)| {
            let mut info = classify(&op);
            if let (Some(database), Some(target)) = (&current, &mut info.target)
                && target.path.len() == 1
            {
                target.path.insert(0, database.clone());
            }
            if let Op::Use(database) = &op {
                current = Some(database.clone());
            }
            Statement { span, info }
        })
        .collect())
}

/// Commands `db.runCommand` runs as reads; anything else counts as a
/// write.
const READ_COMMANDS: &[&str] = &[
    "ping",
    "hello",
    "buildInfo",
    "serverStatus",
    "dbStats",
    "collStats",
    "listCollections",
    "listIndexes",
    "connectionStatus",
    "hostInfo",
];

const DATA_COMMANDS: &[&str] = &["insert", "update", "delete", "findAndModify", "bulkWrite"];

const SCHEMA_COMMANDS: &[&str] = &[
    "create",
    "drop",
    "dropDatabase",
    "createIndexes",
    "dropIndexes",
    "renameCollection",
    "collMod",
    "convertToCapped",
];

/// Commands that end a transaction or a session: Kuery runs each script's
/// transaction itself.
pub(crate) const TRANSACTION_COMMANDS: &[&str] = &[
    "commitTransaction",
    "abortTransaction",
    "startTransaction",
    "endSessions",
    "killSessions",
    "killAllSessions",
    "killAllSessionsByPattern",
];

/// Commands that change who may do what.
const USER_COMMANDS: &[&str] = &[
    "createUser",
    "updateUser",
    "dropUser",
    "dropAllUsersFromDatabase",
    "grantRolesToUser",
    "revokeRolesFromUser",
    "createRole",
    "updateRole",
    "dropRole",
    "dropAllRolesFromDatabase",
    "grantRolesToRole",
    "revokeRolesFromRole",
    "grantPrivilegesToRole",
    "revokePrivilegesFromRole",
];

/// Commands that weigh on the server whatever they are given.
const HEAVY_COMMANDS: &[&str] = &["createIndexes", "validate", "compact"];

/// Pipeline stages that write their results to a collection.
const WRITING_STAGES: &[&str] = &["$out", "$merge"];

/// What one statement does.
pub fn classify(op: &Op) -> OpInfo {
    match op {
        Op::Use(database) => OpInfo {
            enters: Some(vec![database.clone()]),
            ..read(format!("use {database}"))
        },
        Op::Show(Show::Databases) => read("show dbs".into()),
        Op::Show(Show::Collections) => read("show collections".into()),
        Op::Collection {
            target,
            method,
            args,
            chain,
        } => collection(target, *method, args, chain),
        Op::Database {
            database,
            method,
            args,
        } => database_method(database.as_deref(), *method, args),
        Op::RunCommand { admin, command, .. } => {
            let name = command.0.first().map_or("", |(name, _)| name.as_str());
            let summary = match admin {
                true => format!("adminCommand {name}"),
                false => format!("runCommand {name}"),
            };
            let kind = if READ_COMMANDS.contains(&name) {
                StatementKind::Read
            } else if DATA_COMMANDS.contains(&name) {
                StatementKind::Data
            } else if SCHEMA_COMMANDS.contains(&name) {
                StatementKind::Schema
            } else if TRANSACTION_COMMANDS.contains(&name) {
                StatementKind::Transaction
            } else {
                StatementKind::Other
            };
            let mut dangers = Vec::new();
            if USER_COMMANDS.contains(&name) {
                dangers.push(Danger(format!("{name} changes who may do what")));
            }
            let command = Value::Object(command.clone());
            OpInfo {
                kind,
                target: None,
                summary,
                dangers,
                heavy: HEAVY_COMMANDS.contains(&name) || runs_javascript(&command),
                explainable: false,
                enters: None,
            }
        }
    }
}

fn read(summary: String) -> OpInfo {
    OpInfo {
        kind: StatementKind::Read,
        target: None,
        summary,
        dangers: Vec::new(),
        heavy: false,
        explainable: false,
        enters: None,
    }
}

fn collection(
    target: &CollectionRef,
    method: Method,
    args: &[Value],
    chain: &[(Cursor, Vec<Value>)],
) -> OpInfo {
    use Method::*;
    let name = target.path.join(".");
    let kind = match method {
        Find
        | FindOne
        | CountDocuments
        | EstimatedDocumentCount
        | Distinct
        | GetIndexes
        | Stats => StatementKind::Read,
        // `$out` and `$merge` fill or replace a whole collection, which no
        // transaction can hold: like a schema change, they run on their own.
        Aggregate if args.iter().any(writes) => StatementKind::Schema,
        Aggregate => StatementKind::Read,
        InsertOne | InsertMany | UpdateOne | UpdateMany | ReplaceOne | DeleteOne | DeleteMany
        | FindOneAndUpdate | FindOneAndReplace | FindOneAndDelete | BulkWrite => {
            StatementKind::Data
        }
        CreateIndex | CreateIndexes | DropIndex | DropIndexes | Drop | RenameCollection => {
            StatementKind::Schema
        }
    };
    let empty =
        |i: usize| matches!(args.get(i), Some(Value::Object(filter)) if filter.0.is_empty());
    let mut dangers = Vec::new();
    match method {
        UpdateMany if empty(0) => dangers.push(Danger(format!(
            "updateMany with an empty filter changes every document in {name}"
        ))),
        DeleteMany if empty(0) => dangers.push(Danger(format!(
            "deleteMany with an empty filter removes every document in {name}"
        ))),
        Drop => dangers.push(Danger(format!(
            "drop removes the collection {name} with its indexes"
        ))),
        DropIndexes if args.is_empty() || args[0] == Value::String("*".into()) => {
            dangers.push(Danger(format!(
                "dropIndexes removes every index of {name} but _id's"
            )));
        }
        RenameCollection if args.get(1) == Some(&Value::Bool(true)) => {
            let to = match args.first() {
                Some(Value::String(to)) => to.as_str(),
                _ => "the target",
            };
            dangers.push(Danger(format!(
                "renameCollection with dropTarget first drops {to}"
            )));
        }
        ReplaceOne => {
            let by_id =
                matches!(args.first(), Some(Value::Object(filter)) if filter.get("_id").is_some());
            if !by_id {
                dangers.push(Danger(format!(
                    "replaceOne without _id in its filter replaces whichever document in {name} matches first"
                )));
            }
        }
        _ => {}
    }
    let explained = chain.iter().any(|(cursor, _)| *cursor == Cursor::Explain);
    // Explaining a write runs nothing, but Kuery still counts it as one;
    // it cannot run in a transaction either.
    let kind = match (explained, kind) {
        (true, StatementKind::Data | StatementKind::Schema) => StatementKind::Other,
        (_, kind) => kind,
    };
    let summary = match explained {
        true => format!("explain {} {name}", method.name()),
        false => format!("{} {name}", method.name()),
    };
    let javascript = args.iter().any(runs_javascript)
        || chain
            .iter()
            .any(|(_, args)| args.iter().any(runs_javascript));
    OpInfo {
        kind,
        target: Some(target.clone()),
        summary,
        dangers,
        heavy: javascript || matches!(method, CreateIndex | CreateIndexes),
        explainable: method.explainable(),
        enters: None,
    }
}

fn database_method(database: Option<&str>, method: DbMethod, args: &[Value]) -> OpInfo {
    let (kind, dangers) = match method {
        DbMethod::Stats | DbMethod::GetCollectionNames => (StatementKind::Read, Vec::new()),
        DbMethod::CreateCollection | DbMethod::CreateView => (StatementKind::Schema, Vec::new()),
        DbMethod::DropDatabase => {
            let which = database.unwrap_or("the database in use");
            let danger = Danger(format!(
                "dropDatabase removes {which} with every collection in it"
            ));
            (StatementKind::Schema, vec![danger])
        }
    };
    let summary = match args.first() {
        Some(Value::String(name)) => format!("{} {name}", method.name()),
        _ => method.name().to_string(),
    };
    OpInfo {
        kind,
        target: None,
        summary,
        dangers,
        heavy: args.iter().any(runs_javascript),
        explainable: false,
        enters: None,
    }
}

/// Whether a value holds a writing stage anywhere in it: in a pipeline,
/// and in the pipelines of its `$facet`, `$lookup` and `$unionWith` stages.
fn writes(value: &Value) -> bool {
    any_key(value, |name| WRITING_STAGES.contains(&name))
}

#[cfg(test)]
mod tests;
