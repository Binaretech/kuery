//! A project is a folder. Everything Kuery keeps for it lives in
//! `<folder>/.kuery/`, so it can sit inside a code repository:
//!
//! ```text
//! <folder>/.kuery/
//! ├─ project.toml      name and connections (no passwords)
//! └─ queries/          saved queries, plain .sql files, any folder layout
//! ```
//!
//! Passwords go to the OS keychain under each connection's `id` (see
//! [`crate::secrets`]), so the folder can be committed to Git.

use std::path::{Path, PathBuf};

use kuery_core::driver::{ConnectOptions, DriverInfo, Family};

use crate::drivers::{self, Driver};
use serde::{Deserialize, Serialize};

use crate::connection::Environment;

pub const DIR: &str = ".kuery";
/// Where projects kept their files before the app was called Kuery. Opening
/// such a project renames the folder to [`DIR`].
const LEGACY_DIR: &str = ".dbadmin";
const FILE: &str = "project.toml";
const QUERIES: &str = "queries";

#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    /// The folder the user opened.
    pub root: PathBuf,
    pub name: String,
    pub workspace: Option<String>,
    pub connections: Vec<ConnectionSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionSpec {
    /// Stable id: the keychain entry and saved queries refer to it.
    pub id: String,
    pub name: String,
    /// The driver's id (see [`kuery_core::driver`]). Left out of the file
    /// for Postgres, which projects had before there was a choice.
    #[serde(default = "default_driver", skip_serializing_if = "is_default_driver")]
    pub driver: String,
    /// Empty for a database file.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// 0 for a database file.
    #[serde(default = "default_port", skip_serializing_if = "is_zero")]
    pub port: u16,
    /// Postgres: the database name. Redis: the database index the console
    /// starts on (empty is 0).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub database: String,
    /// Postgres: the role. Redis: an ACL user, empty for the default one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// Postgres: libpq `sslmode`, empty for the driver's default. Redis:
    /// anything but `disable` turns TLS on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sslmode: Option<String>,
    /// `development`, `staging` or `production`; detected from the host if
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Reach the server through this SSH host; `host` and `port` are then
    /// the server's as the SSH host sees them (see [`crate::tunnel`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshSpec>,
}

/// An SSH host to tunnel through: a host name or a `~/.ssh/config` alias.
/// Empty fields leave it to `~/.ssh/config` and the SSH agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshSpec {
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// A private key file, `~/` allowed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub identity_file: String,
}

fn default_port() -> u16 {
    5432
}

fn is_zero(port: &u16) -> bool {
    *port == 0
}

/// The driver of connections that name none: projects had Postgres before
/// there was a choice.
const DEFAULT_DRIVER: &str = "postgres";

fn default_driver() -> String {
    DEFAULT_DRIVER.to_string()
}

fn is_default_driver(id: &str) -> bool {
    id == DEFAULT_DRIVER
}

impl ConnectionSpec {
    /// Through SSH the SSH host decides: the server's `localhost` is the
    /// SSH host's, not this machine's. A database file is on this machine.
    pub fn environment(&self) -> Environment {
        let host = match &self.ssh {
            _ if self.is_file() => "localhost",
            Some(ssh) => &ssh.host,
            None => &self.host,
        };
        Environment::detect(&format!("postgres://{host}/"), self.environment.as_deref())
    }

