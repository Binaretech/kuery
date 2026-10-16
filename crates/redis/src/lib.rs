//! Redis driver.
//!
//! Runs on its own tokio runtime, separate from the UI thread, like the
//! Postgres driver. Redis picks the database per connection, so a session
//! keeps one connection per database it has used; each reconnects by itself
//! if the server drops it.
//!
//! The explorer and the key viewer only ever send commands that read
//! (`INFO`, `SCAN`, `TYPE`, `HSCAN`, `LRANGE`…). Commands typed in the
//! console go through [`Session::command`]; deciding which of those may run
//! is the console's job (see `kuery_core::redis::classify`).

mod reply;

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::{ConnectionAddr, ConnectionInfo, IntoConnectionInfo as _, RedisConnectionInfo, Value};
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

pub use reply::{Reply, Table, display_bytes};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
/// Keys asked for per `SCAN` round trip.
const SCAN_COUNT: usize = 500;
/// `SCAN` round trips per page at most, so a pattern that matches little
/// returns (with a cursor to go on) instead of walking a huge keyspace.
const SCAN_ROUNDS: usize = 100;
/// The most of a string value read at once.
pub const MAX_STRING: usize = 1 << 20;

/// Background runtime where the connections live.
pub struct Engine {
    runtime: Runtime,
}

impl Engine {
    pub fn new() -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("kuery-redis")
            .enable_all()
            .build()?;
        Ok(Self { runtime })
    }

    /// Connects and reads the server's version. The returned future can be
    /// awaited from any executor, including the UI one.
    pub fn connect(
        &self,
        target: Target,
    ) -> impl Future<Output = Result<Session, Error>> + Send + use<> {
        let handle = self.runtime.handle().clone();
        run(&handle.clone(), async move {
            let info = target.connection_info()?;
            let mut inner = Inner {
                info,
                default_db: target.db,
                handle,
                connections: Mutex::new(HashMap::new()),
                server: ServerInfo::default(),
            };
            let mut connection = inner.connection(target.db).await?;
            let text: String = redis::cmd("INFO")
                .arg("server")
                .query_async(&mut connection)
                .await?;
            let field = |name: &str| {
                text.lines()
                    .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
                    .map(|value| value.trim().to_string())
                    .unwrap_or_default()
            };
            // CONFIG may be denied to the user; 16 is the server's default.
            let databases = redis::cmd("CONFIG")
                .arg("GET")
                .arg("databases")
                .query_async::<Vec<String>>(&mut connection)
                .await
                .ok()
                .and_then(|pair| pair.get(1)?.parse().ok())
                .unwrap_or(16);
            inner.server = ServerInfo {
                version: field("redis_version"),
                mode: field("redis_mode"),
                databases,
            };
            Ok(Session {
                inner: Arc::new(inner),
            })
        })
    }
}

/// Runs `future` on the driver's runtime and returns a future for its
/// result that any executor can await.
fn run<T, F>(
    handle: &tokio::runtime::Handle,
    future: F,
) -> impl Future<Output = Result<T, Error>> + Send + use<T, F>
where
    T: Send + 'static,
    F: Future<Output = Result<T, Error>> + Send + 'static,
{
    let task = handle.spawn(future);
    async move {
        match task.await {
            Ok(result) => result,
            Err(join_error) => Err(Error::Runtime(join_error.to_string())),
        }
    }
}

/// Where and how to connect.
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    /// The database the console starts on.
    pub db: i64,
    /// An ACL user; `None` is the default user.
    pub username: Option<String>,
    pub password: Option<String>,
    /// TLS, verifying the server's certificate against the system's roots.
    pub tls: bool,
}

