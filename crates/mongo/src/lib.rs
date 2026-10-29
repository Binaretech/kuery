//! MongoDB driver: a [`DocumentAdapter`] over the `mongodb` crate.
//!
//! The rules (read-only by default, heavy work refused, scripts in one
//! transaction or stopped at the first failure, checked document changes,
//! batches, cancelling) are the session's
//! ([`kuery_core::document::session`]); this crate connects, parses
//! mongosh's commands without evaluating them, guards what a read-only
//! connection sends, and runs one command at a time.
//!
//! The `mongodb` crate is async and expects its futures to run to the end:
//! dropping one mid-operation can leave a connection or a session in an
//! unknown state. So every operation is spawned on the driver's own runtime
//! and the session's thread waits for its task; a cancel stops the
//! operation on the server and keeps waiting. Every adapter opened with the
//! same [`Target`] shares one `Client`, so one connection pool.

mod browse;
mod catalog;
pub mod convert;
mod detect;
pub mod literal;
mod run;
mod uri;
mod wire;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};
use std::time::Duration;

use bson::{Bson, Document as BsonDocument, doc};

use kuery_core::adapter::Canceller;
use kuery_core::document::adapter::{DocumentAdapter, DocumentSink, Outcome};
use kuery_core::document::capabilities::Capabilities;
use kuery_core::document::catalog::{CollectionInfo, Container};
use kuery_core::document::change::{BrowseSpec, Continuation, Count, DocChange};
use kuery_core::document::language::{CompletionContext, Language, ParseError};
use kuery_core::document::value::{CollectionRef, Document};
use kuery_core::driver::{
    ConnectOptions, DocumentDriver, DriverInfo, Family, Field, SshRoute, Tls,
};
use kuery_core::session::{Access, QueryError, RunOptions};
use mongodb::Client;
use mongodb::error::{ErrorKind, WriteFailure};
use mongodb::options::{AuthMechanism, ClientOptions, Credential, Socks5Proxy};
use tokio::runtime::Runtime;

use crate::detect::{Detected, Facts};
use crate::run::{Stop, Tag};

pub use uri::{OPTIONS, parse as parse_uri};

/// The runtime every MongoDB connection waits on: multi-thread, because a
/// `Client` leaves tasks (server monitoring, the pool, `killCursors` when a
/// cursor drops) that must keep running between runs.
fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("kuery-mongo")
            .enable_all()
            .build()
            .expect("start the MongoDB runtime")
    })
}

/// How long a server has to answer before a connection gives up.
const ANSWER: Duration = Duration::from_secs(10);

/// What SSH cannot carry, as the form shows it.
const SINGLE_MEMBER: &str =
    "Through SSH, Kuery reaches a single member: name one host, without SRV";

/// How the connection uses TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TlsMode {
    /// On for SRV (`mongodb+srv://`), off otherwise: the driver's default.
    Default,
    Disable,
    /// Always, checking the server's certificate.
    Require,
}

/// Where and how to connect. Two adapters with equal targets share a
/// client.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Target {
    /// `host` and port, one per member; one entry for SRV.
    pub hosts: Vec<(String, u16)>,
    /// The hosts come from the DNS records of the one named (`mongodb+srv://`).
    pub srv: bool,
    /// The database to start in; empty for none.
    pub database: String,
    pub user: String,
    pub password: Option<String>,
    pub tls: TlsMode,
    /// Allowlisted options (`authSource`, `replicaSet`, `tlsCAFile`…; see
    /// [`OPTIONS`]), `srv` aside.
    pub options: BTreeMap<String, String>,
    /// Through SSH: the local end of a tunnel, or a SOCKS proxy.
    pub tunnelled: bool,
    /// Through a forwarded port, the member it leads to, for messages:
    /// `hosts` is then the tunnel's local end.
    pub forwarded: Option<(String, u16)>,
    /// The local port of a SOCKS proxy through SSH, which dials the hosts by
    /// their names, so TLS checks the certificate against them.
    pub proxy: Option<u16>,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("hosts", &self.hosts)
            .field("srv", &self.srv)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("tls", &self.tls)
            .field("options", &self.options)
            .field("tunnelled", &self.tunnelled)
            .field("forwarded", &self.forwarded)
            .field("proxy", &self.proxy)
            .finish_non_exhaustive()
    }
}

