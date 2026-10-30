//! Active connection and its environment.
//!
//! The environment decides whether writes are allowed: in production the session
//! opens read-only and the explorer does not offer row editing. Write mode in
//! a query tab follows [`WritePolicy`]; in production it needs writes allowed
//! for the connection session first, which is never saved anywhere.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use gpui_kit::SharedString;
use kuery_core::document::schema::Sampled;
use kuery_core::document::session::DocumentSession;
use kuery_core::document::value::CollectionRef;
use kuery_core::session::{Access, QueryError, Session};

use crate::theme;
use crate::tunnel::Tunnel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Development,
    Staging,
    Production,
}

impl Environment {
    /// `KUERY_ENVIRONMENT` wins if set; otherwise local databases are
    /// development and any other is treated as production. An unknown value
    /// also counts as production.
    pub fn detect(url: &str, explicit: Option<&str>) -> Self {
        if let Some(name) = explicit {
            return Self::parse(name);
        }
        match host(url).as_str() {
            "localhost" | "127.0.0.1" | "::1" | "[::1]" | "host.docker.internal" => {
                Environment::Development
            }
            _ => Environment::Production,
        }
    }

    /// The environment a project file names, with its aliases (`dev`,
    /// `stage`) in any case; an unknown name is production.
    pub fn parse(name: &str) -> Self {
        match name.trim().to_lowercase().as_str() {
            "development" | "dev" | "local" => Environment::Development,
            "staging" | "stage" => Environment::Staging,
            _ => Environment::Production,
        }
    }

    /// The name written to the project file.
    pub const fn name(self) -> &'static str {
        match self {
            Environment::Development => "development",
            Environment::Staging => "staging",
            Environment::Production => "production",
        }
    }

    /// Short form for the sidebar.
    pub fn short_label(self) -> &'static str {
        match self {
            Environment::Development => "DEV",
            Environment::Staging => "STAGING",
            Environment::Production => "PROD",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Environment::Development => "DEVELOPMENT",
            Environment::Staging => "STAGING",
            Environment::Production => "PRODUCTION",
        }
    }

    pub fn color(self) -> u32 {
        match self {
            Environment::Development => theme::GREEN,
            Environment::Staging => 0x4da3ff,
            Environment::Production => theme::AMBER,
        }
    }

    pub fn access(self) -> Access {
        match self {
            Environment::Production => Access::ReadOnly,
            Environment::Development | Environment::Staging => Access::ReadWrite,
        }
    }

    pub fn write_policy(self) -> WritePolicy {
        match self {
            Environment::Development => WritePolicy::ConfirmDangerous,
            Environment::Staging => WritePolicy::ConfirmWrites,
            Environment::Production => WritePolicy::SessionUnlock,
        }
    }
}

/// The tone of the connection's name in the status bar: amber on
/// production, so it is never mistaken for another.
pub fn status_tone(environment: Environment) -> u32 {
    match environment {
        Environment::Production => 0xffcf7f,
        Environment::Development | Environment::Staging => 0xc7c7cc,
    }
}

/// What the chrome shows about a tab's connection, whichever its family.
#[derive(Clone)]
pub struct ConnMeta {
    pub name: SharedString,
    pub environment: Environment,
    /// `host:port/database`, without credentials.
    pub target: SharedString,
    /// `PostgreSQL 16.4`, `Redis 7.4.1`.
    pub server: SharedString,
    /// What the driver said the user should know (see `Adapter::notice`).
    pub notice: Option<SharedString>,
}

/// How much write mode asks before running a script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritePolicy {
    /// Only statements that are easy to regret (see `kuery_core::analysis`).
    ConfirmDangerous,
    /// Every script that writes.
    ConfirmWrites,
    /// Writes must be allowed for the connection session (typing its name),
    /// full writes a second time, and then every script that writes is
    /// confirmed too. Scripts that cannot be analysed are refused.
    SessionUnlock,
}

/// How a query tab in write mode keeps its changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    /// In a transaction that stays open until Commit or Roll Back.
    Transactional,
    /// Each script is kept as soon as it finishes, all of it or nothing.
    Full,
}

impl WriteKind {
    pub fn label(self) -> &'static str {
        match self {
            WriteKind::Transactional => "Write",
            WriteKind::Full => "Full write",
        }
    }
}

impl WritePolicy {
    /// Whether a script that writes needs confirmation.
    pub fn confirms(self, dangerous: bool) -> bool {
        match self {
            WritePolicy::ConfirmDangerous => dangerous,
            WritePolicy::ConfirmWrites | WritePolicy::SessionUnlock => true,
        }
    }

