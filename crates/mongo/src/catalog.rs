//! What a MongoDB server holds, as the sidebar and the collection tab show
//! it: the databases the user may see, their collections with their kinds,
//! and one collection's indexes, validator and options. Every command goes
//! through the adapter's own path, so a read-only connection guards it.

use std::collections::HashMap;
use std::time::Duration;

use bson::{Bson, Document as BsonDocument, doc};
use kuery_core::document::catalog::{CollectionInfo, Container, ContainerKind, DocIndex};
use kuery_core::document::value::CollectionRef;
use kuery_core::session::QueryError;

use crate::{Mongo, convert};

/// How long a count with a filter may take before it is unknown.
const COUNT_TIME: Duration = Duration::from_secs(5);

/// Databases MongoDB keeps for itself, listed after the user's.
const SYSTEM_DATABASES: &[&str] = &["admin", "config", "local"];

impl Mongo {
    /// The databases the user may see.
    pub(crate) fn databases(&mut self) -> Result<Vec<Container>, QueryError> {
        let answer = self.command(
            "admin",
            doc! { "listDatabases": 1, "nameOnly": false, "authorizedDatabases": true },
        )?;
        Ok(databases(&answer))
    }

    /// A database's collections, views and time series, `system.*` aside.
    pub(crate) fn collections(&mut self, database: &str) -> Result<Vec<Container>, QueryError> {
        let listed = self.list_collections(database, doc! {})?;
        Ok(collections(&listed))
    }

    /// Everything about one collection.
    pub(crate) fn describe(
        &mut self,
        collection: &CollectionRef,
    ) -> Result<CollectionInfo, QueryError> {
        let (database, name) = split(collection)?;
        let listed = self.list_collections(database, doc! { "name": name })?;
        let Some(info) = listed.first() else {
            return Err(missing(collection));
        };
        let kind = kind(info);
        let options = info.get_document("options").cloned().unwrap_or_default();
        let validator = options.get_document("validator").ok().map(|validator| {
            convert::document(validator.get_document("$jsonSchema").unwrap_or(validator))
        });
        let indexes = match kind {
            // A view has no indexes of its own.
            ContainerKind::View => Vec::new(),
            _ => self.indexes(database, name)?,
        };
        Ok(CollectionInfo {
            kind,
            indexes,
            validator,
            options: convert::document(&options),
        })
    }

    /// Documents matching `filter`, counted no further than `cap` (none
    /// for 0) and for at most [`COUNT_TIME`]. Through an aggregation, which
    /// a sharded cluster answers without orphaned documents; `$where`, at
    /// any depth, only works in the `count` command.
    pub(crate) fn count_matching(
        &mut self,
        database: &str,
        name: &str,
        filter: BsonDocument,
        cap: u64,
    ) -> Result<u64, QueryError> {
        let millis = i64::try_from(COUNT_TIME.as_millis()).unwrap_or(i64::MAX);
        let limit = (cap > 0).then(|| i64::try_from(cap + 1).unwrap_or(i64::MAX));
        if has_where(&filter) {
            let mut command = doc! { "count": name, "query": filter, "maxTimeMS": millis };
            if let Some(limit) = limit {
                command.insert("limit", limit);
            }
            let answer = self.command(database, command)?;
            return Ok(answer.get("n").and_then(number).unwrap_or(0));
        }
        let mut pipeline = vec![doc! { "$match": filter }];
        if let Some(limit) = limit {
            pipeline.push(doc! { "$limit": limit });
        }
        pipeline.push(doc! { "$count": "n" });
        let command = doc! {
            "aggregate": name,
            "pipeline": pipeline,
            "cursor": {},
            "maxTimeMS": millis,
        };
        let mut counted = 0;
        self.cursor(database, command, &mut |batch| {
            if let Some(n) = batch.first().and_then(|n| n.get("n")).and_then(number) {
                counted = n;
            }
            Ok(())
        })?;
        Ok(counted)
    }

    /// Whether `collection` is a view, which is read through its pipeline.
    pub(crate) fn is_view(&mut self, collection: &CollectionRef) -> Result<bool, QueryError> {
        let (database, name) = split(collection)?;
        let listed = self.list_collections(database, doc! { "name": name })?;
        Ok(listed
            .first()
            .is_some_and(|info| kind(info) == ContainerKind::View))
    }

    fn list_collections(
        &mut self,
        database: &str,
        filter: BsonDocument,
    ) -> Result<Vec<BsonDocument>, QueryError> {
        let command = doc! {
            "listCollections": 1,
            "filter": filter,
            "nameOnly": false,
            "authorizedCollections": true,
        };
        let mut listed = Vec::new();
        self.cursor(database, command, &mut |batch| {
            listed.extend(batch);
            Ok(())
        })?;
        Ok(listed)
    }