impl Target {
    /// From the connection form: members as `a:27017,b:27017` in the host,
    /// SRV as the `srv` option. Through SSH, which reaches one host and
    /// port, only one member without SRV, connected to directly: at the
    /// local end of a tunnel, or by its name through a SOCKS proxy.
    pub fn from_options(options: &ConnectOptions) -> Result<Target, String> {
        DRIVER.info.check_options(&options.options)?;
        let mut settings = options.options.clone();
        let srv = settings.remove("srv").as_deref() == Some("true");
        let port = match options.port {
            0 => 27017,
            port => port,
        };
        let mut hosts = options
            .host
            .split(',')
            .map(|member| {
                let (host, given) = uri::split_port(member)?;
                if srv && given.is_some() {
                    return Err("An SRV host takes no port".to_string());
                }
                Ok((host.to_string(), given.unwrap_or(port)))
            })
            .collect::<Result<Vec<_>, String>>()
            .map_err(|error| match options.host.trim().is_empty() {
                true => "The host is required".to_string(),
                false => error,
            })?;
        if srv && hosts.len() > 1 {
            return Err("An SRV connection names one host".into());
        }
        let tunnelled = options.tunnel.is_some() || options.proxy.is_some();
        if tunnelled {
            if srv || hosts.len() > 1 {
                return Err(SINGLE_MEMBER.into());
            }
            settings.insert("directConnection".into(), "true".into());
        }
        let mut forwarded = None;
        if let Some(tunnel) = options.tunnel {
            forwarded = hosts.pop();
            hosts = vec![("127.0.0.1".into(), tunnel)];
        }
        Ok(Target {
            hosts,
            srv,
            database: options.database.trim().to_string(),
            user: options.user.trim().to_string(),
            password: options.password.clone().filter(|p| !p.is_empty()),
            tls: match options.tls.as_deref() {
                Some("disable") => TlsMode::Disable,
                Some("require") => TlsMode::Require,
                _ => TlsMode::Default,
            },
            options: settings,
            tunnelled,
            forwarded,
            proxy: options.proxy.filter(|_| options.tunnel.is_none()),
        })
    }

    /// Whether to talk to the one host named rather than discover the
    /// replica set from it: as the options say, else when there is one host
    /// and no replica set named (what mongosh does).
    fn direct(&self) -> bool {
        match self.options.get("directConnection") {
            Some(direct) => direct == "true",
            None => !self.srv && self.hosts.len() == 1 && !self.options.contains_key("replicaSet"),
        }
    }

    /// Why nothing answered: the member as the form names it, through SSH
    /// when the connection goes through it, never the tunnel's local port.
    fn unanswered(&self) -> String {
        let address = match &self.forwarded {
            Some((host, port)) => format!("{}:{port}", bracketed(host)),
            None => self.address(),
        };
        let through = if self.tunnelled { " through SSH" } else { "" };
        format!(
            "{address} did not answer{through} in {} seconds",
            ANSWER.as_secs()
        )
    }

