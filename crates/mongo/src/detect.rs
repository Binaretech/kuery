//! What a MongoDB connection can do, detected when its client opens and
//! never assumed from the product's name: transactions from the server's
//! shape, cancelling from whether the user may list its own operations,
//! and who keeps a read-only connection from writing from the privileges
//! the server reports for the user.

use std::time::Duration;

use bson::{Document, doc};
use kuery_core::document::capabilities::{
    Capabilities, Enforcement, ExplainKind, Preview, Sampling, Transactions,
};
use kuery_core::session::Access;
use mongodb::Client;
use mongodb::error::ErrorKind;

/// What the server said about itself and the user, asked once per client.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Facts {
    pub(crate) hello: Document,
    pub(crate) build: Document,
    /// `connectionStatus` with privileges; `None` when it failed.
    pub(crate) status: Option<Document>,
    /// The user may list its own running operations (`$currentOp`).
    pub(crate) current_op: bool,
    /// Whether the server checks what users may do.
    pub(crate) authorization: Authorization,
}

/// Whether the server checks what each user may do. A server started
/// without authentication still lets users sign in and reports their
/// roles' privileges, while letting anyone do anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Authorization {
    /// It refused the user a command, or its settings turn checking on.
    Enforced,
    /// It answered what only an administrator may ask, or its settings
    /// leave checking off.
    Off,
    /// It did not say.
    Unknown,
}

/// The product behind the protocol, for the server's label only: what a
/// connection can do comes from [`Capabilities`], whatever this says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    MongoDb,
    Atlas,
    DocumentDb,
    CosmosDb,
    FerretDb,
}

impl Flavor {
    fn name(self) -> &'static str {
        match self {
            Flavor::MongoDb => "MongoDB",
            Flavor::Atlas => "MongoDB Atlas",
            Flavor::DocumentDb => "Amazon DocumentDB",
            Flavor::CosmosDb => "Azure Cosmos DB",
            Flavor::FerretDb => "FerretDB",
        }
    }
}

/// What a connection reports about itself to the session.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Detected {
    /// `MongoDB 8.0.4 · replica set rs0`.
    pub(crate) server: String,
    pub(crate) capabilities: Capabilities,
    pub(crate) notice: Option<String>,
}

/// How long the server keeps a transaction open.
const TRANSACTION_LIFETIME: Duration = Duration::from_secs(60);

/// The privilege actions of MongoDB's reading roles (`read`,
/// `readAnyDatabase`, `clusterMonitor`), read from the server. A user whose
/// every action is one of these cannot write; any other action, including
/// one a later server adds, counts as a write.
const READ_ACTIONS: &[&str] = &[
    "allCollectionStats",
    "changeStream",
    "collStats",
    "connPoolStats",
    "dbHash",
    "dbStats",
    "find",
    "getCmdLineOpts",
    "getDatabaseVersion",
    "getDefaultRWConcern",
    "getLog",
    "getParameter",
    "getShardMap",
    "getShardVersion",
    "hostInfo",
    "indexStats",
    "inprog",
    "killCursors",
    "listClusterCatalog",
    "listCollections",
    "listDatabases",
    "listIndexes",
    "listSampledQueries",
    "listSearchIndexes",
    "listSessions",
    "listShards",
    "netstat",
    "operationMetrics",
    "planCacheRead",
    "queryStatsRead",
    "queryStatsReadTransformed",
    "replSetGetConfig",
    "replSetGetStatus",
    "serverStatus",
    "shardedDataDistribution",
    "shardingState",
    "top",
    "useUUID",
];

/// Why only Kuery refuses writes, as [`Enforcement::ClientOnly`] says it.
const NO_AUTHENTICATION: &str = "this server has no authentication";
const NO_USER: &str = "Kuery connected without a user";
const MAY_WRITE: &str = "the user may write";
const UNKNOWN_PRIVILEGES: &str = "the server did not say what the user may do";
const UNKNOWN_AUTHORIZATION: &str = "the server did not say whether it checks what users may do";

/// The notice of a read-only connection only Kuery keeps from writing.
const CLIENT_ONLY: &str = "MongoDB has no read-only session: Kuery refuses writes by itself. For a guarantee, connect as a user with only the read role";