    /// The driver's descriptor; `None` for an id this version does not know.
    pub fn driver_info(&self) -> Option<&'static DriverInfo> {
        drivers::find(&self.driver).map(Driver::info)
    }

    pub fn family(&self) -> Option<Family> {
        self.driver_info().map(|info| info.family)
    }

    /// The SQL it speaks, for SQL drivers.
    pub fn dialect(&self) -> Option<&'static kuery_core::sql::Dialect> {
        match drivers::find(&self.driver)? {
            Driver::Sql(sql) => Some(sql.dialect),
            Driver::Redis => None,
        }
    }

    /// The database is a file (SQLite): `database` is its path.
    pub fn is_file(&self) -> bool {
        self.driver_info().is_some_and(|info| info.file)
    }

    /// The connection with a database file's relative path (or `~/`) made
    /// absolute: relative paths start at the project folder `root`.
    pub fn resolved(&self, root: &Path) -> ConnectionSpec {
        if !self.is_file() {
            return self.clone();
        }
        let written = self.database.trim();
        let path = match written.strip_prefix("~/") {
            Some(rest) => std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| Path::new(&home).join(rest))
                .unwrap_or_else(|| PathBuf::from(written)),
            None if Path::new(written).is_absolute() => PathBuf::from(written),
            // One component at a time, so the path uses the platform's separator.
            None => written
                .split('/')
                .filter(|part| !part.is_empty())
                .fold(root.to_path_buf(), |path, part| path.join(part)),
        };
        ConnectionSpec {
            database: path.display().to_string(),
            ..self.clone()
        }
    }

    /// `host:port/database`, and the SSH host it goes through, for display;
    /// a database file's path.
    pub fn target(&self) -> String {
        if self.is_file() {
            return self.database.clone();
        }
        let target = match self.family() {
            Some(Family::KeyValue) => format!("{}:{}/{}", self.host, self.port, self.redis_db()),
            _ => format!("{}:{}/{}", self.host, self.port, self.database),
        };
        match &self.ssh {
            Some(ssh) => format!("{target} via {}", ssh.host),
            None => target,
        }
    }

    /// The Redis database index the console starts on.
    pub fn redis_db(&self) -> i64 {
        self.database.trim().parse().unwrap_or(0)
    }

    /// Where and how to connect to Redis, through the local end of a tunnel
    /// when `tunnel` gives its port. The password is only ever added here, in
    /// memory.
    pub fn redis_target(&self, password: Option<&str>, tunnel: Option<u16>) -> kuery_redis::Target {
        let endpoint = ConnectOptions {
            host: self.host.clone(),
            port: self.port,
            tls: self.sslmode.clone(),
            tunnel,
            ..ConnectOptions::default()
        }
        .endpoint();
        kuery_redis::Target {
            host: endpoint.host,
            port: endpoint.port,
            db: self.redis_db(),
            username: Some(self.user.trim().to_string()).filter(|user| !user.is_empty()),
            password: password.filter(|p| !p.is_empty()).map(str::to_string),
            tls: endpoint.tls,
            server_name: endpoint.server_name,
        }
    }
}

/// A unique id for a new connection, without pulling in a UUID crate.
pub fn new_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(nanos);
    format!("{:016x}", hasher.finish())
}

#[derive(Serialize, Deserialize)]
struct File {
    project: ProjectSection,
    #[serde(default)]
    connections: Vec<ConnectionSpec>,
}

#[derive(Serialize, Deserialize)]
struct ProjectSection {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
}

const HEADER: &str = "# Kuery project.\n\
                      # Connection passwords are kept in the system keychain, not in this file.\n\n";

impl Project {
    pub fn dir(&self) -> PathBuf {
        self.root.join(DIR)
    }

    pub fn queries_dir(&self) -> PathBuf {
        self.dir().join(QUERIES)
    }

    pub fn is_project(root: &Path) -> bool {
        root.join(DIR).join(FILE).is_file() || root.join(LEGACY_DIR).join(FILE).is_file()
    }

    pub fn open(root: &Path) -> Result<Self, String> {
        migrate_legacy_dir(root)?;
        let path = root.join(DIR).join(FILE);
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
        let file: File = toml::from_str(&text)
            .map_err(|error| format!("Invalid {}: {error}", path.display()))?;
        let mut seen = std::collections::HashSet::new();
        for connection in &file.connections {
            if !seen.insert(connection.id.clone()) {
                return Err(format!(
                    "{}: connection id \"{}\" is used twice",
                    path.display(),
                    connection.id
                ));
            }
        }
        Ok(Self {
            root: root.to_path_buf(),
            name: file.project.name,
            workspace: file.project.workspace,
            connections: file.connections,
        })
    }