    fn indexes(&mut self, database: &str, name: &str) -> Result<Vec<DocIndex>, QueryError> {
        let mut listed = Vec::new();
        self.cursor(database, doc! { "listIndexes": name }, &mut |batch| {
            listed.extend(batch);
            Ok(())
        })?;
        let sizes = self.index_sizes(database, name)?;
        Ok(listed
            .iter()
            .map(|index| self::index(index, &sizes))
            .collect())
    }

    /// Each index's size on disk, when the user may read the collection's
    /// statistics; none otherwise.
    fn index_sizes(
        &mut self,
        database: &str,
        name: &str,
    ) -> Result<HashMap<String, u64>, QueryError> {
        let command = doc! {
            "aggregate": name,
            "pipeline": [{ "$collStats": { "storageStats": {} } }],
            "cursor": {},
        };
        let mut stats = Vec::new();
        let read = self.cursor(database, command, &mut |batch| {
            stats.extend(batch);
            Ok(())
        });
        match read {
            Ok(_) => {}
            Err(QueryError::Database { .. }) => return Ok(HashMap::new()),
            Err(error) => return Err(error),
        }
        let mut sizes = HashMap::new();
        // One document per shard: their sizes add up.
        for stats in &stats {
            let Ok(indexes) = stats
                .get_document("storageStats")
                .and_then(|storage| storage.get_document("indexSizes"))
            else {
                continue;
            };
            for (index, size) in indexes {
                *sizes.entry(index.clone()).or_default() += number(size).unwrap_or(0);
            }
        }
        Ok(sizes)
    }
}

/// `[database, collection]`, the only path a MongoDB collection has.
pub(crate) fn split(collection: &CollectionRef) -> Result<(&str, &str), QueryError> {
    match collection.path.as_slice() {
        [database, name] => Ok((database, name)),
        _ => Err(QueryError::Unsupported(format!(
            "A MongoDB collection is named by its database and its name, not {}",
            collection.display()
        ))),
    }
}

pub(crate) fn missing(collection: &CollectionRef) -> QueryError {
    QueryError::Database {
        code: "26".into(),
        message: format!("{} does not exist", collection.display()),
    }
}