    /// `localhost:27019`, or the members, for messages.
    fn address(&self) -> String {
        self.hosts
            .iter()
            .map(|(host, port)| match self.srv {
                true => host.clone(),
                false => format!("{}:{port}", bracketed(host)),
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The client's options: hosts and settings through a connection
    /// string without credentials (so SRV and its TXT records resolve as
    /// they do everywhere), the credential set apart.
    async fn client_options(&self) -> Result<ClientOptions, String> {
        let mut query = vec![format!("directConnection={}", self.direct())];
        match self.tls {
            TlsMode::Default => {}
            TlsMode::Disable => query.push("tls=false".into()),
            TlsMode::Require => query.push("tls=true".into()),
        }
        for (key, value) in &self.options {
            // Any TLS option turns TLS on, even one that is off.
            let off = value == "false" && key.as_str() == "tlsAllowInvalidCertificates";
            if !off
                && !matches!(
                    key.as_str(),
                    "directConnection" | "authSource" | "authMechanism"
                )
            {
                query.push(format!("{key}={}", uri::encode(value)));
            }
        }
        let scheme = match self.srv {
            true => "mongodb+srv",
            false => "mongodb",
        };
        let string = format!("{scheme}://{}/?{}", self.address(), query.join("&"));
        let mut options = ClientOptions::parse(&string)
            .await
            .map_err(|error| message(&error))?;
        options.app_name = Some("Kuery".into());
        options.socks5_proxy = self.proxy.map(|port| {
            Socks5Proxy::builder()
                .host("127.0.0.1".to_string())
                .port(port)
                .build()
        });
        options.server_selection_timeout = Some(ANSWER);
        options.connect_timeout = Some(ANSWER);
        if !self.database.is_empty() {
            options.default_database = Some(self.database.clone());
        }
        let mechanism = self
            .options
            .get("authMechanism")
            .map(|name| {
                name.parse::<AuthMechanism>()
                    .map_err(|error| message(&error))
            })
            .transpose()?;
        let source = self
            .options
            .get("authSource")
            .cloned()
            .or_else(|| options.credential.as_ref().and_then(|c| c.source.clone()));
        options.credential = match (self.user.is_empty(), &mechanism) {
            (true, None) => None,
            _ => Some(
                Credential::builder()
                    .username((!self.user.is_empty()).then(|| self.user.clone()))
                    .password(self.password.clone())
                    .source(source)
                    .mechanism(mechanism)
                    .build(),
            ),
        };
        Ok(options)
    }
}

/// An IPv6 address in brackets, for a host list.
fn bracketed(host: &str) -> String {
    match host.contains(':') {
        true => format!("[{host}]"),
        false => host.to_string(),
    }
}

/// Holds something whose drop the driver finishes with tasks (a client, its
/// sessions and cursors) and drops it inside the runtime, so those tasks
/// have one to run on.
struct OnRuntime<T>(Option<T>);

impl<T> OnRuntime<T> {
    fn new(value: T) -> Self {
        OnRuntime(Some(value))
    }
}

impl<T> std::ops::Deref for OnRuntime<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.0.as_ref().expect("only taken when dropped")
    }
}

impl<T> Drop for OnRuntime<T> {
    fn drop(&mut self) {
        let _runtime = runtime().enter();
        drop(self.0.take());
    }
}

/// One client, shared by every adapter opened with the same [`Target`].
struct Pool {
    client: OnRuntime<Client>,
    /// What the server said about itself and the user, asked by the first
    /// adapter.
    facts: OnceLock<Facts>,
}

/// The clients open now, by target; one goes away with its last adapter.
static POOLS: LazyLock<Mutex<HashMap<Target, Weak<Pool>>>> = LazyLock::new(Mutex::default);

/// The client for `target`: the open one, or a new one.
async fn pool(target: &Target) -> Result<Arc<Pool>, String> {
    if let Some(pool) = POOLS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(target)
        .and_then(Weak::upgrade)
    {
        return Ok(pool);
    }
    let options = target.client_options().await?;
    let client = Client::with_options(options).map_err(|error| message(&error))?;
    let mut pools = POOLS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    pools.retain(|_, pool| pool.strong_count() > 0);
    // Another thread may have opened one meanwhile: that one is kept.
    if let Some(pool) = pools.get(target).and_then(Weak::upgrade) {
        return Ok(pool);
    }
    let pool = Arc::new(Pool {
        client: OnRuntime::new(client),
        facts: OnceLock::new(),
    });
    pools.insert(target.clone(), Arc::downgrade(&pool));
    Ok(pool)
}

/// MongoDB for the app: what the connection form asks, the console's
/// language, and how a connection opens.
pub static DRIVER: DocumentDriver = DocumentDriver {
    info: DriverInfo {
        id: "mongodb",
        name: "MongoDB",
        aliases: &[
            "mongo",
            "atlas",
            "documentdb",
            "cosmos",
            "ferretdb",
            "percona",
        ],
        family: Family::Document,
        default_port: 27017,
        database: Field {
            label: "Database",
            placeholder: "shop",
            required: false,
            numeric: false,
            help: Some(
                "The database the console starts in; the sidebar lists every database the user can see.",
            ),
        },
        user: Field {
            label: "User",
            placeholder: "kuery",
            required: false,
            numeric: false,
            help: Some("Empty for a server without authentication."),
        },
        tls: Tls::Modes(&["disable", "require"]),
        production_note: "Production connections only read. MongoDB has no read-only session: Kuery refuses writes by itself; for a guarantee, connect as a user with only the read role.",
        file: false,
        host_help: Some(
            "A host, replica set members as a:27017,b:27017, or an Atlas cluster with SRV on. Pasting a mongodb:// connection string fills the form.",
        ),
        options: OPTIONS,
        uri: Some(uri::parse),
        ssh_route: Some(ssh_route),
    },
    language: &LANGUAGE,
    frontends: &[],
    open: |options, access| open(&Target::from_options(options)?, access),
};

/// Where an SSH tunnel leads: to one member, without SRV, at the port it
/// names or the form's; or, when TLS checks the server's name, a SOCKS
/// proxy the driver dials it through by that name. Asked before `ssh`
/// starts, so a host list is refused even when the SSH host is down.
fn ssh_route(options: &ConnectOptions) -> Result<SshRoute, String> {
    let srv = options.options.get("srv").map(String::as_str) == Some("true");
    if srv || options.host.contains(',') {
        return Err(SINGLE_MEMBER.into());
    }
    if uses_tls(options) {
        return Ok(SshRoute::Proxy);
    }
    let (host, given) = uri::split_port(&options.host)?;
    let port = match options.port {
        0 => 27017,
        port => port,
    };
    Ok(SshRoute::Forward {
        host: host.to_string(),
        port: given.unwrap_or(port),
    })
}

/// Whether a connection uses TLS: as chosen, else for SRV and whenever a
/// TLS option is set, which turns it on.
fn uses_tls(options: &ConnectOptions) -> bool {
    match options.tls.as_deref() {
        Some("disable") => false,
        Some("require") => true,
        _ => options
            .options
            .iter()
            .any(|(key, value)| match key.as_str() {
                "srv" | "tlsAllowInvalidCertificates" => value == "true",
                "tlsCAFile" | "tlsCertificateKeyFile" => !value.is_empty(),
                _ => false,
            }),
    }
}

/// mongosh's commands, read and never evaluated. The filter bar and the
/// document editor read its literals already; the console comes later.
pub static LANGUAGE: Language = Language {
    name: "mongosh",
    editor: "javascript",
    analyze: |text| {
        Err(ParseError {
            span: 0..text.len(),
            message: LATER.into(),
        })
    },
    check_filter: literal::check_filter,
    parse_document: literal::parse_document,
    render: literal::render,
    field_filter: literal::field_filter,
    context: |_, _| CompletionContext::Nothing,
    methods: METHODS,
    operators: OPERATORS,
    stages: STAGES,
};

const LATER: &str = "The MongoDB console comes in a later version of Kuery";

/// A collection's methods, for completion after `db.orders.`.
const METHODS: &[&str] = &[
    "find",
    "findOne",
    "aggregate",
    "countDocuments",
    "estimatedDocumentCount",
    "distinct",
    "getIndexes",
    "stats",
    "explain",
    "insertOne",
    "insertMany",
    "updateOne",
    "updateMany",
    "replaceOne",
    "deleteOne",
    "deleteMany",
    "findOneAndUpdate",
    "findOneAndReplace",
    "findOneAndDelete",
    "bulkWrite",
    "createIndex",
    "createIndexes",
    "dropIndex",
    "dropIndexes",
    "drop",
    "renameCollection",
];

/// Query, update and expression operators, for completion in a `$` key.
const OPERATORS: &[&str] = &[
    "$eq",
    "$ne",
    "$gt",
    "$gte",
    "$lt",
    "$lte",
    "$in",
    "$nin",
    "$and",
    "$or",
    "$nor",
    "$not",
    "$exists",
    "$type",
    "$expr",
    "$jsonSchema",
    "$mod",
    "$regex",
    "$options",
    "$text",
    "$search",
    "$all",
    "$elemMatch",
    "$size",
    "$geoWithin",
    "$geoIntersects",
    "$near",
    "$nearSphere",
    "$where",
    "$set",
    "$unset",
    "$inc",
    "$mul",
    "$rename",
    "$setOnInsert",
    "$min",
    "$max",
    "$currentDate",
    "$push",
    "$pull",
    "$pullAll",
    "$addToSet",
    "$pop",
    "$each",
    "$position",
    "$slice",
    "$sort",
    "$sum",
    "$avg",
    "$first",
    "$last",
    "$count",
    "$concat",
    "$cond",
    "$ifNull",
    "$toString",
    "$toInt",
    "$toLong",
    "$toDouble",
    "$toDecimal",
    "$toDate",
    "$dateToString",
    "$arrayElemAt",
    "$filter",
    "$map",
    "$reduce",
    "$mergeObjects",
    "$getField",
    "$setField",
    "$literal",
];

/// Aggregation stages, for completion inside `aggregate([`.
const STAGES: &[&str] = &[
    "$match",
    "$project",
    "$addFields",
    "$set",
    "$unset",
    "$group",
    "$sort",
    "$limit",
    "$skip",
    "$unwind",
    "$lookup",
    "$graphLookup",
    "$unionWith",
    "$facet",
    "$bucket",
    "$bucketAuto",
    "$count",
    "$sortByCount",
    "$replaceRoot",
    "$replaceWith",
    "$sample",
    "$setWindowFields",
    "$densify",
    "$fill",
    "$geoNear",
    "$redact",
    "$documents",
    "$collStats",
    "$indexStats",
    "$currentOp",
    "$out",
    "$merge",
];

/// Numbers adapters, so each one's tags are its own.
static ADAPTERS: AtomicU64 = AtomicU64::new(0);

/// How much longer than the server's own limit Kuery waits for it: the
/// server's `maxTimeMS` answers first.
const SLACK: Duration = Duration::from_secs(5);

/// The commands that take `maxTimeMS`.
const TIMED: &[&str] = &["find", "aggregate", "count", "distinct"];

/// One open connection.
struct Mongo {
    pool: OnRuntime<Arc<Pool>>,
    access: Access,
    detected: Detected,
    /// Tells this adapter's commands from other adapters' on the server.
    id: u64,
    /// Commands sent, for their tags.
    commands: u64,
    stop: Arc<Stop>,
    limits: Limits,
}

/// What [`DocumentAdapter::limit`] set for what runs next.
struct Limits {
    /// The server's time limit; zero for none.
    max_time: Duration,
    first_batch: usize,
    max_batch: usize,
}

impl From<&RunOptions> for Limits {
    fn from(options: &RunOptions) -> Self {
        Limits {
            max_time: options.statement_timeout,
            first_batch: options.first_batch_rows.max(1),
            max_batch: options.max_batch_rows.max(1),
        }
    }
}

/// Connects, and says what it reached: `MongoDB 8.0.4 · replica set rs0`.
pub fn open(target: &Target, access: Access) -> Result<Box<dyn DocumentAdapter>, String> {
    connect(target, access).map(|mongo| Box::new(mongo) as Box<dyn DocumentAdapter>)
}

fn connect(target: &Target, access: Access) -> Result<Mongo, String> {
    let unchecked = target
        .options
        .get("tlsAllowInvalidCertificates")
        .map(String::as_str);
    if access == Access::ReadOnly && unchecked == Some("true") {
        return Err("Invalid certificates are not allowed on production connections".into());
    }
    let pool = runtime().block_on(pool(target))?;
    let client = pool.client.clone();
    let known = pool.facts.get().is_some();
    // The first adapter asks what the server is; the others check it answers.
    let probed = runtime()
        .block_on(runtime().spawn(async move {
            match known {
                true => client
                    .database("admin")
                    .run_command(doc! { "ping": 1 })
                    .await
                    .map(|_| None),
                false => detect::probe(&client).await.map(Some),
            }
        }))
        .map_err(|error| format!("The connection broke: {error}"))?
        .map_err(|error| match *error.kind {
            ErrorKind::ServerSelection { .. } => target.unanswered(),
            _ => message(&error),
        })?;
    let facts = match probed {
        Some(facts) => pool.facts.get_or_init(|| facts),
        None => pool.facts.get().expect("known"),
    };
    let detected = detect::detect(facts, access, &target.hosts);
    Ok(Mongo {
        pool: OnRuntime::new(pool),
        access,
        detected,
        id: ADAPTERS.fetch_add(1, Ordering::Relaxed) + 1,
        commands: 0,
        stop: Arc::default(),
        limits: Limits::from(&RunOptions::default()),
    })
}

/// A driver error in one readable line: the server's own text where there
/// is one.
fn message(error: &mongodb::error::Error) -> String {
    match &*error.kind {
        ErrorKind::Command(command) => command.message.clone(),
        ErrorKind::Write(WriteFailure::WriteError(write)) => write.message.clone(),
        ErrorKind::Write(WriteFailure::WriteConcernError(concern)) => concern.message.clone(),
        ErrorKind::Authentication { message, .. }
        | ErrorKind::InvalidArgument { message, .. }
        | ErrorKind::DnsResolve { message, .. }
        | ErrorKind::InvalidTlsConfig { message, .. }
        | ErrorKind::ServerSelection { message, .. } => message.clone(),
        ErrorKind::Io(io) => io.to_string(),
        _ => error.to_string(),
    }
}

impl Mongo {
    /// A command as it will be sent: checked by [`wire::guard`] on a
    /// read-only connection, tagged, and timed.
    fn prepare(&mut self, mut command: BsonDocument) -> Result<(BsonDocument, Tag), QueryError> {
        if self.access == Access::ReadOnly {
            wire::guard(&command)?;
        }
        self.commands += 1;
        let tag = Tag::new(self.id, self.commands);
        let timed = TIMED.contains(&run::name(&command));
        if timed && !self.limits.max_time.is_zero() && !command.contains_key("maxTimeMS") {
            let millis = i64::try_from(self.limits.max_time.as_millis()).unwrap_or(i64::MAX);
            command.insert("maxTimeMS", millis);
        }
        command.insert("comment", tag.as_str());
        Ok((command, tag))
    }