/// Asks the server what it is and what the user may do.
pub(crate) async fn probe(client: &Client) -> mongodb::error::Result<Facts> {
    let admin = client.database("admin");
    let hello = admin.run_command(doc! { "hello": 1 }).await?;
    let build = admin.run_command(doc! { "buildInfo": 1 }).await?;
    let status = admin
        .run_command(doc! { "connectionStatus": 1, "showPrivileges": true })
        .await
        .ok();
    let current_op = admin
        .run_command(doc! {
            "aggregate": 1,
            "pipeline": [{ "$currentOp": {} }, { "$limit": 1 }],
            "cursor": {},
        })
        .await
        .is_ok();
    let authorization = match status.as_ref().map(users) {
        // With no user, a server that checks lists nothing.
        Some(0) => match admin
            .run_command(doc! { "listDatabases": 1, "nameOnly": true })
            .await
        {
            Ok(_) => Authorization::Off,
            Err(_) => Authorization::Enforced,
        },
        // A user: the server's settings, which only an administrator may
        // read where they are checked.
        Some(_) => match admin.run_command(doc! { "getCmdLineOpts": 1 }).await {
            Ok(answer) => configured(&answer),
            Err(error) => match *error.kind {
                ErrorKind::Command(ref command) if command.code == UNAUTHORIZED => {
                    Authorization::Enforced
                }
                _ => Authorization::Unknown,
            },
        },
        None => Authorization::Unknown,
    };
    Ok(Facts {
        hello,
        build,
        status,
        current_op,
        authorization,
    })
}

/// The server's code for a command the user may not run.
const UNAUTHORIZED: i32 = 13;

/// Whether the settings `getCmdLineOpts` reports check users: on with
/// `authorization: enabled`, and with internal authentication (a key file
/// or a cluster authentication mode), which turns it on; off while moving
/// to it (`transitionToAuth`), when anyone may still connect without a
/// user.
fn configured(answer: &Document) -> Authorization {
    let Ok(parsed) = answer.get_document("parsed") else {
        return Authorization::Unknown;
    };
    let Ok(security) = parsed.get_document("security") else {
        return Authorization::Off;
    };
    if security.get_bool("transitionToAuth").unwrap_or(false) {
        return Authorization::Off;
    }
    match security.get_str("authorization") {
        Ok("enabled") => Authorization::Enforced,
        Ok(_) => Authorization::Off,
        Err(_) if security.contains_key("keyFile") || security.contains_key("clusterAuthMode") => {
            Authorization::Enforced
        }
        Err(_) => Authorization::Off,
    }
}

/// What a connection with `access` can do, from what the server said.
/// `hosts` are the names connected to, which tell hosted products apart.
pub(crate) fn detect(facts: &Facts, access: Access, hosts: &[(String, u16)]) -> Detected {
    let flavor = flavor(facts, hosts);
    let replica_set = facts.hello.get_str("setName").is_ok();
    let sharded = facts
        .hello
        .get_str("msg")
        .is_ok_and(|msg| msg == "isdbgrid");
    let transactions = match replica_set || sharded {
        true => Transactions::Multi {
            lifetime: TRANSACTION_LIFETIME,
        },
        false => Transactions::None,
    };
    let read_only = enforcement(facts);
    let capabilities = Capabilities {
        levels: &["Database", "Collection"],
        transactions,
        read_only,
        preview: match transactions {
            Transactions::Multi { .. } => Preview::Undone,
            Transactions::None => Preview::Estimated,
        },
        explain: Some(ExplainKind::Measured),
        activity: facts.current_op,
        kill: facts.current_op,
        sampling: Sampling::Random,
        version_token: false,
    };
    let secondary = facts.hello.get_bool("secondary").unwrap_or(false);
    let notices = [
        (access == Access::ReadOnly && read_only != Enforcement::Server).then_some(CLIENT_ONLY),
        (read_only == Enforcement::ClientOnly(NO_AUTHENTICATION))
            .then_some("This server has no authentication"),
        secondary.then_some("Connected to a secondary"),
        (access == Access::ReadWrite && !replica_set && !sharded).then_some(
            "Standalone server: no transactions, so a script's writes are not all or nothing",
        ),
        (!facts.current_op).then_some(
            "Cancelling only stops waiting: this user cannot stop its operations on the server",
        ),
    ];
    let notice: Vec<&str> = notices.into_iter().flatten().collect();
    Detected {
        server: server_label(flavor, &facts.hello, &facts.build),
        capabilities,
        notice: (!notice.is_empty()).then(|| notice.join(" · ")),
    }
}