impl Target {
    fn connection_info(&self) -> Result<ConnectionInfo, Error> {
        let addr = if self.tls {
            ConnectionAddr::TcpTls {
                host: self.host.clone(),
                port: self.port,
                insecure: false,
                tls_params: None,
            }
        } else {
            ConnectionAddr::Tcp(self.host.clone(), self.port)
        };
        let mut redis =
            RedisConnectionInfo::default().set_lib_name("kuery", env!("CARGO_PKG_VERSION"));
        if let Some(username) = self.username.as_deref().filter(|u| !u.is_empty()) {
            redis = redis.set_username(username);
        }
        if let Some(password) = self.password.as_deref().filter(|p| !p.is_empty()) {
            redis = redis.set_password(password);
        }
        Ok(addr.into_connection_info()?.set_redis_settings(redis))
    }
}

impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Target")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("db", &self.db)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "…"))
            .field("tls", &self.tls)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Redis(String),
    #[error("the connection runtime failed: {0}")]
    Runtime(String),
}

impl From<redis::RedisError> for Error {
    fn from(error: redis::RedisError) -> Self {
        Error::Redis(error.to_string())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerInfo {
    pub version: String,
    /// `standalone`, `cluster` or `sentinel`.
    pub mode: String,
    /// Databases the server has (`CONFIG GET databases`, else 16).
    pub databases: i64,
}

/// A database and how many keys it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbInfo {
    pub index: i64,
    pub keys: u64,
    pub expires: u64,
}

/// A page of key names from `SCAN`; `cursor` 0 means the scan is complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanPage {
    pub cursor: u64,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    String,
    Hash,
    List,
    Set,
    ZSet,
    Stream,
    /// The key does not exist (any more).
    Missing,
    /// A module's type, such as `ReJSON-RL`.
    Other,
}

impl KeyKind {
    fn parse(name: &str) -> Self {
        match name {
            "string" => KeyKind::String,
            "hash" => KeyKind::Hash,
            "list" => KeyKind::List,
            "set" => KeyKind::Set,
            "zset" => KeyKind::ZSet,
            "stream" => KeyKind::Stream,
            "none" => KeyKind::Missing,
            _ => KeyKind::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            KeyKind::String => "string",
            KeyKind::Hash => "hash",
            KeyKind::List => "list",
            KeyKind::Set => "set",
            KeyKind::ZSet => "sorted set",
            KeyKind::Stream => "stream",
            KeyKind::Missing => "missing",
            KeyKind::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    pub kind: KeyKind,
    /// The type's own name, as `TYPE` says it.
    pub type_name: String,
    /// Milliseconds to live; `None` when the key does not expire.
    pub ttl_ms: Option<i64>,
    /// Bytes for strings, entries for everything else.
    pub length: Option<u64>,
    /// `MEMORY USAGE`, when allowed.
    pub memory: Option<u64>,
    /// `OBJECT ENCODING`, when allowed.
    pub encoding: Option<String>,
}

/// A key as it is now, shown before a command changes or replaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPreview {
    pub key: String,
    pub kind: KeyKind,
    /// Milliseconds to live; `None` when the key does not expire.
    pub ttl_ms: Option<i64>,
    /// Bytes for strings, entries for everything else.
    pub length: Option<u64>,
    /// The start of the value, on one line (`…` when there is more).
    pub sample: String,
}

impl KeyPreview {
    pub fn exists(&self) -> bool {
        self.kind != KeyKind::Missing
    }
}

/// Characters of a value a preview keeps.
const SAMPLE: usize = 80;
/// Entries of a collection a preview keeps.
const SAMPLE_ENTRIES: usize = 3;

/// Where a page of a key's value starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    Start,
    /// `HSCAN`/`SSCAN` cursor.
    Cursor(u64),
    /// Position in a list or sorted set.
    Offset(u64),
    /// After this stream entry id.
    After(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    /// A string value, up to [`MAX_STRING`] bytes; `truncated` when longer.
    Text { bytes: Vec<u8>, truncated: bool },
    /// Fields, members, items or entries.
    Rows(Table),
    /// A type Kuery cannot show.
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValuePage {
    pub content: Content,
    /// Where the next page starts; `None` when this was the last.
    pub next: Option<Page>,
}

/// An open connection to a server.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

struct Inner {
    info: ConnectionInfo,
    default_db: i64,
    handle: tokio::runtime::Handle,
    connections: Mutex<HashMap<i64, ConnectionManager>>,
    server: ServerInfo,
}

impl Inner {
    /// The connection to database `db`, opened on first use.
    async fn connection(&self, db: i64) -> Result<ConnectionManager, Error> {
        let mut connections = self.connections.lock().await;
        if let Some(connection) = connections.get(&db) {
            return Ok(connection.clone());
        }
        let settings = self.info.redis_settings().clone().set_db(db);
        let client = redis::Client::open(self.info.clone().set_redis_settings(settings))?;
        let config = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(CONNECT_TIMEOUT))
            .set_response_timeout(Some(RESPONSE_TIMEOUT))
            .set_number_of_retries(2);
        let connection = client.get_connection_manager_with_config(config).await?;
        connections.insert(db, connection.clone());
        Ok(connection)
    }
}

impl Session {
    pub fn server(&self) -> &ServerInfo {
        &self.inner.server
    }