    /// Whether a script Kuery cannot analyse may run after a warning.
    pub fn allows_unanalysed(self) -> bool {
        self != WritePolicy::SessionUnlock
    }
}

/// An open connection, shared by the sidebar and the tabs opened from it.
#[derive(Clone)]
pub struct Db {
    /// Name from the project configuration, e.g. `local-dev`.
    pub name: SharedString,
    pub environment: Environment,
    pub session: Session,
    /// `host:port/database`, without credentials.
    pub target: SharedString,
    /// Production only: what is allowed for this connection session. Shared
    /// by every tab on it, gone when it disconnects or the app quits.
    writes_allowed: Arc<AtomicBool>,
    full_writes_allowed: Arc<AtomicBool>,
    /// The SSH tunnel the connection goes through, open while any clone is.
    _tunnel: Option<Arc<Tunnel>>,
}

impl Db {
    pub fn new(
        name: SharedString,
        environment: Environment,
        session: Session,
        target: SharedString,
        tunnel: Option<Arc<Tunnel>>,
    ) -> Self {
        Self {
            name,
            environment,
            session,
            target,
            writes_allowed: Arc::new(AtomicBool::new(false)),
            full_writes_allowed: Arc::new(AtomicBool::new(false)),
            _tunnel: tunnel,
        }
    }

    pub fn can_write(&self) -> bool {
        self.session.access() == Access::ReadWrite
    }

    /// Whether writes of this kind can run without allowing them first. Full
    /// writes need writes allowed too.
    pub fn allows(&self, kind: WriteKind) -> bool {
        allows(
            self.environment.write_policy(),
            self.writes_allowed.load(Ordering::SeqCst),
            self.full_writes_allowed.load(Ordering::SeqCst),
            kind,
        )
    }

    /// Allows writes of this kind for the connection session.
    pub fn allow(&self, kind: WriteKind) {
        match kind {
            WriteKind::Transactional => &self.writes_allowed,
            WriteKind::Full => &self.full_writes_allowed,
        }
        .store(true, Ordering::SeqCst);
    }

    /// Stops allowing any writes for the connection session.
    pub fn revoke_writes(&self) {
        self.writes_allowed.store(false, Ordering::SeqCst);
        self.full_writes_allowed.store(false, Ordering::SeqCst);
    }

    /// What the status bar, tab tooltips and the production stripe show.
    pub fn meta(&self) -> ConnMeta {
        ConnMeta {
            name: self.name.clone(),
            environment: self.environment,
            target: self.target.clone(),
            server: self.session.server_label().to_string().into(),
            notice: self
                .session
                .notice()
                .map(|notice| notice.to_string().into()),
        }
    }
}

/// An open Redis connection, shared by the sidebar and the tabs opened from
/// it. Redis has no transactions, so writes are allowed or not: on
/// production only once allowed for the connection session. The session
/// keeps that, so it refuses writes too, whatever the tab checked.
#[derive(Clone)]
pub struct RedisDb {
    pub name: SharedString,
    pub environment: Environment,
    /// Opened with the environment's access. Production only: whether writes
    /// were allowed for it is shared by every tab, gone when it disconnects.
    pub session: kuery_redis::Session,
    /// `host:port/database`, without credentials.
    pub target: SharedString,
    /// The SSH tunnel the connection goes through, open while any clone is.
    _tunnel: Option<Arc<Tunnel>>,
}

impl RedisDb {
    pub fn new(
        name: SharedString,
        environment: Environment,
        session: kuery_redis::Session,
        target: SharedString,
        tunnel: Option<Arc<Tunnel>>,
    ) -> Self {
        Self {
            name,
            environment,
            session,
            target,
            _tunnel: tunnel,
        }
    }

    /// Whether writes can run without allowing them first.
    pub fn allows_writes(&self) -> bool {
        self.session.allows_writes()
    }

    pub fn allow_writes(&self) {
        self.session.allow_writes();
    }

    pub fn revoke_writes(&self) {
        self.session.revoke_writes();
    }

    /// What the status bar, tab tooltips and the production stripe show.
    pub fn meta(&self) -> ConnMeta {
        ConnMeta {
            name: self.name.clone(),
            environment: self.environment,
            target: self.target.clone(),
            server: format!("Redis {}", self.session.server().version).into(),
            notice: None,
        }
    }
}

/// Documents sampled for a collection's schema.
pub const SCHEMA_SAMPLE: usize = 1_000;

