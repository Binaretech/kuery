//! The console's language: the commands of mongosh people paste from the
//! documentation and their own code (`db.orders.find({status: "paid"})
//! .sort({_id: -1})`), read and never evaluated.
//!
//! A script splits into statements at `;` and at line ends outside
//! brackets (a line that starts with `.` goes on with the one before), and
//! each statement reads as one [`Op`]: `use x`, `show dbs|collections`, a
//! collection's method with its cursor modifiers, a database's method, or
//! `db.runCommand`/`db.adminCommand`. Everything else is JavaScript, which
//! Kuery does not run: variables, functions, loops, `forEach`, `load()`,
//! `db.eval`, `mapReduce` and `Mongo()` are refused with [`JAVASCRIPT`].
//! Reading never runs anything, so the classifier can trust what an
//! operation does from its parts, and a read-only connection stays one.

mod complete;
mod parse;
mod print;

#[cfg(test)]
mod tests;

use kuery_core::document::value::{CollectionRef, Document, Value};

pub use complete::context;
pub use parse::statements;
pub use print::{collection_ref, print};

/// What a statement that is not a MongoDB command is refused with.
pub const JAVASCRIPT: &str =
    "Kuery runs MongoDB commands, not JavaScript: one command per statement";

/// One statement of the console.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// `use shop`: the database later statements work in.
    Use(String),
    Show(Show),
    /// `db.orders.find(…).sort(…)`. The target is `[collection]` in the
    /// database in use, or `[database, collection]` through
    /// `db.getSiblingDB("x")`.
    Collection {
        target: CollectionRef,
        method: Method,
        args: Vec<Value>,
        /// Cursor modifiers, in order; `explain` comes last.
        chain: Vec<(Cursor, Vec<Value>)>,
    },
    /// `db.createCollection("x")`, in the database in use or the one
    /// named.
    Database {
        database: Option<String>,
        method: DbMethod,
        args: Vec<Value>,
    },
    /// `db.runCommand({…})`, or `db.adminCommand({…})` (`admin`), which
    /// runs in `admin` whatever the database.
    RunCommand {
        database: Option<String>,
        admin: bool,
        command: Document,
    },
}

/// `show dbs` and `show collections`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Show {
    Databases,
    Collections,
}

/// What an argument must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arg {
    Document,
    /// A pipeline, a list of documents or of index keys.
    Array,
    String,
    Number,
    /// A document or a pipeline (an update), a name or keys (an index)…
    Any,
}

macro_rules! methods {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $text:literal ($min:literal; $($arg:ident),*)),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),* }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),*];

            /// The name mongosh calls it by.
            pub fn name(self) -> &'static str {
                match self { $($name::$variant => $text),* }
            }

            pub(crate) fn named(name: &str) -> Option<$name> {
                match name { $($text => Some($name::$variant),)* _ => None }
            }

            /// The arguments it takes, and how many it needs.
            pub(crate) fn args(self) -> (usize, &'static [Arg]) {
                match self { $($name::$variant => ($min, &[$(Arg::$arg),*])),* }
            }
        }
    };
}

methods! {
    /// A collection's methods.
    Method {
        Find = "find" (0; Document, Document, Document),
        FindOne = "findOne" (0; Document, Document, Document),
        Aggregate = "aggregate" (0; Array, Document),
        CountDocuments = "countDocuments" (0; Document, Document),
        EstimatedDocumentCount = "estimatedDocumentCount" (0; Document),
        Distinct = "distinct" (1; String, Document, Document),
        GetIndexes = "getIndexes" (0;),
        Stats = "stats" (0; Any),
        InsertOne = "insertOne" (1; Document, Document),
        InsertMany = "insertMany" (1; Array, Document),
        UpdateOne = "updateOne" (2; Document, Any, Document),
        UpdateMany = "updateMany" (2; Document, Any, Document),
        ReplaceOne = "replaceOne" (2; Document, Document, Document),
        DeleteOne = "deleteOne" (1; Document, Document),
        DeleteMany = "deleteMany" (1; Document, Document),
        FindOneAndUpdate = "findOneAndUpdate" (2; Document, Any, Document),
        FindOneAndReplace = "findOneAndReplace" (2; Document, Document, Document),
        FindOneAndDelete = "findOneAndDelete" (1; Document, Document),
        BulkWrite = "bulkWrite" (1; Array, Document),
        CreateIndex = "createIndex" (1; Document, Document),
        CreateIndexes = "createIndexes" (1; Array, Document),
        DropIndex = "dropIndex" (1; Any),
        DropIndexes = "dropIndexes" (0; Any),
        Drop = "drop" (0; Document),
        RenameCollection = "renameCollection" (1; String, Any),
    }
}

methods! {
    /// A database's methods (`getCollection`, `getSiblingDB`,
    /// `runCommand` and `adminCommand` read as part of the statement).
    DbMethod {
        CreateCollection = "createCollection" (1; String, Document),
        CreateView = "createView" (3; String, String, Array, Document),
        DropDatabase = "dropDatabase" (0; Document),
        Stats = "stats" (0; Any),
        GetCollectionNames = "getCollectionNames" (0;),
    }
}

methods! {
    /// What a `find` cursor takes after it (`.sort(…)`), and `explain`.
    Cursor {
        Sort = "sort" (1; Document),
        Limit = "limit" (1; Number),
        Skip = "skip" (1; Number),
        Projection = "projection" (1; Document),
        Hint = "hint" (1; Any),
        Collation = "collation" (1; Document),
        MaxTimeMs = "maxTimeMS" (1; Number),
        BatchSize = "batchSize" (1; Number),
        Explain = "explain" (0; String),
    }
}

impl Method {
    /// Whether `.explain()` can follow it: reads and the writes the server
    /// explains.
    pub(crate) fn explainable(self) -> bool {
        use Method::*;
        matches!(
            self,
            Find | FindOne
                | Aggregate
                | CountDocuments
                | EstimatedDocumentCount
                | Distinct
                | UpdateOne
                | UpdateMany
                | ReplaceOne
                | DeleteOne
                | DeleteMany
                | FindOneAndUpdate
                | FindOneAndReplace
                | FindOneAndDelete
        )
    }

    /// Whether cursor modifiers other than `explain` can follow it.
    pub(crate) fn has_cursor(self) -> bool {
        self == Method::Find
    }
}