    /// How long to wait for the server before giving up on it.
    fn patience(&self) -> Option<Duration> {
        (!self.limits.max_time.is_zero()).then(|| self.limits.max_time + SLACK)
    }

    /// Runs a command that answers with one document.
    fn command(
        &mut self,
        database: &str,
        command: BsonDocument,
    ) -> Result<BsonDocument, QueryError> {
        let (command, tag) = self.prepare(command)?;
        let db = self.pool.client.database(database);
        run::wait(
            &self.pool.client,
            &tag,
            &self.stop,
            self.patience(),
            async move { db.run_command(command).await },
        )
    }

    /// Runs a command that answers with a cursor; its documents go to
    /// `sink` as they arrive. Returns how many there were.
    fn cursor(
        &mut self,
        database: &str,
        command: BsonDocument,
        sink: &mut dyn FnMut(Vec<BsonDocument>) -> Result<(), QueryError>,
    ) -> Result<u64, QueryError> {
        let (command, tag) = self.prepare(command)?;
        let db = self.pool.client.database(database);
        let comment = Bson::String(tag.as_str().into());
        let batch = u32::try_from(self.limits.max_batch).unwrap_or(u32::MAX);
        run::stream(
            &self.pool.client,
            &tag,
            &self.stop,
            self.patience(),
            self.limits.first_batch,
            async move {
                db.run_cursor_command(command)
                    .batch_size(batch)
                    .comment(comment)
                    .await
            },
            sink,
        )
    }