/// A schema sample on its way, which every tab that asks for it waits for.
type Sampling = Shared<BoxFuture<'static, Result<Arc<Sampled>, QueryError>>>;

/// An open document connection. The sidebar uses `catalog`; each tab
/// opened on it opens a session of its own from it
/// ([`DocumentSession::reopen`]), sharing the driver's client, so the
/// sidebar never waits behind a long query.
#[derive(Clone)]
pub struct DocDb {
    pub name: SharedString,
    pub environment: Environment,
    pub catalog: DocumentSession,
    /// `host:port/database`, without credentials.
    pub target: SharedString,
    /// The container the form names (`["shop"]`), where a console starts;
    /// empty for the driver's own default.
    pub home: Vec<String>,
    /// Production only: whether writes were allowed for this connection
    /// session. Shared by every tab on it, gone when it disconnects.
    writes_allowed: Arc<AtomicBool>,
    /// The schemas sampled on this connection, by collection: one for every
    /// tab on it (the collection tab's Schema, completion, the insert
    /// template), gone when it disconnects.
    schemas: Arc<Mutex<HashMap<CollectionRef, Arc<Sampled>>>>,
    /// The samples on their way, by collection: whoever asks for one
    /// meanwhile waits for it instead of sampling again.
    sampling: Arc<Mutex<HashMap<CollectionRef, Sampling>>>,
    /// The SSH tunnel the connection goes through, open while any clone is.
    _tunnel: Option<Arc<Tunnel>>,
}

impl DocDb {
    pub fn new(
        name: SharedString,
        environment: Environment,
        catalog: DocumentSession,
        target: SharedString,
        home: Vec<String>,
        tunnel: Option<Arc<Tunnel>>,
    ) -> Self {
        Self {
            name,
            environment,
            catalog,
            target,
            home,
            writes_allowed: Arc::new(AtomicBool::new(false)),
            schemas: Arc::default(),
            sampling: Arc::default(),
            _tunnel: tunnel,
        }
    }

    /// The collection's schema, if it was sampled on this connection.
    pub fn cached_schema(&self, collection: &CollectionRef) -> Option<Arc<Sampled>> {
        self.schemas.lock().ok()?.get(collection).cloned()
    }

    /// The collection's schema: the one sampled on this connection, or
    /// (when there is none, or `again`) [`SCHEMA_SAMPLE`] documents sampled
    /// and inferred on the catalog session, off the UI thread, and kept.
    /// A sample already on its way is waited for, not run again. One that
    /// found nothing (the collection is empty, or does not exist yet) is
    /// not kept.
    pub fn schema(
        &self,
        collection: CollectionRef,
        again: bool,
    ) -> impl Future<Output = Result<Arc<Sampled>, QueryError>> + 'static {
        let cached = match again {
            true => None,
            false => self.cached_schema(&collection),
        };
        let sampling = cached.is_none().then(|| self.sampling(collection));
        async move {
            match (cached, sampling) {
                (Some(cached), _) => Ok(cached),
                (None, Some(sampling)) => sampling.await,
                (None, None) => unreachable!("sampled when not cached"),
            }
        }
    }

    /// The sample of `collection` on its way, started if there is none.
    fn sampling(&self, collection: CollectionRef) -> Sampling {
        let Ok(mut on_way) = self.sampling.lock() else {
            return self.sample(collection);
        };
        if let Some(sampling) = on_way.get(&collection) {
            return sampling.clone();
        }
        let sampling = self.sample(collection.clone());
        on_way.insert(collection, sampling.clone());
        sampling
    }

    /// Samples `collection` and keeps what it found, once it is done.
    fn sample(&self, collection: CollectionRef) -> Sampling {
        let reading = self.catalog.schema(collection.clone(), SCHEMA_SAMPLE);
        let (schemas, on_way) = (self.schemas.clone(), self.sampling.clone());
        async move {
            let sampled = reading.await.map(Arc::new);
            if let Ok(mut on_way) = on_way.lock() {
                on_way.remove(&collection);
            }
            if let Ok(sampled) = &sampled
                && sampled.schema.documents > 0
                && let Ok(mut schemas) = schemas.lock()
            {
                schemas.insert(collection, sampled.clone());
            }
            sampled
        }
        .boxed()
        .shared()
    }

    /// Whether writes can run without allowing them first.
    pub fn allows_writes(&self) -> bool {
        self.environment.write_policy() != WritePolicy::SessionUnlock
            || self.writes_allowed.load(Ordering::SeqCst)
    }

    pub fn allow_writes(&self) {
        self.writes_allowed.store(true, Ordering::SeqCst);
    }

    pub fn revoke_writes(&self) {
        self.writes_allowed.store(false, Ordering::SeqCst);
    }

    /// What the status bar, tab tooltips and the production stripe show.
    pub fn meta(&self) -> ConnMeta {
        ConnMeta {
            name: self.name.clone(),
            environment: self.environment,
            target: self.target.clone(),
            server: self.catalog.server_label().to_string().into(),
            notice: self
                .catalog
                .notice()
                .map(|notice| notice.to_string().into()),
        }
    }
}