    /// The database the connection settings name.
    pub fn default_db(&self) -> i64 {
        self.inner.default_db
    }

    fn spawn<T, F, J>(&self, job: J) -> impl Future<Output = Result<T, Error>> + Send + use<T, F, J>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, Error>> + Send + 'static,
        J: FnOnce(Arc<Inner>) -> F,
    {
        let inner = self.inner.clone();
        run(&self.inner.handle, job(inner))
    }

    /// Every database with its key count (`INFO keyspace` leaves out the
    /// empty ones; they are listed with 0).
    pub fn keyspace(&self) -> impl Future<Output = Result<Vec<DbInfo>, Error>> + Send + use<> {
        self.spawn(|inner| async move {
            let mut connection = inner.connection(inner.default_db).await?;
            let text: String = redis::cmd("INFO")
                .arg("keyspace")
                .query_async(&mut connection)
                .await?;
            Ok(parse_keyspace(&text, inner.server.databases))
        })
    }

    /// Key names from `cursor` on, until about `wanted` are found or the
    /// scan ends. `pattern` is a `MATCH` glob.
    pub fn scan(
        &self,
        db: i64,
        cursor: u64,
        pattern: Option<String>,
        wanted: usize,
    ) -> impl Future<Output = Result<ScanPage, Error>> + Send + use<> {
        self.spawn(move |inner| async move {
            let mut connection = inner.connection(db).await?;
            let mut cursor = cursor;
            let mut keys = Vec::new();
            for _ in 0..SCAN_ROUNDS {
                let mut command = redis::cmd("SCAN");
                command.arg(cursor);
                if let Some(pattern) = pattern.as_deref().filter(|p| !p.is_empty()) {
                    command.arg("MATCH").arg(pattern);
                }
                command.arg("COUNT").arg(SCAN_COUNT);
                let (next, batch): (u64, Vec<Vec<u8>>) =
                    command.query_async(&mut connection).await?;
                keys.extend(
                    batch
                        .iter()
                        .map(|key| String::from_utf8_lossy(key).into_owned()),
                );
                cursor = next;
                if cursor == 0 || keys.len() >= wanted {
                    break;
                }
            }
            Ok(ScanPage { cursor, keys })
        })
    }