    /// Writes need a connection that may write.
    fn writable(&self) -> Result<(), QueryError> {
        match self.access {
            Access::ReadWrite => Ok(()),
            Access::ReadOnly => Err(QueryError::WritesDisabled),
        }
    }
}

impl DocumentAdapter for Mongo {
    fn language(&self) -> &'static Language {
        &LANGUAGE
    }

    fn server(&self) -> String {
        self.detected.server.clone()
    }

    fn capabilities(&self) -> Capabilities {
        self.detected.capabilities.clone()
    }

    fn notice(&self) -> Option<String> {
        self.detected.notice.clone()
    }

    fn limit(&mut self, options: &RunOptions) -> Result<(), QueryError> {
        self.limits = Limits::from(options);
        Ok(())
    }

    fn canceller(&self) -> Canceller {
        let stop = self.stop.clone();
        Arc::new(move || stop.request())
    }

    fn execute(
        &mut self,
        _statement: &str,
        _sink: &mut dyn DocumentSink,
    ) -> Result<Outcome, QueryError> {
        Err(QueryError::Unsupported(LATER.into()))
    }

    /// Refused on a read-only connection whatever the session checked.
    fn begin(&mut self) -> Result<(), QueryError> {
        self.writable()?;
        later()
    }

    fn children(&mut self, parent: &[String]) -> Result<Vec<Container>, QueryError> {
        match parent {
            [] => self.databases(),
            [database] => self.collections(database),
            _ => Ok(Vec::new()),
        }
    }