    /// Creates `.kuery/` in `root` with an empty project.
    pub fn init(root: &Path, name: &str) -> Result<Self, String> {
        if Self::is_project(root) {
            return Err(format!("{} already has a Kuery project", root.display()));
        }
        let project = Self {
            root: root.to_path_buf(),
            name: name.to_string(),
            workspace: None,
            connections: Vec::new(),
        };
        std::fs::create_dir_all(project.queries_dir())
            .map_err(|error| format!("Could not create {}: {error}", project.dir().display()))?;
        project.save()?;
        Ok(project)
    }

    /// Opens the project in `root`, creating one named after the folder if
    /// there is none yet.
    pub fn open_or_init(root: &Path) -> Result<Self, String> {
        if Self::is_project(root) {
            Self::open(root)
        } else {
            let name = root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled".into());
            Self::init(root, &name)
        }
    }

    /// Writes `project.toml` atomically.
    pub fn save(&self) -> Result<(), String> {
        let file = File {
            project: ProjectSection {
                name: self.name.clone(),
                workspace: self.workspace.clone(),
            },
            connections: self.connections.clone(),
        };
        let text = toml::to_string_pretty(&file).map_err(|error| error.to_string())?;
        let path = self.dir().join(FILE);
        crate::paths::write_atomic(&path, format!("{HEADER}{text}"))
            .map_err(|error| format!("Could not save {}: {error}", path.display()))
    }

    pub fn connection(&self, id: &str) -> Option<&ConnectionSpec> {
        self.connections.iter().find(|c| c.id == id)
    }

    /// Adds the connection, or replaces the one with the same id.
    pub fn upsert_connection(&mut self, spec: ConnectionSpec) -> Result<(), String> {
        if self
            .connections
            .iter()
            .any(|c| c.name == spec.name && c.id != spec.id)
        {
            return Err(format!(
                "There is already a connection named \"{}\"",
                spec.name
            ));
        }
        match self.connections.iter_mut().find(|c| c.id == spec.id) {
            Some(existing) => *existing = spec,
            None => self.connections.push(spec),
        }
        Ok(())
    }

    pub fn remove_connection(&mut self, id: &str) {
        self.connections.retain(|c| c.id != id);
    }

    /// Saved queries and their folders, relative to `queries/`, folders
    /// first and then alphabetically at each level.
    pub fn queries(&self) -> Vec<QueryEntry> {
        let mut entries = Vec::new();
        walk(&self.queries_dir(), Path::new(""), &mut entries);
        entries
    }
}

/// The open project, shared by the window's views. Whoever changes it saves
/// it and calls `cx.notify()`; views that show it observe the entity.
pub struct ProjectModel {
    pub project: Project,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryEntry {
    /// Path relative to `queries/`, e.g. `reports/monthly_sales.sql`.
    pub relative: PathBuf,
    pub is_dir: bool,
}

impl QueryEntry {
    pub fn depth(&self) -> usize {
        self.relative.components().count().saturating_sub(1)
    }

    pub fn name(&self) -> String {
        let name = self
            .relative
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if self.is_dir {
            name
        } else {
            name.trim_end_matches(".sql").to_string()
        }
    }
}

fn walk(dir: &Path, relative: &Path, entries: &mut Vec<QueryEntry>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<(String, bool)> = read
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().ok()?.is_dir();
            let visible = !name.starts_with('.') && (is_dir || name.ends_with(".sql"));
            visible.then_some((name, is_dir))
        })
        .collect();
    items.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    for (name, is_dir) in items {
        let path = relative.join(&name);
        entries.push(QueryEntry {
            relative: path.clone(),
            is_dir,
        });
        if is_dir {
            walk(&dir.join(&name), &path, entries);
        }
    }
}