    /// The key's type, TTL, length, memory and encoding.
    pub fn key_info(
        &self,
        db: i64,
        key: String,
    ) -> impl Future<Output = Result<KeyInfo, Error>> + Send + use<> {
        self.spawn(move |inner| async move {
            let mut connection = inner.connection(db).await?;
            let type_name: String = redis::cmd("TYPE")
                .arg(&key)
                .query_async(&mut connection)
                .await?;
            let kind = KeyKind::parse(&type_name);
            let ttl: i64 = redis::cmd("PTTL")
                .arg(&key)
                .query_async(&mut connection)
                .await?;
            let length_command = match kind {
                KeyKind::String => Some("STRLEN"),
                KeyKind::Hash => Some("HLEN"),
                KeyKind::List => Some("LLEN"),
                KeyKind::Set => Some("SCARD"),
                KeyKind::ZSet => Some("ZCARD"),
                KeyKind::Stream => Some("XLEN"),
                KeyKind::Missing | KeyKind::Other => None,
            };
            let length = match length_command {
                Some(command) => Some(
                    redis::cmd(command)
                        .arg(&key)
                        .query_async::<u64>(&mut connection)
                        .await?,
                ),
                None => None,
            };
            let memory = redis::cmd("MEMORY")
                .arg("USAGE")
                .arg(&key)
                .query_async::<Option<u64>>(&mut connection)
                .await
                .ok()
                .flatten();
            let encoding = redis::cmd("OBJECT")
                .arg("ENCODING")
                .arg(&key)
                .query_async::<Option<String>>(&mut connection)
                .await
                .ok()
                .flatten();
            Ok(KeyInfo {
                kind,
                type_name,
                ttl_ms: (ttl >= 0).then_some(ttl),
                length,
                memory,
                encoding,
            })
        })
    }

    /// What `keys` hold now, in order, for the confirmation before a write.
    pub fn preview_keys(
        &self,
        db: i64,
        keys: Vec<String>,
    ) -> impl Future<Output = Result<Vec<KeyPreview>, Error>> + Send + use<> {
        self.spawn(move |inner| async move {
            let mut connection = inner.connection(db).await?;
            let mut previews = Vec::with_capacity(keys.len());
            for key in keys {
                previews.push(preview(&mut connection, key).await?);
            }
            Ok(previews)
        })
    }

    /// A page of the key's value, up to `limit` entries from `page`.
    pub fn read_value(
        &self,
        db: i64,
        key: String,
        kind: KeyKind,
        page: Page,
        limit: usize,
    ) -> impl Future<Output = Result<ValuePage, Error>> + Send + use<> {
        self.spawn(move |inner| async move {
            let mut connection = inner.connection(db).await?;
            let limit = limit.max(1);
            let page = match kind {
                KeyKind::String => {
                    let bytes: Vec<u8> = redis::cmd("GETRANGE")
                        .arg(&key)
                        .arg(0)
                        .arg(MAX_STRING)
                        .query_async(&mut connection)
                        .await?;
                    let truncated = bytes.len() > MAX_STRING;
                    let mut bytes = bytes;
                    bytes.truncate(MAX_STRING);
                    ValuePage {
                        content: Content::Text { bytes, truncated },
                        next: None,
                    }
                }
                KeyKind::Hash | KeyKind::Set => {
                    let cursor = match page {
                        Page::Cursor(cursor) => cursor,
                        _ => 0,
                    };
                    let command = if kind == KeyKind::Hash {
                        "HSCAN"
                    } else {
                        "SSCAN"
                    };
                    let (next, items): (u64, Vec<Vec<u8>>) = redis::cmd(command)
                        .arg(&key)
                        .arg(cursor)
                        .arg("COUNT")
                        .arg(limit)
                        .query_async(&mut connection)
                        .await?;
                    let table = if kind == KeyKind::Hash {
                        Table {
                            columns: vec!["field".into(), "value".into()],
                            rows: items
                                .chunks(2)
                                .map(|pair| pair.iter().map(|v| Some(display_bytes(v))).collect())
                                .collect(),
                        }
                    } else {
                        Table {
                            columns: vec!["member".into()],
                            rows: items.iter().map(|v| vec![Some(display_bytes(v))]).collect(),
                        }
                    };
                    ValuePage {
                        content: Content::Rows(table),
                        next: (next != 0).then_some(Page::Cursor(next)),
                    }
                }
                KeyKind::List | KeyKind::ZSet => {
                    let offset = match page {
                        Page::Offset(offset) => offset,
                        _ => 0,
                    };
                    let stop = offset + limit as u64 - 1;
                    let table = if kind == KeyKind::List {
                        let items: Vec<Vec<u8>> = redis::cmd("LRANGE")
                            .arg(&key)
                            .arg(offset)
                            .arg(stop)
                            .query_async(&mut connection)
                            .await?;
                        Table {
                            columns: vec!["index".into(), "value".into()],
                            rows: items
                                .iter()
                                .enumerate()
                                .map(|(i, v)| {
                                    vec![
                                        Some((offset + i as u64).to_string()),
                                        Some(display_bytes(v)),
                                    ]
                                })
                                .collect(),
                        }
                    } else {
                        let items: Vec<(Vec<u8>, String)> = redis::cmd("ZRANGE")
                            .arg(&key)
                            .arg(offset)
                            .arg(stop)
                            .arg("WITHSCORES")
                            .query_async(&mut connection)
                            .await?;
                        Table {
                            columns: vec!["rank".into(), "member".into(), "score".into()],
                            rows: items
                                .iter()
                                .enumerate()
                                .map(|(i, (member, score))| {
                                    vec![
                                        Some((offset + i as u64).to_string()),
                                        Some(display_bytes(member)),
                                        Some(score.clone()),
                                    ]
                                })
                                .collect(),
                        }
                    };
                    let full = table.rows.len() == limit;
                    ValuePage {
                        content: Content::Rows(table),
                        next: full.then_some(Page::Offset(offset + limit as u64)),
                    }
                }
                KeyKind::Stream => {
                    let start = match page {
                        Page::After(id) => format!("({id}"),
                        _ => "-".to_string(),
                    };
                    let entries: Value = redis::cmd("XRANGE")
                        .arg(&key)
                        .arg(start)
                        .arg("+")
                        .arg("COUNT")
                        .arg(limit)
                        .query_async(&mut connection)
                        .await?;
                    let (table, last) = reply::stream_table(&entries);
                    let full = table.rows.len() == limit;
                    ValuePage {
                        content: Content::Rows(table),
                        next: last.filter(|_| full).map(Page::After),
                    }
                }
                KeyKind::Missing => ValuePage {
                    content: Content::Unsupported("The key does not exist any more.".into()),
                    next: None,
                },
                KeyKind::Other => ValuePage {
                    content: Content::Unsupported(
                        "Kuery cannot show values of this type yet.".into(),
                    ),
                    next: None,
                },
            };
            Ok(page)
        })
    }