    fn collection(&mut self, collection: &CollectionRef) -> Result<CollectionInfo, QueryError> {
        self.describe(collection)
    }

    /// Documents picked at random; the first ones of a view, whose
    /// pipeline would otherwise run over all of it.
    fn sample(
        &mut self,
        collection: &CollectionRef,
        n: usize,
        sink: &mut dyn DocumentSink,
    ) -> Result<(), QueryError> {
        let (database, name) = catalog::split(collection)?;
        let size = i64::try_from(n).unwrap_or(i64::MAX);
        let stage = match self.is_view(collection)? {
            true => doc! { "$limit": size },
            false => doc! { "$sample": { "size": size } },
        };
        let command = doc! { "aggregate": name, "pipeline": [stage], "cursor": {} };
        self.cursor(database, command, &mut |batch| {
            sink.documents(batch.iter().map(convert::document).collect())
        })?;
        Ok(())
    }

    fn browse(
        &mut self,
        collection: &CollectionRef,
        spec: &BrowseSpec,
        sink: &mut dyn DocumentSink,
    ) -> Result<Option<Continuation>, QueryError> {
        let (database, name) = catalog::split(collection)?;
        let command = browse::find(name, spec, self.limits.first_batch)?;
        let (mut returned, mut last) = (0, None);
        self.cursor(database, command, &mut |batch| {
            let documents: Vec<Document> = batch.iter().map(convert::document).collect();
            returned += documents.len();
            if let Some(document) = documents.last() {
                last = document.get("_id").cloned();
            }
            sink.documents(documents)
        })?;
        Ok(browse::next(spec, returned, last))
    }