/// The server keeps the connection from writing only when it checks what
/// users may do, a user signed in, and none of its privileges is a write,
/// as reported at connect time. With no user, or on a server that does not
/// check, the privilege lists say nothing about who may write, so they
/// never mean read-only; nor does an empty list, which servers that do not
/// keep MongoDB's privileges report.
fn enforcement(facts: &Facts) -> Enforcement {
    let Some(status) = &facts.status else {
        return Enforcement::ClientOnly(UNKNOWN_PRIVILEGES);
    };
    if facts.authorization == Authorization::Off {
        return Enforcement::ClientOnly(NO_AUTHENTICATION);
    }
    if users(status) == 0 {
        return Enforcement::ClientOnly(NO_USER);
    }
    let privileges = status
        .get_document("authInfo")
        .and_then(|info| info.get_array("authenticatedUserPrivileges"));
    let privileges = match privileges {
        Ok(privileges) if !privileges.is_empty() => privileges,
        _ => return Enforcement::ClientOnly(UNKNOWN_PRIVILEGES),
    };
    let reads_only = privileges.iter().all(|privilege| {
        privilege
            .as_document()
            .and_then(|privilege| privilege.get_array("actions").ok())
            .is_some_and(|actions| {
                actions
                    .iter()
                    .all(|action| action.as_str().is_some_and(|a| READ_ACTIONS.contains(&a)))
            })
    });
    match (reads_only, facts.authorization) {
        (false, _) => Enforcement::ClientOnly(MAY_WRITE),
        (true, Authorization::Enforced) => Enforcement::Server,
        (true, _) => Enforcement::ClientOnly(UNKNOWN_AUTHORIZATION),
    }
}

/// Users signed in on the connection.
fn users(status: &Document) -> usize {
    status
        .get_document("authInfo")
        .and_then(|info| info.get_array("authenticatedUsers"))
        .map_or(0, Vec::len)
}

fn flavor(facts: &Facts, hosts: &[(String, u16)]) -> Flavor {
    let ferret = facts
        .build
        .keys()
        .any(|key| key.to_ascii_lowercase().contains("ferretdb"));
    if ferret {
        return Flavor::FerretDb;
    }
    let ends = |suffixes: &[&str]| {
        hosts.iter().any(|(host, _)| {
            let host = host.to_ascii_lowercase();
            suffixes.iter().any(|suffix| host.ends_with(suffix))
        })
    };
    if ends(&[".docdb.amazonaws.com", ".docdb-elastic.amazonaws.com"]) {
        Flavor::DocumentDb
    } else if ends(&[".cosmos.azure.com", ".documents.azure.com"]) {
        Flavor::CosmosDb
    } else if ends(&[".mongodb.net"]) {
        Flavor::Atlas
    } else {
        Flavor::MongoDb
    }
}

/// `MongoDB 8.0.4 · replica set rs0`, `· standalone` or `· sharded cluster`,
/// from `hello` and `buildInfo`.
fn server_label(flavor: Flavor, hello: &Document, build: &Document) -> String {
    let version = build.get_str("version").unwrap_or("?");
    let shape = match (hello.get_str("setName"), hello.get_str("msg")) {
        (Ok(set), _) => format!("replica set {set}"),
        (_, Ok("isdbgrid")) => "sharded cluster".into(),
        _ => "standalone".into(),
    };
    format!("{} {version} · {shape}", flavor.name())
}

#[cfg(test)]
mod tests {
    use bson::Bson;

    use super::*;

    /// `connectionStatus` as the dev server answers it.
    fn status(users: &[&str], actions: &[&[&str]]) -> Option<Document> {
        let privileges: Vec<Bson> = actions
            .iter()
            .map(|actions| {
                Bson::Document(doc! {
                    "resource": { "db": "shop", "collection": "" },
                    "actions": actions.iter().map(|a| Bson::String(a.to_string())).collect::<Vec<_>>(),
                })
            })
            .collect();
        let users: Vec<Bson> = users
            .iter()
            .map(|user| Bson::Document(doc! { "user": *user, "db": "admin" }))
            .collect();
        Some(doc! {
            "authInfo": {
                "authenticatedUsers": users,
                "authenticatedUserRoles": [],
                "authenticatedUserPrivileges": privileges,
            },
            "ok": 1.0,
        })
    }

    fn facts(hello: Document, status: Option<Document>, authorization: Authorization) -> Facts {
        Facts {
            hello,
            build: doc! { "version": "8.0.4" },
            status,
            current_op: true,
            authorization,
        }
    }

    fn replica_set(status: Option<Document>) -> Facts {
        facts(
            doc! { "setName": "rs0", "isWritablePrimary": true },
            status,
            Authorization::Enforced,
        )
    }