    /// Runs a command as typed (the first argument is its name).
    pub fn command(
        &self,
        db: i64,
        args: Vec<String>,
    ) -> impl Future<Output = Result<Reply, Error>> + Send + use<> {
        self.spawn(move |inner| async move {
            let mut connection = inner.connection(db).await?;
            let Some((name, rest)) = args.split_first() else {
                return Err(Error::Redis("empty command".into()));
            };
            let mut command = redis::cmd(name);
            for arg in rest {
                command.arg(arg);
            }
            match command.query_async::<Value>(&mut connection).await {
                Ok(value) => Ok(Reply::from(value)),
                // An error reply is the command's result, not a broken connection.
                Err(error) if error.code().is_some() => Ok(Reply::Error(error.to_string())),
                Err(error) => Err(error.into()),
            }
        })
    }
}

async fn preview(connection: &mut ConnectionManager, key: String) -> Result<KeyPreview, Error> {
    let type_name: String = redis::cmd("TYPE").arg(&key).query_async(connection).await?;
    let kind = KeyKind::parse(&type_name);
    let ttl: i64 = redis::cmd("PTTL").arg(&key).query_async(connection).await?;
    let last = SAMPLE_ENTRIES as i64 - 1;
    let (length, items): (Option<u64>, Vec<String>) = match kind {
        KeyKind::String => {
            let length: u64 = redis::cmd("STRLEN")
                .arg(&key)
                .query_async(connection)
                .await?;
            let start: Vec<u8> = redis::cmd("GETRANGE")
                .arg(&key)
                .arg(0)
                .arg(SAMPLE * 4)
                .query_async(connection)
                .await?;
            (Some(length), vec![display_bytes(&start)])
        }
        KeyKind::Hash => {
            let length: u64 = redis::cmd("HLEN").arg(&key).query_async(connection).await?;
            let (_, items): (u64, Vec<Vec<u8>>) = redis::cmd("HSCAN")
                .arg(&key)
                .arg(0)
                .arg("COUNT")
                .arg(SAMPLE_ENTRIES)
                .query_async(connection)
                .await?;
            let pairs = items
                .chunks(2)
                .take(SAMPLE_ENTRIES)
                .map(|pair| {
                    format!(
                        "{}={}",
                        display_bytes(&pair[0]),
                        pair.get(1).map(|v| display_bytes(v)).unwrap_or_default()
                    )
                })
                .collect();
            (Some(length), pairs)
        }
        KeyKind::List => {
            let length: u64 = redis::cmd("LLEN").arg(&key).query_async(connection).await?;
            let items: Vec<Vec<u8>> = redis::cmd("LRANGE")
                .arg(&key)
                .arg(0)
                .arg(last)
                .query_async(connection)
                .await?;
            (
                Some(length),
                items.iter().map(|v| display_bytes(v)).collect(),
            )
        }
        KeyKind::Set => {
            let length: u64 = redis::cmd("SCARD")
                .arg(&key)
                .query_async(connection)
                .await?;
            let (_, items): (u64, Vec<Vec<u8>>) = redis::cmd("SSCAN")
                .arg(&key)
                .arg(0)
                .arg("COUNT")
                .arg(SAMPLE_ENTRIES)
                .query_async(connection)
                .await?;
            let members = items
                .iter()
                .take(SAMPLE_ENTRIES)
                .map(|v| display_bytes(v))
                .collect();
            (Some(length), members)
        }
        KeyKind::ZSet => {
            let length: u64 = redis::cmd("ZCARD")
                .arg(&key)
                .query_async(connection)
                .await?;
            let items: Vec<(Vec<u8>, String)> = redis::cmd("ZRANGE")
                .arg(&key)
                .arg(0)
                .arg(last)
                .arg("WITHSCORES")
                .query_async(connection)
                .await?;
            let members = items
                .iter()
                .map(|(member, score)| format!("{} ({score})", display_bytes(member)))
                .collect();
            (Some(length), members)
        }
        KeyKind::Stream => {
            let length: u64 = redis::cmd("XLEN").arg(&key).query_async(connection).await?;
            (Some(length), Vec::new())
        }
        KeyKind::Missing | KeyKind::Other => (None, Vec::new()),
    };
    let more = length.is_some_and(|length| match kind {
        KeyKind::String => length as usize > SAMPLE,
        _ => length as usize > items.len(),
    });
    let mut sample: String = items
        .join(", ")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if sample.chars().count() > SAMPLE {
        sample = sample.chars().take(SAMPLE).collect();
        sample.push('…');
    } else if more && !sample.is_empty() {
        sample.push_str(", …");
    }
    Ok(KeyPreview {
        key,
        kind,
        ttl_ms: (ttl >= 0).then_some(ttl),
        length,
        sample,
    })
}

/// `INFO keyspace` → one entry per database, empty ones included.
fn parse_keyspace(text: &str, databases: i64) -> Vec<DbInfo> {
    let mut found: HashMap<i64, (u64, u64)> = HashMap::new();
    for line in text.lines() {
        let Some((name, fields)) = line.trim().split_once(':') else {
            continue;
        };
        let Some(index) = name.strip_prefix("db").and_then(|i| i.parse::<i64>().ok()) else {
            continue;
        };
        let field = |key: &str| {
            fields
                .split(',')
                .find_map(|pair| pair.strip_prefix(key)?.strip_prefix('='))
                .and_then(|value| value.parse().ok())
                .unwrap_or(0)
        };
        found.insert(index, (field("keys"), field("expires")));
    }
    let last = found.keys().copied().max().unwrap_or(0).max(databases - 1);
    (0..=last)
        .map(|index| {
            let (keys, expires) = found.get(&index).copied().unwrap_or((0, 0));
            DbInfo {
                index,
                keys,
                expires,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_keyspace() {
        let text =
            "# Keyspace\r\ndb0:keys=1770,expires=61,avg_ttl=2184\r\ndb3:keys=2,expires=0\r\n";
        let dbs = parse_keyspace(text, 4);
        assert_eq!(dbs.len(), 4);
        assert_eq!((dbs[0].keys, dbs[0].expires), (1770, 61));
        assert_eq!((dbs[1].keys, dbs[3].keys), (0, 2));
        assert_eq!(parse_keyspace("", 2).len(), 2);
    }

    #[test]
    fn hides_the_password_when_printed() {
        let target = Target {
            host: "localhost".into(),
            port: 6379,
            db: 0,
            username: None,
            password: Some("secret".into()),
            tls: false,
        };
        assert!(!format!("{target:?}").contains("secret"));
    }

    // Against the dev Redis (`docker compose -p kuery-dev -f dev/compose.yaml up -d redis`).

    fn dev() -> (Engine, Target) {
        let target = Target {
            host: "127.0.0.1".into(),
            port: 63799,
            db: 0,
            username: None,
            password: None,
            tls: false,
        };
        (Engine::new().unwrap(), target)
    }

    fn block<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    #[ignore = "needs the dev Redis"]
    fn connects_and_lists_databases() {
        let (engine, target) = dev();
        let session = block(engine.connect(target)).expect("connects to the dev Redis");
        assert!(session.server().version.starts_with('7'));
        let dbs = block(session.keyspace()).unwrap();
        assert!(dbs.len() >= 16);
        // Sessions and cached pages expire, the rest stays.
        assert!((1709..=1770).contains(&dbs[0].keys));
        assert!((1..=41).contains(&dbs[1].keys));
    }

    #[test]
    #[ignore = "needs the dev Redis"]
    fn scans_keys_page_by_page() {
        let (engine, target) = dev();
        let session = block(engine.connect(target)).unwrap();
        let mut cursor = 0;
        let mut all = Vec::new();
        loop {
            let page = block(session.scan(0, cursor, Some("product:*".into()), 400)).unwrap();
            all.extend(page.keys);
            cursor = page.cursor;
            if cursor == 0 {
                break;
            }
        }
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 1500);
        let cache = block(session.scan(1, 0, None, 1000)).unwrap();
        assert_eq!(cache.cursor, 0);
        assert!(cache.keys.iter().any(|key| key == "cache:stats"));
    }

    #[test]
    #[ignore = "needs the dev Redis"]
    fn reads_every_kind_of_value() {
        let (engine, target) = dev();
        let session = block(engine.connect(target)).unwrap();
        let info = |key: &str| block(session.key_info(0, key.into())).unwrap();
        let read = |key: &str, kind, page, limit| {
            block(session.read_value(0, key.into(), kind, page, limit)).unwrap()
        };

        let config = info("app:config");
        assert_eq!((config.kind, config.ttl_ms), (KeyKind::String, None));
        let Content::Text { bytes, truncated } =
            read("app:config", KeyKind::String, Page::Start, 1).content
        else {
            panic!("text");
        };
        assert!(!truncated && bytes.starts_with(b"{\"theme\""));
        assert!(info("maintenance:banner").ttl_ms.is_some_and(|ttl| ttl > 0));

        let user = info("user:1");
        assert_eq!((user.kind, user.length), (KeyKind::Hash, Some(4)));
        let Content::Rows(fields) = read("user:1", KeyKind::Hash, Page::Start, 100).content else {
            panic!("rows");
        };
        assert_eq!(fields.columns, ["field", "value"]);
        assert_eq!(fields.rows.len(), 4);

        let first = read("queue:emails", KeyKind::List, Page::Start, 100);
        assert_eq!(first.next, Some(Page::Offset(100)));
        let last = read("queue:emails", KeyKind::List, Page::Offset(200), 100);
        let Content::Rows(items) = &last.content else {
            panic!("rows");
        };
        assert_eq!(items.rows[0][0].as_deref(), Some("200"));

        let Content::Rows(top) = read("leaderboard", KeyKind::ZSet, Page::Start, 5).content else {
            panic!("rows");
        };
        assert_eq!(top.columns, ["rank", "member", "score"]);

        let orders = read("events:orders", KeyKind::Stream, Page::Start, 200);
        let Content::Rows(entries) = &orders.content else {
            panic!("rows");
        };
        assert_eq!(
            entries.columns,
            ["id", "order_id", "user", "amount", "status"]
        );
        assert_eq!(entries.rows.len(), 200);
        let Some(Page::After(after)) = orders.next else {
            panic!("more entries");
        };
        let rest = read("events:orders", KeyKind::Stream, Page::After(after), 1000);
        let Content::Rows(rest_entries) = rest.content else {
            panic!("rows");
        };
        assert_eq!((rest_entries.rows.len(), rest.next), (300, None));

        let mut members = 0;
        let mut page = Page::Start;
        loop {
            let read = read("online:users", KeyKind::Set, page, 20);
            let Content::Rows(table) = read.content else {
                panic!("rows");
            };
            members += table.rows.len();
            match read.next {
                Some(next) => page = next,
                None => break,
            }
        }
        assert!(members >= 50);
    }

    #[test]
    #[ignore = "needs the dev Redis"]
    fn writes_and_previews_keys() {
        let (engine, target) = dev();
        let session = block(engine.connect(target)).unwrap();
        // Keys of its own, removed at the end, in database 2 (empty in the seed).
        let run = |line: &[&str]| {
            block(session.command(2, line.iter().map(|a| a.to_string()).collect())).unwrap()
        };
        let preview = |keys: &[&str]| {
            block(session.preview_keys(2, keys.iter().map(|k| k.to_string()).collect())).unwrap()
        };
        run(&["DEL", "kuery:test:s", "kuery:test:h", "kuery:test:renamed"]);
        assert_eq!(
            run(&["SET", "kuery:test:s", "hello", "EX", "100"]),
            Reply::Status("OK".into())
        );
        run(&[
            "HSET",
            "kuery:test:h",
            "a",
            "1",
            "b",
            "2",
            "c",
            "3",
            "d",
            "4",
        ]);
        let previews = preview(&["kuery:test:s", "kuery:test:h", "kuery:test:none"]);
        assert_eq!(previews[0].kind, KeyKind::String);
        assert_eq!(previews[0].sample, "hello");
        assert!(previews[0].ttl_ms.is_some_and(|ttl| ttl > 0));
        assert_eq!(
            (previews[1].kind, previews[1].length),
            (KeyKind::Hash, Some(4))
        );
        assert!(previews[1].sample.ends_with('…'), "{}", previews[1].sample);
        assert!(!previews[2].exists());

        run(&["PERSIST", "kuery:test:s"]);
        run(&["RENAME", "kuery:test:s", "kuery:test:renamed"]);
        run(&["HDEL", "kuery:test:h", "a"]);
        let previews = preview(&["kuery:test:renamed", "kuery:test:h"]);
        assert_eq!(previews[0].ttl_ms, None);
        assert_eq!(previews[1].length, Some(3));
        assert_eq!(
            run(&["DEL", "kuery:test:renamed", "kuery:test:h"]),
            Reply::Int(2)
        );
    }

    #[test]
    #[ignore = "needs the dev Redis"]
    fn runs_commands_and_reports_error_replies() {
        let (engine, target) = dev();
        let session = block(engine.connect(target)).unwrap();
        let reply =
            block(session.command(0, vec!["HGET".into(), "user:1".into(), "plan".into()])).unwrap();
        assert_eq!(reply, Reply::Text("pro".into()));
        let reply =
            block(session.command(1, vec!["HGET".into(), "cache:stats".into(), "hits".into()]))
                .unwrap();
        assert_eq!(reply, Reply::Text("9812".into()));
        let error =
            block(session.command(0, vec!["HGET".into(), "app:name".into(), "x".into()])).unwrap();
        assert!(matches!(error, Reply::Error(text) if text.contains("WRONGTYPE")));
    }
}