/// `listDatabases` as the tree's top level: the user's databases by name,
/// then MongoDB's own.
fn databases(answer: &BsonDocument) -> Vec<Container> {
    let mut databases: Vec<Container> = answer
        .get_array("databases")
        .map(|listed| {
            listed
                .iter()
                .filter_map(Bson::as_document)
                .filter_map(|database| {
                    Some(Container {
                        name: database.get_str("name").ok()?.to_string(),
                        kind: ContainerKind::Database,
                        leaf: false,
                        estimated: None,
                        size: database.get("sizeOnDisk").and_then(number),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    databases.sort_by(|a, b| {
        let system = |name: &str| SYSTEM_DATABASES.contains(&name);
        (system(&a.name), &a.name).cmp(&(system(&b.name), &b.name))
    });
    databases
}

/// `listCollections` as a database's children, by name. Counts are left
/// to the tree, which asks for the collections it shows.
fn collections(listed: &[BsonDocument]) -> Vec<Container> {
    let mut collections: Vec<Container> = listed
        .iter()
        .filter_map(|info| {
            let name = info.get_str("name").ok()?;
            (!name.starts_with("system.")).then(|| Container {
                name: name.to_string(),
                kind: kind(info),
                leaf: true,
                estimated: None,
                size: None,
            })
        })
        .collect();
    collections.sort_by(|a, b| a.name.cmp(&b.name));
    collections
}

/// A collection's kind, from what `listCollections` says of it.
fn kind(info: &BsonDocument) -> ContainerKind {
    match info.get_str("type") {
        Ok("view") => ContainerKind::View,
        Ok("timeseries") => ContainerKind::TimeSeries,
        _ => {
            let capped = info
                .get_document("options")
                .ok()
                .and_then(|options| options.get("capped"))
                .is_some_and(truthy);
            match capped {
                true => ContainerKind::Capped,
                false => ContainerKind::Collection,
            }
        }
    }
}

fn index(index: &BsonDocument, sizes: &HashMap<String, u64>) -> DocIndex {
    let name = index.get_str("name").unwrap_or_default().to_string();
    let flag = |key: &str| index.get(key).is_some_and(truthy);
    DocIndex {
        keys: index
            .get_document("key")
            .map(convert::document)
            .unwrap_or_default(),
        unique: flag("unique"),
        sparse: flag("sparse"),
        partial: index
            .get_document("partialFilterExpression")
            .ok()
            .map(convert::document),
        ttl: index.get("expireAfterSeconds").and_then(number),
        hidden: flag("hidden"),
        size: sizes.get(&name).copied(),
        name,
    }
}

/// Whether a filter holds `$where`, which `$match` refuses, anywhere: at
/// its top or under `$or`, `$and` or `$nor`.
fn has_where(filter: &BsonDocument) -> bool {
    fn within(value: &Bson) -> bool {
        match value {
            Bson::Document(document) => has_where(document),
            Bson::Array(items) => items.iter().any(within),
            _ => false,
        }
    }
    filter
        .iter()
        .any(|(key, value)| key == "$where" || within(value))
}

/// A count or a size, whichever number type the server used.
pub(crate) fn number(value: &Bson) -> Option<u64> {
    match value {
        Bson::Int32(n) => u64::try_from(*n).ok(),
        Bson::Int64(n) => u64::try_from(*n).ok(),
        Bson::Double(x) if *x >= 0.0 => Some(*x as u64),
        _ => None,
    }
}

/// A flag as old servers also wrote it: `1` for `true`.
fn truthy(value: &Bson) -> bool {
    match value {
        Bson::Boolean(flag) => *flag,
        Bson::Int32(n) => *n != 0,
        Bson::Int64(n) => *n != 0,
        Bson::Double(x) => *x != 0.0,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use kuery_core::document::value::Value;

    use super::*;

    #[test]
    fn lists_the_users_databases_first() {
        let answer = doc! { "databases": [
            { "name": "admin", "sizeOnDisk": 40960_i64, "empty": false },
            { "name": "shop", "sizeOnDisk": 2_048_000_i64, "empty": false },
            { "name": "config", "sizeOnDisk": 12288.0, "empty": false },
            { "name": "analytics", "sizeOnDisk": 8192, "empty": false },
        ] };
        let listed = databases(&answer);
        let names: Vec<&str> = listed.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["analytics", "shop", "admin", "config"]);
        assert_eq!(listed[1].size, Some(2_048_000));
        assert!(
            listed
                .iter()
                .all(|d| !d.leaf && d.kind == ContainerKind::Database)
        );
    }

    #[test]
    fn tells_collections_by_their_kind() {
        let listed = [
            doc! { "name": "orders", "type": "collection", "options": {} },
            doc! { "name": "paid_orders", "type": "view", "options": { "viewOn": "orders", "pipeline": [] } },
            doc! { "name": "readings", "type": "timeseries", "options": { "timeseries": {} } },
            doc! { "name": "events", "type": "collection", "options": { "capped": true, "size": 1_048_576 } },
            doc! { "name": "system.views", "type": "collection", "options": {} },
            doc! { "name": "system.buckets.readings", "type": "collection", "options": {} },
        ];
        let kinds: Vec<(String, ContainerKind)> = collections(&listed)
            .into_iter()
            .map(|c| (c.name, c.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                ("events".into(), ContainerKind::Capped),
                ("orders".into(), ContainerKind::Collection),
                ("paid_orders".into(), ContainerKind::View),
                ("readings".into(), ContainerKind::TimeSeries),
            ]
        );
    }

    #[test]
    fn reads_an_index() {
        let sizes = HashMap::from([("email_1".to_string(), 20480)]);
        let read = index(
            &doc! {
                "v": 2,
                "key": { "email": 1 },
                "name": "email_1",
                "unique": true,
                "partialFilterExpression": { "email": { "$exists": true } },
                "expireAfterSeconds": 3600,
                "sparse": 1,
            },
            &sizes,
        );
        assert_eq!(read.name, "email_1");
        assert_eq!(read.keys.get("email"), Some(&Value::Int32(1)));
        assert!(read.unique && read.sparse && !read.hidden);
        assert!(read.partial.is_some());
        assert_eq!((read.ttl, read.size), (Some(3600), Some(20480)));
    }

    #[test]
    fn finds_where_at_any_depth() {
        assert!(has_where(&doc! { "$where": "this.n < 10" }));
        assert!(has_where(
            &doc! { "$or": [{ "_id": 1 }, { "$where": "true" }] }
        ));
        assert!(has_where(
            &doc! { "$and": [{ "$nor": [{ "$where": "false" }] }] }
        ));
        assert!(!has_where(
            &doc! { "status": "paid", "items": { "$elemMatch": { "qty": 1 } } }
        ));
        // A value that reads `$where` is no operator.
        assert!(!has_where(&doc! { "note": "$where" }));
    }

    #[test]
    fn names_collections_by_database_and_name() {
        let orders = CollectionRef::new(&["shop", "orders"]);
        assert_eq!(split(&orders).unwrap(), ("shop", "orders"));
        assert!(split(&CollectionRef::new(&["shop"])).is_err());
    }
}
