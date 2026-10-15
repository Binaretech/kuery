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
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub database: String,
    pub user: String,
    /// libpq `sslmode`; empty uses the driver's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sslmode: Option<String>,
    /// `development`, `staging` or `production`; detected from the host if
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
}

fn default_port() -> u16 {
    5432
}

impl ConnectionSpec {
    pub fn environment(&self) -> Environment {
        Environment::detect(
            &format!("postgres://{}/", self.host),
            self.environment.as_deref(),
        )
    }

    /// `host:port/database`, for display.
    pub fn target(&self) -> String {
        format!("{}:{}/{}", self.host, self.port, self.database)
    }

    /// libpq `key=value` connection string. The password is only ever added
    /// here, in memory.
    pub fn conninfo(&self, password: Option<&str>) -> String {
        let mut parts = vec![
            format!("host={}", quote(&self.host)),
            format!("port={}", self.port),
            format!("dbname={}", quote(&self.database)),
            format!("user={}", quote(&self.user)),
            "application_name=Kuery".to_string(),
        ];
        if let Some(password) = password.filter(|p| !p.is_empty()) {
            parts.push(format!("password={}", quote(password)));
        }
        if let Some(mode) = self.sslmode.as_deref().filter(|m| !m.is_empty()) {
            parts.push(format!("sslmode={}", quote(mode)));
        }
        parts.join(" ")
    }
}

/// libpq quoting: single quotes, with `\` and `'` escaped by a backslash.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
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

    /// Writes `project.toml` atomically (temporary file, then rename).
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
        let temporary = self.dir().join(format!("{FILE}.tmp"));
        std::fs::create_dir_all(self.dir())
            .and_then(|_| std::fs::write(&temporary, format!("{HEADER}{text}")))
            .and_then(|_| std::fs::rename(&temporary, &path))
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
            port: 54329,
            database: "kuery".into(),
            user: "kuery".into(),
            sslmode: None,
            environment: None,
        }
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
    fn builds_a_quoted_conninfo_with_the_password_in_memory() {
        let mut spec = spec("x");
        spec.user = "o'brien".into();
        spec.sslmode = Some("require".into());
        assert_eq!(
            spec.conninfo(Some(r"p\ss")),
            r"host='localhost' port=54329 dbname='kuery' user='o\'brien' application_name=Kuery password='p\\ss' sslmode='require'"
        );
        assert!(!spec.conninfo(None).contains("password"));
        assert_eq!(spec.target(), "localhost:54329/kuery");
        assert_eq!(spec.environment(), Environment::Development);
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