    const HOSTS: &[(String, u16)] = &[];

    /// `kuery_ro`, with the `read` role on `shop`.
    const READ: &[&str] = &[
        "changeStream",
        "collStats",
        "dbHash",
        "dbStats",
        "find",
        "killCursors",
        "listCollections",
        "listIndexes",
        "listSearchIndexes",
        "planCacheRead",
    ];

    #[test]
    fn a_read_user_is_kept_read_only_by_the_server() {
        let reader = replica_set(status(&["kuery_ro"], &[READ, &["find"]]));
        let detected = detect(&reader, Access::ReadOnly, HOSTS);
        assert_eq!(detected.capabilities.read_only, Enforcement::Server);
        assert_eq!(detected.notice, None);
        assert_eq!(detected.server, "MongoDB 8.0.4 · replica set rs0");
        assert_eq!(
            detected.capabilities.transactions,
            Transactions::Multi {
                lifetime: TRANSACTION_LIFETIME
            }
        );
        assert_eq!(detected.capabilities.preview, Preview::Undone);
    }

    #[test]
    fn a_user_that_may_write_is_read_only_only_in_kuery() {
        let root = replica_set(status(&["kuery"], &[READ, &["find", "insert", "update"]]));
        let detected = detect(&root, Access::ReadOnly, HOSTS);
        assert_eq!(
            detected.capabilities.read_only,
            Enforcement::ClientOnly(MAY_WRITE)
        );
        assert_eq!(detected.notice.as_deref(), Some(CLIENT_ONLY));
        // Writing anyway: nothing to warn about on a replica set.
        assert_eq!(detect(&root, Access::ReadWrite, HOSTS).notice, None);
        // An action Kuery does not know counts as a write.
        let future = replica_set(status(&["kuery"], &[&["find", "someFutureAction"]]));
        assert_ne!(
            detect(&future, Access::ReadOnly, HOSTS)
                .capabilities
                .read_only,
            Enforcement::Server
        );
    }

    #[test]
    fn a_server_without_authentication_is_never_read_only_on_the_server() {
        let open = facts(
            doc! { "isWritablePrimary": true },
            status(&[], &[]),
            Authorization::Off,
        );
        let detected = detect(&open, Access::ReadOnly, HOSTS);
        assert_eq!(
            detected.capabilities.read_only,
            Enforcement::ClientOnly(NO_AUTHENTICATION)
        );
        assert_eq!(
            detected.notice.as_deref(),
            Some(&*format!(
                "{CLIENT_ONLY} · This server has no authentication"
            ))
        );
        assert_eq!(detected.capabilities.transactions, Transactions::None);
        assert_eq!(detected.capabilities.preview, Preview::Estimated);
        assert_eq!(detected.server, "MongoDB 8.0.4 · standalone");
        let writing = detect(&open, Access::ReadWrite, HOSTS).notice.unwrap();
        assert!(writing.starts_with("This server has no authentication · Standalone server"));
        // No user on a server with authentication: not "no authentication".
        let anonymous = replica_set(status(&[], &[]));
        assert_eq!(
            detect(&anonymous, Access::ReadOnly, HOSTS)
                .capabilities
                .read_only,
            Enforcement::ClientOnly(NO_USER)
        );
        // Privileges unknown: in doubt, warn.
        assert_eq!(
            detect(&replica_set(None), Access::ReadOnly, HOSTS)
                .capabilities
                .read_only,
            Enforcement::ClientOnly(UNKNOWN_PRIVILEGES)
        );
    }

    #[test]
    fn a_read_user_on_a_server_that_does_not_check_is_read_only_only_in_kuery() {
        // A server started without authentication still signs users in and
        // reports their roles' privileges, and lets them write anyway.
        let unchecked = facts(
            doc! { "isWritablePrimary": true },
            status(&["ro"], &[READ]),
            Authorization::Off,
        );
        let detected = detect(&unchecked, Access::ReadOnly, HOSTS);
        assert_eq!(
            detected.capabilities.read_only,
            Enforcement::ClientOnly(NO_AUTHENTICATION)
        );
        assert_eq!(
            detected.notice.as_deref(),
            Some(&*format!(
                "{CLIENT_ONLY} · This server has no authentication"
            ))
        );
        // Not knowing whether it checks is no proof either.
        let mut unknown = replica_set(status(&["kuery_ro"], &[READ]));
        unknown.authorization = Authorization::Unknown;
        let detected = detect(&unknown, Access::ReadOnly, HOSTS);
        assert_eq!(
            detected.capabilities.read_only,
            Enforcement::ClientOnly(UNKNOWN_AUTHORIZATION)
        );
        assert_eq!(detected.notice.as_deref(), Some(CLIENT_ONLY));
    }

