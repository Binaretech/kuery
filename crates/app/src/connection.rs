//! Active connection and its environment.
//!
//! The environment decides whether writes are allowed: in production the session
//! opens read-only and the explorer does not offer row editing. Write mode in
//! a query tab follows [`WritePolicy`]; in production it needs writes allowed
//! for the connection session first, which is never saved anywhere.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui_kit::SharedString;
use kuery_pg::{Access, ConnectError, Engine, Session};

use crate::theme;

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
    /// and then every script that writes is confirmed too. Scripts that
    /// cannot be analysed are refused.
    SessionUnlock,
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

/// Opens more connections like the one in [`Db`], for write mode. It holds
/// the connection string, password included, only in memory.
#[derive(Clone)]
pub struct Reconnect {
    conninfo: Arc<str>,
}

impl Reconnect {
    pub fn new(conninfo: String) -> Self {
        Self {
            conninfo: conninfo.into(),
        }
    }

    pub fn open(
        &self,
        engine: &Engine,
        access: Access,
    ) -> impl Future<Output = Result<Session, ConnectError>> + Send + use<> {
        engine.connect_with(self.conninfo.to_string(), access)
    }
}

impl fmt::Debug for Reconnect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Reconnect(…)")
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
    pub reconnect: Reconnect,
    /// Production only: writes allowed for this connection session. Shared by
    /// every tab on it, gone when it disconnects or the app quits.
    writes_allowed: Arc<AtomicBool>,
}

impl Db {
    pub fn new(
        name: SharedString,
        environment: Environment,
        session: Session,
        target: SharedString,
        reconnect: Reconnect,
    ) -> Self {
        Self {
            name,
            environment,
            session,
            target,
            reconnect,
            writes_allowed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn can_write(&self) -> bool {
        self.session.access() == Access::ReadWrite
    }

    /// Whether write mode can be turned on without allowing it first.
    pub fn writes_allowed(&self) -> bool {
        self.environment.write_policy() != WritePolicy::SessionUnlock
            || self.writes_allowed.load(Ordering::SeqCst)
    }

    /// Allows (or stops allowing) writes for this connection session.
    pub fn set_writes_allowed(&self, allowed: bool) {
        self.writes_allowed.store(allowed, Ordering::SeqCst);
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
