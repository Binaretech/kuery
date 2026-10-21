//! The drivers Kuery connects with, described as data: their id (what
//! `project.toml` stores), name and aliases for the searchable list, the
//! family of tools they get in the app, and what their connection form asks
//! for. A driver's code lives in its own crate; the app pairs each
//! descriptor here with its implementation of the driver contract.
//!
//! Adding a driver starts here: a [`DriverInfo`] in [`ALL`], then the
//! implementation registered in the app.

/// Which tools a driver gets: the sidebar tree and the tabs it opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    /// Schemas and tables, SQL query tabs, table tabs.
    Sql,
    /// Databases and keys, key tabs, a command console.
    KeyValue,
}

impl Family {
    pub fn label(self) -> &'static str {
        match self {
            Family::Sql => "SQL",
            Family::KeyValue => "Key-value",
        }
    }
}

/// A text field of the connection form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub label: &'static str,
    pub placeholder: &'static str,
    pub required: bool,
    /// Only digits (a Redis database index, say).
    pub numeric: bool,
    /// A line under the field.
    pub help: Option<&'static str>,
}

/// How a driver's form asks about TLS. Stored as the connection's
/// `sslmode`: empty for the driver's default, else the chosen option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tls {
    /// Named modes (libpq's `sslmode`, say); none chosen is the default.
    Modes(&'static [&'static str]),
    /// On, verifying the server's certificate, or off.
    Toggle,
}

impl Tls {
    /// The choices the form offers, the default first.
    pub fn options(self) -> Vec<&'static str> {
        match self {
            Tls::Modes(modes) => std::iter::once("default")
                .chain(modes.iter().copied())
                .collect(),
            Tls::Toggle => vec!["disabled", "enabled"],
        }
    }

    /// What the connection stores for the option at `index`.
    pub fn stored(self, index: usize) -> Option<String> {
        match self {
            Tls::Modes(modes) => index
                .checked_sub(1)
                .and_then(|i| modes.get(i))
                .map(|m| m.to_string()),
            Tls::Toggle => (index == 1).then(|| "require".to_string()),
        }
    }

    /// The option a stored value stands for.
    pub fn index(self, stored: Option<&str>) -> usize {
        let Some(stored) = stored.filter(|s| !s.is_empty()) else {
            return 0;
        };
        match self {
            Tls::Modes(modes) => modes.iter().position(|m| *m == stored).map_or(0, |i| i + 1),
            Tls::Toggle => usize::from(stored != "disable"),
        }
    }
}

/// Everything the app needs to know about a driver before talking to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverInfo {
    /// Stable id, stored in `project.toml`.
    pub id: &'static str,
    pub name: &'static str,
    /// Other names to find it by in the list.
    pub aliases: &'static [&'static str],
    pub family: Family,
    pub default_port: u16,
    pub database: Field,
    pub user: Field,
    pub tls: Tls,
    /// What production means for it, under the form's environment.
    pub production_note: &'static str,
    /// The database is a file: the form asks for its path (in the database
    /// field), not for a server, a user or TLS.
    pub file: bool,
    /// A line under the host field, for hosts that take more than a name.
    pub host_help: Option<&'static str>,
}

impl DriverInfo {
    /// Whether the list's search finds it: by name, id or alias, ignoring
    /// case.
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || self.name.to_lowercase().contains(&query)
            || self.id.contains(&query)
            || self.aliases.iter().any(|alias| alias.contains(&query))
            || self.family.label().to_lowercase().contains(&query)
    }

    /// Checks the form's database and user against the fields.
    pub fn validate(&self, host: &str, database: &str, user: &str) -> Result<(), String> {
        if !self.file && host.trim().is_empty() {
            return Err("The host is required".into());
        }
        for (field, value) in [(&self.database, database), (&self.user, user)] {
            let value = value.trim();
            if field.required && value.is_empty() {
                return Err(format!("The {} is required", field.label.to_lowercase()));
            }
            if field.numeric && !value.is_empty() && value.parse::<u32>().is_err() {
                return Err(format!(
                    "The {} is a number, such as {}",
                    field.label.to_lowercase(),
                    field.placeholder
                ));
            }
        }
        Ok(())
    }
}