/// The connection a saved query is bound to: a first line like
/// `-- connection: local-dev`.
pub fn query_connection(sql: &str) -> Option<&str> {
    sql.lines()
        .next()?
        .trim()
        .strip_prefix("--")?
        .trim()
        .strip_prefix("connection:")
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

/// `sql` with its first line set to `-- connection: <name>`.
pub fn with_connection_header(sql: &str, connection: &str) -> String {
    let body = if query_connection(sql).is_some() {
        sql.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
    } else {
        sql
    };
    format!("-- connection: {connection}\n{body}")
}

/// Renames `.dbadmin/` to `.kuery/`, unless `.kuery/` already exists.
fn migrate_legacy_dir(root: &Path) -> Result<(), String> {
    let (legacy, current) = (root.join(LEGACY_DIR), root.join(DIR));
    if current.exists() || !legacy.join(FILE).is_file() {
        return Ok(());
    }
    std::fs::rename(&legacy, &current).map_err(|error| {
        format!(
            "Could not rename {} to {}: {error}",
            legacy.display(),
            current.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("kuery-test-{name}-{}", new_id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn spec(name: &str) -> ConnectionSpec {
        ConnectionSpec {
            id: new_id(),
            name: name.into(),
            host: "localhost".into(),
            driver: "postgres".into(),
            port: 54329,
            database: "kuery".into(),
            user: "kuery".into(),
            sslmode: None,
            environment: None,
            ssh: None,
        }
    }

    #[test]
    fn keeps_postgres_files_as_they_were_and_reads_redis_ones() {
        let postgres = toml::to_string(&spec("local-dev")).unwrap();
        assert!(!postgres.contains("driver"));
        let redis: ConnectionSpec = toml::from_str(
            "id = \"r\"\nname = \"cache\"\ndriver = \"redis\"\nhost = \"localhost\"\nport = 63799\n",
        )
        .unwrap();
        assert_eq!(
            (redis.driver.as_str(), redis.family()),
            ("redis", Some(Family::KeyValue))
        );
        assert_eq!(
            (redis.redis_db(), redis.target()),
            (0, "localhost:63799/0".to_string())
        );
        let target = redis.redis_target(Some(""), None);
        assert_eq!(
            (target.username, target.password, target.tls),
            (None, None, false)
        );
        let tls = ConnectionSpec {
            sslmode: Some("require".into()),
            database: "2".into(),
            user: "reader".into(),
            ..redis
        };
        let target = tls.redis_target(Some("pw"), None);
        assert_eq!((target.db, target.tls), (2, true));
        assert_eq!(target.username.as_deref(), Some("reader"));
        assert!(
            toml::to_string(&tls)
                .unwrap()
                .contains("driver = \"redis\"")
        );
    }

    #[test]
    fn creates_saves_and_reopens_a_project() {
        let dir = TempDir::new("project");
        let mut project = Project::open_or_init(&dir.0).unwrap();
        assert!(dir.0.join(".kuery/project.toml").is_file());
        assert!(dir.0.join(".kuery/queries").is_dir());
        assert!(project.name.starts_with("kuery-test-project"));

        project.upsert_connection(spec("local-dev")).unwrap();
        project.save().unwrap();
        let text = std::fs::read_to_string(dir.0.join(".kuery/project.toml")).unwrap();
        assert!(text.contains("system keychain"));
        assert!(!text.contains("password ="), "{text}");

        let reopened = Project::open(&dir.0).unwrap();
        assert_eq!(reopened, project);
        assert!(Project::init(&dir.0, "again").is_err());
    }

    #[test]
    fn connection_names_are_unique_and_ids_stable() {
        let mut project = Project {
            root: PathBuf::new(),
            name: "p".into(),
            workspace: None,
            connections: vec![],
        };
        let first = spec("a");
        project.upsert_connection(first.clone()).unwrap();
        assert!(project.upsert_connection(spec("a")).is_err());
        let renamed = ConnectionSpec {
            name: "b".into(),
            ..first.clone()
        };
        project.upsert_connection(renamed).unwrap();
        assert_eq!(project.connections.len(), 1);
        assert_eq!(project.connection(&first.id).unwrap().name, "b");
        project.remove_connection(&first.id);
        assert!(project.connections.is_empty());
    }

    #[test]
    fn describes_a_server_connection() {
        let spec = spec("x");
        assert_eq!(spec.target(), "localhost:54329/kuery");
        assert_eq!(spec.environment(), Environment::Development);
    }

    #[test]
    fn finds_database_files_from_the_project_folder() {
        let file = ConnectionSpec {
            driver: "sqlite".into(),
            host: String::new(),
            database: "dev/shop.sqlite".into(),
            ..spec("shop")
        };
        assert!(file.is_file());
        assert_eq!(file.environment(), Environment::Development);
        assert_eq!(file.target(), "dev/shop.sqlite");
        let (root, data) = if cfg!(windows) {
            (r"C:\work\app", r"C:\data\x.db")
        } else {
            ("/work/app", "/data/x.db")
        };
        let root = Path::new(root);
        let expected = root.join("dev").join("shop.sqlite");
        assert_eq!(file.resolved(root).database, expected.display().to_string());
        let absolute = ConnectionSpec {
            database: data.into(),
            ..file.clone()
        };
        assert_eq!(absolute.resolved(root).database, data);
        let production = ConnectionSpec {
            environment: Some("production".into()),
            ..file
        };
        assert_eq!(production.environment(), Environment::Production);
        // Servers are left as they are.
        let server = spec("pg");
        assert_eq!(server.resolved(root), server);
    }

    #[test]
    fn connects_through_ssh_and_lets_the_ssh_host_decide_the_environment() {
        let through = ConnectionSpec {
            ssh: Some(SshSpec {
                host: "bastion.example.com".into(),
                port: Some(2222),
                user: "deploy".into(),
                identity_file: String::new(),
            }),
            ..spec("prod")
        };
        // The server's localhost is the bastion's: production, not this machine.
        assert_eq!(through.environment(), Environment::Production);
        assert_eq!(
            through.target(),
            "localhost:54329/kuery via bastion.example.com"
        );
        let redis = through.redis_target(None, Some(40001));
        assert_eq!((redis.host.as_str(), redis.port), ("127.0.0.1", 40001));
        // TLS still checks the server's certificate, not the tunnel's address.
        assert_eq!(redis.server_name, "localhost");

        let text = toml::to_string(&through).unwrap();
        assert!(
            text.contains("[ssh]\nhost = \"bastion.example.com\"\nport = 2222\nuser = \"deploy\""),
            "{text}"
        );
        assert!(!text.contains("identity_file"));
        let read: ConnectionSpec = toml::from_str(&text).unwrap();
        assert_eq!(read, through);
        assert!(!toml::to_string(&spec("x")).unwrap().contains("ssh"));
    }

    #[test]
    fn lists_saved_queries_folders_first() {
        let dir = TempDir::new("queries");
        let project = Project::init(&dir.0, "q").unwrap();
        let queries = project.queries_dir();
        std::fs::create_dir_all(queries.join("reports")).unwrap();
        std::fs::write(queries.join("reports/monthly.sql"), "select 1").unwrap();
        std::fs::write(queries.join("adhoc.sql"), "select 2").unwrap();
        std::fs::write(queries.join("notes.txt"), "ignored").unwrap();

        let entries = project.queries();
        let names: Vec<(String, usize, bool)> = entries
            .iter()
            .map(|e| (e.name(), e.depth(), e.is_dir))
            .collect();
        assert_eq!(
            names,
            vec![
                ("reports".into(), 0, true),
                ("monthly".into(), 1, false),
                ("adhoc".into(), 0, false),
            ]
        );
    }

    #[test]
    fn reads_and_writes_the_connection_header() {
        assert_eq!(
            query_connection("-- connection: local-dev\nselect 1"),
            Some("local-dev")
        );
        assert_eq!(query_connection("select 1"), None);
        assert_eq!(
            with_connection_header("select 1", "a"),
            "-- connection: a\nselect 1"
        );
        assert_eq!(
            with_connection_header("-- connection: a\nselect 1", "b"),
            "-- connection: b\nselect 1"
        );
    }

    #[test]
    fn opening_a_project_from_before_the_rename_moves_its_folder() {
        let dir = TempDir::new("legacy");
        let legacy = dir.0.join(LEGACY_DIR);
        std::fs::create_dir_all(legacy.join(QUERIES)).unwrap();
        std::fs::write(legacy.join(FILE), "[project]\nname = \"Old\"\n").unwrap();
        std::fs::write(legacy.join(QUERIES).join("a.sql"), "select 1;\n").unwrap();

        assert!(Project::is_project(&dir.0));
        let project = Project::open(&dir.0).unwrap();
        assert_eq!(project.name, "Old");
        assert!(!legacy.exists());
        assert!(project.queries_dir().join("a.sql").is_file());
    }
}
