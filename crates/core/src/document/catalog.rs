//! What a document database holds, as the sidebar tree and the collection
//! tab show it: containers within containers down to collections (a
//! MongoDB database and its collections; a Couchbase bucket, scope and
//! collection), and what Kuery knows about one collection.

use crate::document::value::Document;

/// What a node of the tree is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerKind {
    Database,
    /// A level between the database and its collections (Couchbase).
    Scope,
    Collection,
    /// Read through a stored query; it holds no documents of its own.
    View,
    TimeSeries,
    /// A fixed-size collection that drops its oldest documents.
    Capped,
}

/// A node of the tree, as its parent lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Container {
    pub name: String,
    pub kind: ContainerKind,
    /// Nothing lies under it: a collection, not a database.
    pub leaf: bool,
    /// Documents, as the engine estimates them, when it said.
    pub estimated: Option<u64>,
    /// Bytes on disk, when the engine said.
    pub size: Option<u64>,
}

/// One index of a collection.
#[derive(Debug, Clone, PartialEq)]
pub struct DocIndex {
    pub name: String,
    /// The indexed fields and their order or type (`{status: 1}`), as the
    /// engine wrote them.
    pub keys: Document,
    pub unique: bool,
    /// Leaves out documents without the field.
    pub sparse: bool,
    /// Only documents matching this filter are indexed.
    pub partial: Option<Document>,
    /// Seconds after which a document expires.
    pub ttl: Option<u64>,
    /// Kept up to date but not used by queries.
    pub hidden: bool,
    pub size: Option<u64>,
    /// How often queries used it, when the user may read that.
    pub usage: Option<IndexUsage>,
}

/// How often an index was used, as the engine counted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexUsage {
    /// Operations that used it.
    pub operations: u64,
    /// Since when it counts (milliseconds since 1970, UTC), when the
    /// engine said: counts start over when the server restarts.
    pub since: Option<i64>,
}

/// What Kuery knows about one collection.
#[derive(Debug, Clone, PartialEq)]
pub struct CollectionInfo {
    pub kind: ContainerKind,
    pub indexes: Vec<DocIndex>,
    /// The schema documents must follow, when the engine checks one.
    pub validator: Option<Document>,
    /// Everything else the engine reports about it (a view's pipeline, a
    /// collation), as it reported it.
    pub options: Document,
}
