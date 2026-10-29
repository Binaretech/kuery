//! The drivers Kuery connects with, described as data: their id (what
//! `project.toml` stores), name and aliases for the searchable list, the
//! family of tools they get in the app, and what their connection form asks
//! for. A driver's code lives in its own crate; the app pairs each
//! descriptor here with its implementation of the driver contract.
//!
//! Adding a driver starts here: a [`DriverInfo`] in [`ALL`], then the
//! implementation registered in the app.

use std::collections::BTreeMap;
use std::ops::Range;

use crate::document::adapter::DocumentAdapter;
use crate::document::language::{Language, ParseError};
use crate::session::Access;

/// Which tools a driver gets: the sidebar tree and the tabs it opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    /// Schemas and tables, SQL query tabs, table tabs.
    Sql,
    /// Databases and keys, key tabs, a command console.
    KeyValue,
    /// Databases and collections, collection tabs, a console in the
    /// engine's own language.
    Document,
}

impl Family {
    pub fn label(self) -> &'static str {
        match self {
            Family::Sql => "SQL",
            Family::KeyValue => "Key-value",
            Family::Document => "Document",
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

/// A setting of the connection form beyond host, database, user and TLS,
/// stored in the connection's `options` under `key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionField {
    /// The name the driver reads it by (`authSource`).
    pub key: &'static str,
    pub label: &'static str,
    pub kind: OptionKind,
    pub placeholder: &'static str,
    /// A line under the field.
    pub help: Option<&'static str>,
    /// Offered on production connections too. When false the form hides it
    /// there and the connection does not keep it (allowing invalid
    /// certificates, say).
    pub production: bool,
}

/// How the form asks for an option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionKind {
    Text,
    /// On or off, stored as `true` when on and left out when off.
    Toggle,
    /// One of these values, or none for the driver's default.
    Choice(&'static [&'static str]),
    /// A file's path, picked or typed.
    File,
}

/// Everything the app needs to know about a driver before talking to it.
#[derive(Debug, Clone, Copy)]
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
    /// Its other settings, in the form's order. Only these keys are ever
    /// stored, so nothing else (a password in a pasted string, say) reaches
    /// the project file.
    pub options: &'static [OptionField],
    /// Splits a pasted connection string into the form's fields, for
    /// drivers that have one.
    pub uri: Option<SplitUri>,
    /// Where an SSH tunnel leads for these settings, or why they cannot go
    /// through SSH, asked before `ssh` starts: for a driver whose host field
    /// takes more than one name and port, or that dials the server through
    /// a SOCKS proxy. `None` for the others, whose tunnel is a port
    /// forwarded to the host and port as given.
    pub ssh_route: Option<RouteSsh>,
}

/// Says where an SSH tunnel leads (see [`DriverInfo::ssh_route`]).
pub type RouteSsh = fn(&ConnectOptions) -> Result<SshRoute, String>;

/// Where an SSH tunnel leads (see [`DriverInfo::ssh_route`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshRoute {
    /// A local port forwarded to this host and port, as the SSH host sees
    /// them.
    Forward { host: String, port: u16 },
    /// A SOCKS proxy (`ssh -D`, see [`ConnectOptions::proxy`]) the driver
    /// dials the server through by its own name: for a driver that can
    /// check a TLS certificate only against the name it dials.
    Proxy,
}

/// Splits a connection string (see [`DriverInfo::uri`]).
pub type SplitUri = fn(&str) -> Result<ParsedUri, String>;

/// A connection string split into the form's fields.
#[derive(Clone, PartialEq, Eq)]
pub struct ParsedUri {
    /// One host, or members as `a:27017,b:27018`.
    pub host: String,
    /// The port of a single host that names one.
    pub port: Option<u16>,
    pub database: String,
    pub user: String,
    /// Kept only in memory: the form moves it to the keychain.
    pub password: Option<String>,
    /// The TLS choice as stored (see [`Tls::stored`]), when the string says.
    pub tls: Option<String>,
    /// The driver's options by key (see [`DriverInfo::options`]).
    pub options: BTreeMap<String, String>,
}

impl std::fmt::Debug for ParsedUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedUri")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("tls", &self.tls)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
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

    /// Checks a connection's options against [`Self::options`]: keys it
    /// does not have are refused by name (never with their value, which may
    /// be a secret), and toggles and choices must hold one of their values.
    pub fn check_options(&self, options: &BTreeMap<String, String>) -> Result<(), String> {
        let unknown: Vec<&str> = options
            .keys()
            .map(String::as_str)
            .filter(|key| !self.options.iter().any(|field| field.key == *key))
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "Kuery does not keep these options: {}",
                unknown.join(", ")
            ));
        }
        for field in self.options {
            let Some(value) = options.get(field.key) else {
                continue;
            };
            let allowed: &[&str] = match field.kind {
                OptionKind::Toggle => &["true", "false"],
                OptionKind::Choice(choices) => choices,
                OptionKind::Text | OptionKind::File => continue,
            };
            if !allowed.contains(&value.as_str()) {
                return Err(format!(
                    "{} is one of {}, not {value}",
                    field.label,
                    allowed.join(", ")
                ));
            }
        }
        Ok(())
    }
}