/// Whether `kind` may run, given what the connection session allows.
fn allows(policy: WritePolicy, writes: bool, full_writes: bool, kind: WriteKind) -> bool {
    if policy != WritePolicy::SessionUnlock {
        return true;
    }
    match kind {
        WriteKind::Transactional => writes,
        WriteKind::Full => writes && full_writes,
    }
}

/// `postgres://user:password@host:port/database?…` → `host:port/database`.
pub fn describe_target(url: &str) -> String {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let without_user = without_scheme
        .rsplit_once('@')
        .map(|(_, rest)| rest)
        .unwrap_or(without_scheme);
    without_user
        .split('?')
        .next()
        .unwrap_or(without_user)
        .to_string()
}

fn host(url: &str) -> String {
    let target = describe_target(url);
    let authority = target.split('/').next().unwrap_or_default();
    let host = if authority.starts_with('[') {
        authority
            .split(']')
            .next()
            .map(|h| format!("{h}]"))
            .unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default().to_string()
    };
    host.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_credentials_in_the_target() {
        assert_eq!(
            describe_target("postgres://kuery:secret@localhost:54329/kuery?sslmode=disable"),
            "localhost:54329/kuery"
        );
    }

    #[test]
    fn local_databases_are_development_and_the_rest_production() {
        let detect = |url| Environment::detect(url, None);
        assert_eq!(
            detect("postgres://u:p@localhost:5432/db"),
            Environment::Development
        );
        assert_eq!(
            detect("postgres://u:p@127.0.0.1/db"),
            Environment::Development
        );
        assert_eq!(
            detect("postgres://u:p@[::1]:5432/db"),
            Environment::Development
        );
        assert_eq!(
            detect("postgres://u:p@db.example.com/db"),
            Environment::Production
        );
        assert_eq!(
            detect("postgres://u:p@localhost.example.com/db"),
            Environment::Production
        );
        assert_eq!(
            detect("postgres://cluster0.x.mongodb.net/"),
            Environment::Production
        );
    }

    #[test]
    fn the_explicit_environment_wins_and_unknown_means_production() {
        let url = "postgres://u:p@localhost/db";
        assert_eq!(
            Environment::detect(url, Some("staging")),
            Environment::Staging
        );
        assert_eq!(
            Environment::detect(url, Some("PRODUCTION")),
            Environment::Production
        );
        assert_eq!(
            Environment::detect(url, Some("qa?")),
            Environment::Production
        );
        assert_eq!(Environment::Production.access(), Access::ReadOnly);
    }

    #[test]
    fn production_full_writes_need_both_permissions() {
        use WriteKind::*;
        let dev = WritePolicy::ConfirmDangerous;
        let production = WritePolicy::SessionUnlock;
        assert!(allows(dev, false, false, Full));
        assert!(!allows(production, false, false, Transactional));
        assert!(allows(production, true, false, Transactional));
        assert!(!allows(production, true, false, Full));
        assert!(!allows(production, false, true, Full));
        assert!(allows(production, true, true, Full));
    }

    #[test]
    fn only_production_names_stand_out_in_the_status_bar() {
        assert_eq!(status_tone(Environment::Production), 0xffcf7f);
        assert_eq!(status_tone(Environment::Staging), 0xc7c7cc);
        assert_eq!(status_tone(Environment::Development), 0xc7c7cc);
    }

    #[test]
    fn write_policies_ask_more_the_closer_to_production() {
        let dev = Environment::Development.write_policy();
        assert!(!dev.confirms(false) && dev.confirms(true) && dev.allows_unanalysed());
        let staging = Environment::Staging.write_policy();
        assert!(staging.confirms(false) && staging.allows_unanalysed());
        let production = Environment::Production.write_policy();
        assert_eq!(production, WritePolicy::SessionUnlock);
        assert!(production.confirms(false) && !production.allows_unanalysed());
    }
}
