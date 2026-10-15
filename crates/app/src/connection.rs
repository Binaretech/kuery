//! Active connection and its environment.
//!
//! The environment decides whether writes are allowed: in production the session
//! opens read-only and the explorer does not offer row editing.

use dbadmin_pg::{Access, Session};
use gpui_kit::SharedString;

use crate::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Development,
    Staging,
    Production,
}

impl Environment {
    /// `DBADMIN_ENVIRONMENT` wins if set; otherwise local databases are
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
}

impl Db {
    pub fn can_write(&self) -> bool {
        self.session.access() == Access::ReadWrite
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
            describe_target("postgres://dbadmin:secret@localhost:54329/dbadmin?sslmode=disable"),
            "localhost:54329/dbadmin"
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
}
