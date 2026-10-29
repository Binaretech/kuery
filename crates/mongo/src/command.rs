//! A console statement as the command the server runs, and how to read
//! its answer. Pure, so each command is tested without a server; what a
//! read-only connection may send is decided again on the result, by
//! [`crate::wire::guard`].

use bson::{Bson, Document as BsonDocument, doc};
use kuery_core::document::value::{Document, Value};
use kuery_core::session::QueryError;

use crate::convert;
use crate::shell::{Cursor, DbMethod, Method, Op, Show};

/// A command, where it runs, and what its answer holds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Built {
    pub(crate) database: String,
    pub(crate) command: BsonDocument,
    pub(crate) reply: Reply,
}

/// Where a command's documents are in its answer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Reply {
    /// It answers with a cursor, read to the end.
    Cursor,
    /// The answer is the result.
    Answer,
    /// The documents of an array in the answer (`databases`).
    Listed(&'static str),
    /// `{count: n}`, from the answer's `n`.
    Count,
    /// `countDocuments`: a cursor of one `{n}`, or none for no document.
    Counted,
    /// `distinct`: one document per value, under the field's name.
    Values(String),
    Write(Write),
    /// `findOneAnd…`: the document in `value`, and what it did.
    FindAndModify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Write {
    Insert,
    Update,
    Delete,
}

/// The command for `op`, run where `current` is the database in use.
/// `None` for `use`, which only changes the database.
pub(crate) fn build(op: &Op, current: &str) -> Result<Option<Built>, QueryError> {
    let built = match op {
        Op::Use(_) => return Ok(None),
        Op::Show(Show::Databases) => Built {
            database: "admin".into(),
            command: doc! { "listDatabases": 1, "nameOnly": false, "authorizedDatabases": true },
            reply: Reply::Listed("databases"),
        },
        Op::Show(Show::Collections) => collections(current),
        Op::Collection {
            target,
            method,
            args,
            chain,
        } => {
            let (database, name) = match target.path.as_slice() {
                [database, name] => (database.as_str(), name.as_str()),
                [name] => (current, name.as_str()),
                _ => return Err(unsupported("A collection is named db.orders")),
            };
            let args = arguments(args)?;
            let chain = chain
                .iter()
                .map(|(cursor, args)| Ok((*cursor, arguments(args)?)))
                .collect::<Result<Vec<_>, QueryError>>()?;
            let built = collection(database, name, *method, &args, &chain)?;
            match chain.iter().find(|(cursor, _)| *cursor == Cursor::Explain) {
                Some((_, args)) => explain(built, args),
                None => built,
            }
        }
        Op::Database {
            database,
            method,
            args,
        } => {
            let database = database.as_deref().unwrap_or(current);
            database_method(database, *method, &arguments(args)?)?
        }
        Op::RunCommand {
            database,
            admin,
            command,
        } => Built {
            database: match admin {
                true => "admin".into(),
                false => database.as_deref().unwrap_or(current).into(),
            },
            command: bson_document(command)?,
            reply: Reply::Answer,
        },
    };
    Ok(Some(built))
}

fn unsupported(message: &str) -> QueryError {
    QueryError::Unsupported(message.into())
}

fn arguments(args: &[Value]) -> Result<Vec<Bson>, QueryError> {
    args.iter()
        .map(|value| convert::bson(value).map_err(QueryError::Unsupported))
        .collect()
}

fn bson_document(document: &Document) -> Result<BsonDocument, QueryError> {
    convert::bson_document(document).map_err(QueryError::Unsupported)
}

/// The `n`th argument as a document; none when it is not there.
fn document(args: &[Bson], n: usize) -> BsonDocument {
    match args.get(n) {
        Some(Bson::Document(document)) => document.clone(),
        _ => BsonDocument::new(),
    }
}

/// Adds the options a caller gave, the command's own fields first.
fn with(mut command: BsonDocument, options: BsonDocument) -> BsonDocument {
    for (key, value) in options {
        command.entry(key).or_insert(value);
    }
    command
}

fn collections(database: &str) -> Built {
    Built {
        database: database.into(),
        command: doc! { "listCollections": 1, "nameOnly": true, "authorizedCollections": true },
        reply: Reply::Cursor,
    }
}

fn collection(
    database: &str,
    name: &str,
    method: Method,
    args: &[Bson],
    chain: &[(Cursor, Vec<Bson>)],
) -> Result<Built, QueryError> {
    use Method::*;
    let built = |command: BsonDocument, reply: Reply| Built {
        database: database.into(),
        command,
        reply,
    };
    let filter = || document(args, 0);
    Ok(match method {
        Find => built(find(name, args, chain), Reply::Cursor),
        FindOne => {
            let mut command = find(name, args, &[]);
            command.insert("limit", 1);
            command.insert("singleBatch", true);
            built(command, Reply::Cursor)
        }
        Aggregate => {
            let mut options = document(args, 1);
            let batch = options.remove("batchSize");
            let explained = options.remove("explain") == Some(Bson::Boolean(true));
            let pipeline = args.first().cloned().unwrap_or(Bson::Array(Vec::new()));
            let cursor = match batch {
                Some(batch) => doc! { "batchSize": batch },
                None => doc! {},
            };
            let command = doc! { "aggregate": name, "pipeline": pipeline, "cursor": cursor };
            let aggregate = built(with(command, options), Reply::Cursor);
            match explained {
                true => explain(aggregate, &[]),
                false => aggregate,
            }
        }
        CountDocuments => built(
            count_documents(name, filter(), document(args, 1)),
            Reply::Counted,
        ),
        EstimatedDocumentCount => built(
            with(doc! { "count": name }, document(args, 0)),
            Reply::Count,
        ),
        Distinct => {
            let field = match args.first() {
                Some(Bson::String(field)) => field.clone(),
                _ => return Err(unsupported("distinct() takes a field's name")),
            };
            let command = doc! { "distinct": name, "key": &field, "query": document(args, 1) };
            built(with(command, document(args, 2)), Reply::Values(field))
        }
        GetIndexes => built(doc! { "listIndexes": name, "cursor": {} }, Reply::Cursor),
        Stats => {
            let mut command = doc! { "collStats": name };
            match args.first() {
                Some(Bson::Document(options)) => command = with(command, options.clone()),
                Some(scale) => {
                    command.insert("scale", scale.clone());
                }
                None => {}
            }
            built(command, Reply::Answer)
        }
        InsertOne | InsertMany => {
            let documents = match (method, args.first()) {
                (InsertMany, Some(Bson::Array(documents))) => documents.clone(),
                (_, Some(document)) => vec![document.clone()],
                _ => Vec::new(),
            };
            let command = doc! { "insert": name, "documents": documents };
            built(
                with(command, document(args, 1)),
                Reply::Write(Write::Insert),
            )
        }
        UpdateOne | UpdateMany | ReplaceOne => {
            let (statement, options) = statement_options(document(args, 2));
            let mut update = doc! {
                "q": filter(),
                "u": args.get(1).cloned().unwrap_or(Bson::Document(BsonDocument::new())),
                "multi": method == UpdateMany,
            };
            update.extend(statement);
            let command = doc! { "update": name, "updates": [update] };
            built(with(command, options), Reply::Write(Write::Update))
        }
        DeleteOne | DeleteMany => {
            let (statement, options) = statement_options(document(args, 1));
            let mut delete = doc! { "q": filter(), "limit": i32::from(method == DeleteOne) };
            delete.extend(statement);
            let command = doc! { "delete": name, "deletes": [delete] };
            built(with(command, options), Reply::Write(Write::Delete))
        }
        FindOneAndUpdate | FindOneAndReplace | FindOneAndDelete => {
            let (change, options) = match method {
                FindOneAndDelete => (doc! { "remove": true }, document(args, 1)),
                _ => (
                    doc! { "update": args.get(1).cloned().unwrap_or(Bson::Document(BsonDocument::new())) },
                    document(args, 2),
                ),
            };
            let mut command = doc! { "findAndModify": name, "query": filter() };
            command.extend(change);
            built(
                with(command, find_and_modify_options(options)),
                Reply::FindAndModify,
            )
        }
        BulkWrite => {
            return Err(unsupported(
                "Kuery does not run bulkWrite: write each change as its own statement",
            ));
        }
        CreateIndex | CreateIndexes => {
            let keys: Vec<Bson> = match (method, args.first()) {
                (CreateIndexes, Some(Bson::Array(keys))) => keys.clone(),
                (_, Some(keys)) => vec![keys.clone()],
                _ => Vec::new(),
            };
            let options = document(args, 1);
            let indexes = keys
                .into_iter()
                .map(|keys| {
                    let keys = match keys {
                        Bson::Document(keys) => keys,
                        _ => {
                            return Err(unsupported(
                                "An index's keys are a document, such as {status: 1}",
                            ));
                        }
                    };
                    let mut index = doc! { "key": &keys, "name": index_name(&keys) };
                    index.extend(options.clone());
                    Ok(Bson::Document(index))
                })
                .collect::<Result<Vec<_>, QueryError>>()?;
            built(
                doc! { "createIndexes": name, "indexes": indexes },
                Reply::Answer,
            )
        }
        DropIndex | DropIndexes => {
            let index = args.first().cloned().unwrap_or(Bson::String("*".into()));
            built(doc! { "dropIndexes": name, "index": index }, Reply::Answer)
        }
        Drop => built(
            with(doc! { "drop": name }, document(args, 0)),
            Reply::Answer,
        ),
        RenameCollection => {
            let to = match args.first() {
                Some(Bson::String(to)) => to.clone(),
                _ => return Err(unsupported("renameCollection() takes the new name")),
            };
            let drop_target = matches!(args.get(1), Some(Bson::Boolean(true)));
            Built {
                database: "admin".into(),
                command: doc! {
                    "renameCollection": format!("{database}.{name}"),
                    "to": format!("{database}.{to}"),
                    "dropTarget": drop_target,
                },
                reply: Reply::Answer,
            }
        }
    })
}

/// `find` with its arguments (filter, projection, options) and the
/// cursor's modifiers, which win over the options.
fn find(name: &str, args: &[Bson], chain: &[(Cursor, Vec<Bson>)]) -> BsonDocument {
    let mut command = doc! { "find": name, "filter": document(args, 0) };
    let projection = document(args, 1);
    if !projection.is_empty() {
        command.insert("projection", projection);
    }
    let mut options = document(args, 2);
    if let Some(Bson::Document(projection)) = options.remove("projection") {
        command.insert("projection", projection);
    }
    for (cursor, args) in chain {
        let Some(arg) = args.first().cloned() else {
            continue;
        };
        let field = match cursor {
            Cursor::Sort => "sort",
            Cursor::Limit => "limit",
            Cursor::Skip => "skip",
            Cursor::Projection => "projection",
            Cursor::Hint => "hint",
            Cursor::Collation => "collation",
            Cursor::MaxTimeMs => "maxTimeMS",
            Cursor::BatchSize => "batchSize",
            Cursor::Explain => continue,
        };
        // `limit(-5)` in mongosh: five, in one batch.
        let arg = match (field, arg) {
            ("limit", Bson::Int32(n)) if n < 0 => {
                command.insert("singleBatch", true);
                Bson::Int32(n.saturating_abs())
            }
            ("limit", Bson::Int64(n)) if n < 0 => {
                command.insert("singleBatch", true);
                Bson::Int64(n.saturating_abs())
            }
            (_, arg) => arg,
        };
        command.insert(field, arg);
    }
    with(command, options)
}

/// `countDocuments`: an aggregation, which a sharded cluster answers
/// without orphaned documents; with `$where`, which a `$match` refuses,
/// the `count` command.
fn count_documents(name: &str, filter: BsonDocument, mut options: BsonDocument) -> BsonDocument {
    let skip = options.remove("skip");
    let limit = options.remove("limit");
    if crate::literal::runs_javascript(&convert::value(&Bson::Document(filter.clone()))) {
        let mut command = doc! { "count": name, "query": filter };
        if let Some(skip) = skip {
            command.insert("skip", skip);
        }
        if let Some(limit) = limit {
            command.insert("limit", limit);
        }
        return with(command, options);
    }
    let mut pipeline = vec![doc! { "$match": filter }];
    if let Some(skip) = skip {
        pipeline.push(doc! { "$skip": skip });
    }
    if let Some(limit) = limit {
        pipeline.push(doc! { "$limit": limit });
    }
    pipeline.push(doc! { "$group": { "_id": 1, "n": { "$sum": 1 } } });
    with(
        doc! { "aggregate": name, "pipeline": pipeline, "cursor": {} },
        options,
    )
}

/// An update's or a delete's options: those of the statement itself, and
/// those of the command.
fn statement_options(options: BsonDocument) -> (BsonDocument, BsonDocument) {
    const STATEMENT: &[&str] = &["upsert", "arrayFilters", "collation", "hint", "sort"];
    let (mut statement, mut command) = (BsonDocument::new(), BsonDocument::new());
    for (key, value) in options {
        match STATEMENT.contains(&key.as_str()) {
            true => statement.insert(key, value),
            false => command.insert(key, value),
        };
    }
    (statement, command)
}

/// mongosh's names for `findAndModify`'s options.
fn find_and_modify_options(options: BsonDocument) -> BsonDocument {
    let mut command = BsonDocument::new();
    for (key, value) in options {
        match key.as_str() {
            "projection" => {
                command.insert("fields", value);
            }
            "returnDocument" => {
                command.insert("new", value.as_str() == Some("after"));
            }
            "returnNewDocument" => {
                command.insert("new", value);
            }
            _ => {
                command.insert(key, value);
            }
        }
    }
    command
}

/// `status_1_total_-1`, as the server and mongosh name an index.
fn index_name(keys: &BsonDocument) -> String {
    keys.iter()
        .map(|(key, value)| {
            let value = match value {
                Bson::Int32(n) => n.to_string(),
                Bson::Int64(n) => n.to_string(),
                Bson::Double(x) if x.fract() == 0.0 => format!("{}", *x as i64),
                Bson::String(kind) => kind.clone(),
                other => other.to_string(),
            };
            format!("{key}_{value}")
        })
        .collect::<Vec<_>>()
        .join("_")
}

fn database_method(database: &str, method: DbMethod, args: &[Bson]) -> Result<Built, QueryError> {
    let built = |command: BsonDocument, reply: Reply| Built {
        database: database.into(),
        command,
        reply,
    };
    let name = |n: usize| match args.get(n) {
        Some(Bson::String(name)) => Ok(name.clone()),
        _ => Err(unsupported("Name the collection in quotes")),
    };
    Ok(match method {
        DbMethod::CreateCollection => built(
            with(doc! { "create": name(0)? }, document(args, 1)),
            Reply::Answer,
        ),
        DbMethod::CreateView => {
            let pipeline = args.get(2).cloned().unwrap_or(Bson::Array(Vec::new()));
            let command = doc! { "create": name(0)?, "viewOn": name(1)?, "pipeline": pipeline };
            built(with(command, document(args, 3)), Reply::Answer)
        }
        DbMethod::DropDatabase => built(
            with(doc! { "dropDatabase": 1 }, document(args, 0)),
            Reply::Answer,
        ),
        DbMethod::Stats => {
            let mut command = doc! { "dbStats": 1 };
            match args.first() {
                Some(Bson::Document(options)) => command = with(command, options.clone()),
                Some(scale) => {
                    command.insert("scale", scale.clone());
                }
                None => {}
            }
            built(command, Reply::Answer)
        }
        DbMethod::GetCollectionNames => collections(database),
    })
}

/// The command explained instead of run, `queryPlanner` unless the
/// verbosity is named.
fn explain(built: Built, args: &[Bson]) -> Built {
    let verbosity = match args.first() {
        Some(Bson::String(verbosity)) => verbosity.clone(),
        Some(Bson::Boolean(true)) => "allPlansExecution".into(),
        _ => "queryPlanner".into(),
    };
    Built {
        database: built.database,
        command: doc! { "explain": built.command, "verbosity": verbosity },
        reply: Reply::Answer,
    }
}

#[cfg(test)]
mod tests;