/// Whether a stored TLS choice (see [`Tls::stored`]) turns TLS on.
pub fn tls_on(stored: Option<&str>) -> bool {
    stored.is_some_and(|mode| !mode.is_empty() && mode != "disable")
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
    /// The local port of a SOCKS proxy through SSH, to dial `host:port`
    /// through by name (see [`SshRoute::Proxy`]).
    pub proxy: Option<u16>,
    /// The driver's other settings by key (see [`DriverInfo::options`]).
    pub options: BTreeMap<String, String>,
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
            .field("proxy", &self.proxy)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl ConnectOptions {
    /// Where to open the connection and which name TLS checks: through the
    /// local end of a tunnel when there is one, still checking the
    /// certificate against the server as named.
    pub fn endpoint(&self) -> Endpoint {
        let server_name = self.host.trim().to_string();
        Endpoint {
            host: match self.tunnel {
                Some(_) => "127.0.0.1".into(),
                None => server_name.clone(),
            },
            port: self.tunnel.unwrap_or(self.port),
            server_name,
            tls: tls_on(self.tls.as_deref()),
        }
    }
}

/// See [`ConnectOptions::endpoint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The host to dial.
    pub host: String,
    pub port: u16,
    /// The name the server's certificate must carry.
    pub server_name: String,
    /// Whether the stored choice turns TLS on, for drivers with a toggle.
    pub tls: bool,
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

/// Opens a connection with a document driver.
pub type OpenDocument = fn(&ConnectOptions, Access) -> Result<Box<dyn DocumentAdapter>, String>;

/// A document database driver, which its crate exports as `DRIVER`: what
/// the app shows for it, the language its console speaks, the other
/// languages it can translate from, and how it opens a connection (on the
/// session's thread; see [`crate::document::session::DocumentSession::open`]).
pub struct DocumentDriver {
    pub info: DriverInfo,
    pub language: &'static Language,
    /// Languages the console offers besides [`Self::language`].
    pub frontends: &'static [QueryFrontend],
    pub open: OpenDocument,
}

/// Another language a document console accepts (read-only SQL, say). It
/// never runs by itself: each statement is translated into the driver's own
/// language first, and that text is what the session checks and runs.
pub struct QueryFrontend {
    pub name: &'static str,
    /// The editor's highlighting (`"sql"`).
    pub editor: &'static str,
    /// Where each statement of a script is, without reading it further.
    pub split: fn(&str) -> Vec<Range<usize>>,
    /// The native statement a statement stands for; `None` while the
    /// driver cannot translate yet.
    pub translate: Option<Translate>,
}

/// Translates a statement of a [`QueryFrontend`] into the native one.
pub type Translate = fn(&str) -> Result<String, ParseError>;

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
    options: &[],
    uri: None,
    ssh_route: None,
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
        options: &[],
        uri: None,
        ssh_route: None,
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
        let documents = DriverInfo {
            family: Family::Document,
            ..SERVER
        };
        assert!(documents.matches("document"));
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
    fn keeps_only_the_options_a_driver_has() {
        const OPTIONS: &[OptionField] = &[
            OptionField {
                key: "authSource",
                label: "Authentication database",
                kind: OptionKind::Text,
                placeholder: "admin",
                help: None,
                production: true,
            },
            OptionField {
                key: "srv",
                label: "SRV",
                kind: OptionKind::Toggle,
                placeholder: "",
                help: None,
                production: true,
            },
            OptionField {
                key: "readPreference",
                label: "Read preference",
                kind: OptionKind::Choice(&["primary", "secondary"]),
                placeholder: "",
                help: None,
                production: true,
            },
        ];
        let with = DriverInfo {
            options: OPTIONS,
            ..SERVER
        };
        let options = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect()
        };
        assert!(with.check_options(&options(&[])).is_ok());
        assert!(
            with.check_options(&options(&[
                ("authSource", "admin"),
                ("srv", "true"),
                ("readPreference", "secondary")
            ]))
            .is_ok()
        );
        // By name only: the value may be a secret.
        assert_eq!(
            with.check_options(&options(&[("maxPoolSize", "5"), ("authSource", "admin")]))
                .unwrap_err(),
            "Kuery does not keep these options: maxPoolSize"
        );
        assert!(
            with.check_options(&options(&[("srv", "yes")]))
                .unwrap_err()
                .contains("SRV is one of true, false")
        );
        assert!(
            with.check_options(&options(&[("readPreference", "nearest")]))
                .is_err()
        );
        // A driver without options keeps none.
        assert!(
            SERVER
                .check_options(&options(&[("authSource", "admin")]))
                .is_err()
        );
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

    #[test]
    fn dials_the_tunnel_and_checks_the_certificate_against_the_server() {
        let options = ConnectOptions {
            host: " db.example.com ".into(),
            port: 3306,
            tls: Some("require".into()),
            ..ConnectOptions::default()
        };
        assert_eq!(
            options.endpoint(),
            Endpoint {
                host: "db.example.com".into(),
                port: 3306,
                server_name: "db.example.com".into(),
                tls: true,
            }
        );
        let tunnelled = ConnectOptions {
            tunnel: Some(40000),
            ..options.clone()
        };
        assert_eq!(
            tunnelled.endpoint(),
            Endpoint {
                host: "127.0.0.1".into(),
                port: 40000,
                server_name: "db.example.com".into(),
                tls: true,
            }
        );
        for (stored, tls) in [(None, false), (Some(""), false), (Some("disable"), false)] {
            let options = ConnectOptions {
                tls: stored.map(str::to_string),
                ..tunnelled.clone()
            };
            assert_eq!(options.endpoint().tls, tls, "{stored:?}");
        }
    }
}