    /// Without a filter, the count the server keeps; with one, counted up to
    /// `cap` for a few seconds, and unknown after them.
    fn count(
        &mut self,
        collection: &CollectionRef,
        filter: Option<&str>,
        cap: u64,
    ) -> Result<Count, QueryError> {
        let (database, name) = catalog::split(collection)?;
        let filter = browse::bar(filter)?;
        let counted = match filter {
            None => self
                .command(database, doc! { "count": name })
                .map(|answer| Count::Exact(answer.get("n").and_then(catalog::number).unwrap_or(0))),
            Some(filter) => {
                self.count_matching(database, name, filter, cap)
                    .map(|counted| match cap > 0 && counted > cap {
                        true => Count::AtLeast(cap),
                        false => Count::Exact(counted),
                    })
            }
        };
        match counted {
            Ok(count) => Ok(count),
            Err(QueryError::Database { code, .. }) if code == "50" => Ok(Count::Unknown),
            Err(error) => Err(error),
        }
    }

    /// Refused on a read-only connection whatever the session checked.
    fn apply(
        &mut self,
        _collection: &CollectionRef,
        _change: &DocChange,
    ) -> Result<Outcome, QueryError> {
        self.writable()?;
        later()
    }
}

fn later<T>() -> Result<T, QueryError> {
    Err(QueryError::Unsupported(
        "Kuery does not change MongoDB documents yet".into(),
    ))
}

#[cfg(test)]
mod unit {
    use super::*;

    fn options(host: &str) -> ConnectOptions {
        ConnectOptions {
            host: host.into(),
            port: 27017,
            database: " shop ".into(),
            user: "kuery".into(),
            password: Some("kuery".into()),
            ..ConnectOptions::default()
        }
    }

    fn with(mut options: ConnectOptions, pairs: &[(&str, &str)]) -> ConnectOptions {
        for (key, value) in pairs {
            options.options.insert(key.to_string(), value.to_string());
        }
        options
    }

    #[test]
    fn reads_members_and_their_ports() {
        let target = Target::from_options(&options("a, b:27018,[::1]:27020")).unwrap();
        assert_eq!(
            target.hosts,
            [
                ("a".to_string(), 27017),
                ("b".to_string(), 27018),
                ("::1".to_string(), 27020)
            ]
        );
        assert_eq!(target.database, "shop");
        assert_eq!(target.address(), "a:27017,b:27018,[::1]:27020");
        // Several members: the replica set is discovered.
        assert!(!target.direct());
        assert!(
            Target::from_options(&options(" "))
                .unwrap_err()
                .contains("required")
        );
    }

    #[test]
    fn connects_directly_to_one_host_without_a_replica_set() {
        assert!(
            Target::from_options(&options("localhost:27019"))
                .unwrap()
                .direct()
        );
        let named = with(options("localhost"), &[("replicaSet", "rs0")]);
        assert!(!Target::from_options(&named).unwrap().direct());
        let srv = with(options("cluster0.x.mongodb.net"), &[("srv", "true")]);
        let srv = Target::from_options(&srv).unwrap();
        assert!(srv.srv && !srv.direct());
        assert!(!srv.options.contains_key("srv"));
    }

    #[test]
    fn reaches_one_member_through_a_tunnel() {
        let tunnelled = ConnectOptions {
            tunnel: Some(40000),
            ..with(
                options("db.internal:27017"),
                &[("directConnection", "false")],
            )
        };
        let target = Target::from_options(&tunnelled).unwrap();
        assert_eq!(target.hosts, [("127.0.0.1".to_string(), 40000)]);
        assert_eq!(target.options["directConnection"], "true");
        assert!(target.tunnelled && target.direct());
        // A timeout names the member, not the tunnel's local port.
        assert_eq!(
            target.unanswered(),
            "db.internal:27017 did not answer through SSH in 10 seconds"
        );
        for refused in [
            with(options("cluster0.x.mongodb.net"), &[("srv", "true")]),
            options("a:27017,b:27017"),
        ] {
            let refused = ConnectOptions {
                tunnel: Some(40000),
                ..refused
            };
            assert_eq!(Target::from_options(&refused).unwrap_err(), SINGLE_MEMBER);
        }
    }

