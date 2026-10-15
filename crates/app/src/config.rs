//! Project configuration: the project name and its connections.
//!
//! Read from `DBADMIN_CONFIG`, or `dbadmin.toml` in the working directory.
//! Without a file, the app falls back to a single connection taken from
//! `DBADMIN_DATABASE_URL` (or the dev Postgres), as before.
//!
//! ```toml
//! [project]
//! name = "Checkout v2"
//! workspace = "Online Store"
//!
//! [[connections]]
//! name = "local-dev"
//! url = "postgres://dbadmin:dbadmin@localhost:54329/dbadmin"
//! environment = "development"   # optional; detected from the host if absent
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::connection::Environment;

pub const DEFAULT_URL: &str = "postgres://dbadmin:dbadmin@localhost:54329/dbadmin";

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub project: Project,
    pub connections: Vec<ConnectionConfig>,
    /// Where the configuration came from, to show it in the interface.
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    File(PathBuf),
    Environment,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Project {
    pub name: String,
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionConfig {
    pub name: String,
    pub url: String,
    pub environment: Environment,
}

#[derive(Deserialize)]
struct File {
    project: Project,
    #[serde(default)]
    connections: Vec<FileConnection>,
}

#[derive(Deserialize)]
struct FileConnection {
    name: String,
    url: String,
    #[serde(default)]
    environment: Option<String>,
}

impl Config {
    /// Loads the configuration file if there is one; otherwise builds a single
    /// connection from the environment.
    pub fn load() -> Result<Self, String> {
        let path = std::env::var_os("DBADMIN_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("dbadmin.toml"));
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
            Self::parse(&text, &path)
        } else {
            let url = std::env::var("DBADMIN_DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.into());
            let explicit = std::env::var("DBADMIN_ENVIRONMENT").ok();
            Ok(Self::single(url, explicit.as_deref()))
        }
    }

    pub fn parse(text: &str, path: &Path) -> Result<Self, String> {
        let file: File =
            toml::from_str(text).map_err(|error| format!("Invalid {}: {error}", path.display()))?;
        if file.connections.is_empty() {
            return Err(format!("{} has no [[connections]]", path.display()));
        }
        let mut connections: Vec<ConnectionConfig> = Vec::with_capacity(file.connections.len());
        for connection in file.connections {
            if connections.iter().any(|c| c.name == connection.name) {
                return Err(format!(
                    "{}: connection \"{}\" is defined twice",
                    path.display(),
                    connection.name
                ));
            }
            connections.push(ConnectionConfig {
                environment: Environment::detect(
                    &connection.url,
                    connection.environment.as_deref(),
                ),
                name: connection.name,
                url: connection.url,
            });
        }
        Ok(Self {
            project: file.project,
            connections,
            source: Source::File(path.to_path_buf()),
        })
    }

    fn single(url: String, explicit_environment: Option<&str>) -> Self {
        Self {
            project: Project {
                name: "dbadmin".into(),
                workspace: None,
            },
            connections: vec![ConnectionConfig {
                name: crate::connection::describe_target(&url),
                environment: Environment::detect(&url, explicit_environment),
                url,
            }],
            source: Source::Environment,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, String> {
        Config::parse(text, Path::new("dbadmin.toml"))
    }

    #[test]
    fn reads_the_project_and_its_connections() {
        let config = parse(
            r#"
            [project]
            name = "Checkout v2"
            workspace = "Online Store"

            [[connections]]
            name = "local-dev"
            url = "postgres://u:p@localhost:54329/dbadmin"

            [[connections]]
            name = "prod-replica"
            url = "postgres://u:p@localhost:54329/dbadmin"
            environment = "production"
            "#,
        )
        .unwrap();
        assert_eq!(config.project.name, "Checkout v2");
        assert_eq!(config.project.workspace.as_deref(), Some("Online Store"));
        assert_eq!(config.connections.len(), 2);
        assert_eq!(config.connections[0].environment, Environment::Development);
        assert_eq!(config.connections[1].environment, Environment::Production);
    }

    #[test]
    fn rejects_files_without_connections_or_with_duplicates() {
        assert!(parse("[project]\nname = \"x\"\n").is_err());
        let duplicated = r#"
            [project]
            name = "x"
            [[connections]]
            name = "a"
            url = "postgres://localhost/a"
            [[connections]]
            name = "a"
            url = "postgres://localhost/b"
        "#;
        assert!(parse(duplicated).unwrap_err().contains("defined twice"));
    }

    #[test]
    fn falls_back_to_a_single_connection() {
        let config = Config::single("postgres://u:secret@db.example.com/app".into(), None);
        assert_eq!(config.connections[0].name, "db.example.com/app");
        assert_eq!(config.connections[0].environment, Environment::Production);
    }
}