/// Where and how to reach a database, the same for every driver: each
/// reads what it needs (a SQLite file has only `database`, its path).
#[derive(Clone, Default)]
pub struct ConnectOptions {
    /// The server as named (also the name TLS checks).
    pub host: String,
    pub port: u16,
    /// The database, or a database file's path.
    pub database: String,
    pub user: String,
    /// Only ever in memory.
    pub password: Option<String>,
    /// The TLS choice as stored (see [`Tls::stored`]); `None` for the
    /// driver's default.
    pub tls: Option<String>,
    /// The local port of an SSH tunnel to connect to instead of
    /// `host:port`.
    pub tunnel: Option<u16>,
}

impl std::fmt::Debug for ConnectOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectOptions")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("tls", &self.tls)
            .field("tunnel", &self.tunnel)
            .finish_non_exhaustive()
    }
}

/// Opens a connection with a driver.
pub type Open =
    fn(&ConnectOptions, crate::session::Access) -> Result<Box<dyn crate::adapter::Adapter>, String>;

/// A SQL driver, which its crate exports as `DRIVER`: what the app shows
/// for it, the SQL it speaks, and how it opens a connection (on the
/// session's thread; see [`crate::session::Session::open`]).
pub struct SqlDriver {
    pub info: DriverInfo,
    pub dialect: &'static crate::sql::Dialect,
    pub open: Open,
}

pub const REDIS: DriverInfo = DriverInfo {
    id: "redis",
    name: "Redis",
    aliases: &["cache", "key-value", "kv"],
    family: Family::KeyValue,
    default_port: 6379,
    database: Field {
        label: "Database",
        placeholder: "0",
        required: false,
        numeric: true,
        help: Some("The database index the console starts on; the sidebar lists them all."),
    },
    user: Field {
        label: "User",
        placeholder: "default",
        required: false,
        numeric: false,
        help: Some("An ACL user; leave empty for the default user."),
    },
    tls: Tls::Toggle,
    production_note: "Production connections only write once writes are allowed for the session.",
    file: false,
    host_help: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    const FIELD: Field = Field {
        label: "Database",
        placeholder: "postgres",
        required: true,
        numeric: false,
        help: None,
    };

    /// A server with named TLS modes, like PostgreSQL.
    const SERVER: DriverInfo = DriverInfo {
        id: "server",
        name: "Some Server",
        aliases: &["pg"],
        family: Family::Sql,
        default_port: 5432,
        database: FIELD,
        user: Field {
            label: "User",
            ..FIELD
        },
        tls: Tls::Modes(&["disable", "prefer", "require"]),
        production_note: "",
        file: false,
        host_help: None,
    };

    /// A database file, like SQLite.
    const FILE: DriverInfo = DriverInfo {
        id: "file",
        name: "Some File",
        database: Field {
            label: "File",
            ..FIELD
        },
        user: Field {
            required: false,
            ..FIELD
        },
        file: true,
        ..SERVER
    };

    #[test]
    fn searches_by_name_alias_and_family() {
        assert!(SERVER.matches("some s"));
        assert!(SERVER.matches("PG"));
        assert!(SERVER.matches("sql"));
        assert!(REDIS.matches("key-value"));
        assert!(REDIS.matches(""));
        assert!(!REDIS.matches("server"));
    }

    #[test]
    fn validates_the_form_by_its_fields() {
        assert!(SERVER.validate("localhost", "kuery", "kuery").is_ok());
        assert!(
            SERVER
                .validate("localhost", "", "kuery")
                .unwrap_err()
                .contains("database")
        );
        assert!(REDIS.validate("localhost", "", "").is_ok());
        assert!(
            REDIS
                .validate("localhost", "two", "")
                .unwrap_err()
                .contains("number")
        );
        assert!(REDIS.validate(" ", "0", "").is_err());
        // A file needs its path, not a host.
        assert!(FILE.validate("", "shop.sqlite", "").is_ok());
        assert!(FILE.validate("", "", "").unwrap_err().contains("file"));
    }

    #[test]
    fn maps_tls_choices_to_what_is_stored() {
        let modes = SERVER.tls;
        assert_eq!(modes.options()[0], "default");
        assert_eq!(modes.stored(0), None);
        assert_eq!(modes.stored(3).as_deref(), Some("require"));
        assert_eq!(modes.index(Some("require")), 3);
        assert_eq!(modes.index(None), 0);
        let toggle = REDIS.tls;
        assert_eq!(toggle.options(), ["disabled", "enabled"]);
        assert_eq!(toggle.stored(1).as_deref(), Some("require"));
        assert_eq!((toggle.index(Some("require")), toggle.index(None)), (1, 0));
    }
}