    #[test]
    fn a_user_with_no_privileges_listed_is_read_only_only_in_kuery() {
        // What servers that do not keep MongoDB's privileges answer, even
        // for an administrator.
        let mut ferret = replica_set(status(&["admin"], &[]));
        ferret.hello = doc! { "isWritablePrimary": true };
        ferret.build.insert("ferretdbVersion", "v1.24.0");
        let detected = detect(&ferret, Access::ReadOnly, HOSTS);
        assert_eq!(
            detected.capabilities.read_only,
            Enforcement::ClientOnly(UNKNOWN_PRIVILEGES)
        );
        assert!(
            detected
                .notice
                .as_deref()
                .is_some_and(|notice| notice.starts_with(CLIENT_ONLY)),
            "{detected:?}"
        );
    }

    #[test]
    fn reads_whether_the_server_checks_users_from_its_settings() {
        let settings = |security: Option<Document>| {
            let mut parsed = doc! { "net": { "bindIp": "*" } };
            if let Some(security) = security {
                parsed.insert("security", security);
            }
            configured(&doc! { "argv": ["mongod"], "parsed": parsed, "ok": 1.0 })
        };
        // The dev replica set, as root sees it.
        assert_eq!(
            settings(Some(doc! { "keyFile": "/tmp/kuery.key" })),
            Authorization::Enforced
        );
        assert_eq!(
            settings(Some(doc! { "authorization": "enabled" })),
            Authorization::Enforced
        );
        assert_eq!(
            settings(Some(doc! { "clusterAuthMode": "x509" })),
            Authorization::Enforced
        );
        // The default Docker `mongo`.
        assert_eq!(settings(None), Authorization::Off);
        assert_eq!(
            settings(Some(doc! { "authorization": "disabled" })),
            Authorization::Off
        );
        assert_eq!(
            settings(Some(
                doc! { "keyFile": "/tmp/kuery.key", "transitionToAuth": true }
            )),
            Authorization::Off
        );
        assert_eq!(configured(&doc! { "ok": 1.0 }), Authorization::Unknown);
    }

    #[test]
    fn says_what_it_cannot_promise() {
        let mut secondary = replica_set(status(&["kuery_ro"], &[READ]));
        secondary.hello.insert("secondary", true);
        secondary.current_op = false;
        let detected = detect(&secondary, Access::ReadOnly, HOSTS);
        assert!(!detected.capabilities.kill);
        assert_eq!(
            detected.notice.as_deref(),
            Some(
                "Connected to a secondary · Cancelling only stops waiting: this user cannot stop its operations on the server"
            )
        );
        let sharded = facts(
            doc! { "msg": "isdbgrid" },
            status(&["a"], &[READ]),
            Authorization::Enforced,
        );
        let detected = detect(&sharded, Access::ReadWrite, HOSTS);
        assert_eq!(detected.server, "MongoDB 8.0.4 · sharded cluster");
        assert!(matches!(
            detected.capabilities.transactions,
            Transactions::Multi { .. }
        ));
    }

    #[test]
    fn names_the_product_without_changing_what_it_can_do() {
        let reader = replica_set(status(&["kuery_ro"], &[READ]));
        let at = |host: &str| vec![(host.to_string(), 27017)];
        let atlas = detect(&reader, Access::ReadOnly, &at("cluster0.ab1cd.mongodb.net"));
        assert_eq!(atlas.server, "MongoDB Atlas 8.0.4 · replica set rs0");
        let documentdb = detect(
            &reader,
            Access::ReadOnly,
            &at("shop.cluster-x.eu-west-1.docdb.amazonaws.com"),
        );
        assert_eq!(
            documentdb.server,
            "Amazon DocumentDB 8.0.4 · replica set rs0"
        );
        assert_eq!(documentdb.capabilities, atlas.capabilities);
        let cosmos = detect(
            &reader,
            Access::ReadOnly,
            &at("shop.mongo.cosmos.azure.com"),
        );
        assert!(cosmos.server.starts_with("Azure Cosmos DB"));
        let mut ferret = reader.clone();
        ferret.build.insert("ferretdbVersion", "v1.24.0");
        assert!(
            detect(&ferret, Access::ReadOnly, HOSTS)
                .server
                .starts_with("FerretDB")
        );
    }
}
