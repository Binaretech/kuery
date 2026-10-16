//! Active connection and its environment.
//!
//! The environment decides whether writes are allowed: in production the session
//! opens read-only and the explorer does not offer row editing. Write mode in
//! a query tab follows [`WritePolicy`]; in production it needs writes allowed
//! for the connection session first, which is never saved anywhere.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui_kit::SharedString;
use kuery_core::session::{Access, Session};

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
            return match name.trim().to_lowercase().as_str() {
                "development" | "dev" | "local" => Environment::Development,
                "staging" | "stage" => Environment::Staging,
                _ => Environment::Production,
            };
        }
        match host(url).as_str() {
            "localhost" | "127.0.0.1" | "::1" | "[::1]" | "host.docker.internal" => {
                Environment::Development
            }
            _ => Environment::Production,
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
}

/// An open Redis connection, shared by the sidebar and the tabs opened from
/// it. Redis has no transactions, so writes are allowed or not: on
/// production only once allowed for the connection session.
#[derive(Clone)]
pub struct RedisDb {
    pub name: SharedString,
    pub environment: Environment,
    pub session: kuery_redis::Session,
    /// `host:port/database`, without credentials.
    pub target: SharedString,
    /// Production only: whether writes were allowed for this connection
    /// session. Shared by every tab on it, gone when it disconnects.
    writes_allowed: Arc<AtomicBool>,
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
            writes_allowed: Arc::new(AtomicBool::new(false)),
            _tunnel: tunnel,
        }
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