    #[test]
    fn reaches_one_member_by_name_through_a_proxy() {
        let proxied = ConnectOptions {
            proxy: Some(40000),
            tls: Some("require".into()),
            ..options("mongo-tls:27017")
        };
        let target = Target::from_options(&proxied).unwrap();
        // The name, not the proxy's address: TLS checks it.
        assert_eq!(target.hosts, [("mongo-tls".to_string(), 27017)]);
        assert_eq!(target.proxy, Some(40000));
        assert!(target.tunnelled && target.direct());
        let built = runtime().block_on(target.client_options()).unwrap();
        let proxy = built.socks5_proxy.unwrap();
        assert_eq!(
            (proxy.host.as_str(), proxy.port),
            ("127.0.0.1", Some(40000))
        );
        let refused = ConnectOptions {
            proxy: Some(40000),
            ..options("a:27017,b:27017")
        };
        assert_eq!(Target::from_options(&refused).unwrap_err(), SINGLE_MEMBER);
    }

    #[test]
    fn routes_a_tunnel_to_the_member_and_its_own_port() {
        let route = |options: ConnectOptions| ssh_route(&options);
        // The member's own port, not the form's.
        assert_eq!(
            route(options("db.internal:27018")),
            Ok(SshRoute::Forward {
                host: "db.internal".into(),
                port: 27018
            })
        );
        assert_eq!(
            route(options("[::1]")),
            Ok(SshRoute::Forward {
                host: "::1".into(),
                port: 27017
            })
        );
        let tls = ConnectOptions {
            tls: Some("require".into()),
            ..options("mongo-tls:27018")
        };
        assert_eq!(route(tls), Ok(SshRoute::Proxy));
        // Refused before ssh starts.
        for refused in [
            with(options("cluster0.x.mongodb.net"), &[("srv", "true")]),
            options("a:27017,b:27017"),
        ] {
            assert_eq!(route(refused).unwrap_err(), SINGLE_MEMBER);
        }
    }

    #[test]
    fn says_when_a_connection_uses_tls() {
        let tls = |stored: Option<&str>, pairs: &[(&str, &str)]| {
            uses_tls(&ConnectOptions {
                tls: stored.map(str::to_string),
                ..with(options("h"), pairs)
            })
        };
        assert!(!tls(None, &[]));
        assert!(tls(Some("require"), &[]));
        assert!(!tls(Some("disable"), &[("srv", "true")]));
        // SRV and any TLS option turn it on.
        assert!(tls(None, &[("srv", "true")]));
        assert!(tls(None, &[("tlsCAFile", "/ca.pem")]));
        assert!(tls(None, &[("tlsAllowInvalidCertificates", "true")]));
        assert!(!tls(None, &[("authSource", "admin")]));
    }

    #[test]
    fn production_never_skips_the_certificate_check() {
        let unchecked = Target::from_options(&with(
            options("localhost"),
            &[("tlsAllowInvalidCertificates", "true")],
        ))
        .unwrap();
        assert_eq!(
            connect(&unchecked, Access::ReadOnly).err().unwrap(),
            "Invalid certificates are not allowed on production connections"
        );
    }

    #[test]
    fn refuses_options_it_does_not_keep() {
        let unknown = with(
            options("h"),
            &[("maxPoolSize", "5"), ("authSource", "admin")],
        );
        assert_eq!(
            Target::from_options(&unknown).unwrap_err(),
            "Kuery does not keep these options: maxPoolSize"
        );
        let wrong = with(options("h"), &[("readPreference", "anywhere")]);
        assert!(Target::from_options(&wrong).is_err());
    }

    #[test]
    fn maps_the_tls_choice() {
        for (stored, mode) in [
            (None, TlsMode::Default),
            (Some("disable"), TlsMode::Disable),
            (Some("require"), TlsMode::Require),
        ] {
            let options = ConnectOptions {
                tls: stored.map(str::to_string),
                ..options("h")
            };
            assert_eq!(Target::from_options(&options).unwrap().tls, mode);
        }
    }

    #[test]
    fn builds_client_options_without_credentials_in_the_string() {
        let target = Target::from_options(&with(
            options("localhost:27019"),
            &[("authSource", "admin"), ("tlsCAFile", "/etc/ca file.pem")],
        ))
        .unwrap();
        let built = runtime().block_on(target.client_options()).unwrap();
        assert_eq!(built.direct_connection, Some(true));
        assert_eq!(built.app_name.as_deref(), Some("Kuery"));
        let credential = built.credential.unwrap();
        assert_eq!(credential.username.as_deref(), Some("kuery"));
        assert_eq!(credential.source.as_deref(), Some("admin"));
        let anonymous = Target::from_options(&ConnectOptions {
            user: String::new(),
            ..options("localhost")
        })
        .unwrap();
        assert!(
            runtime()
                .block_on(anonymous.client_options())
                .unwrap()
                .credential
                .is_none()
        );
    }
}
