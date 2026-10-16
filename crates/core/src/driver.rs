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

pub const POSTGRES: DriverInfo = DriverInfo {
    id: "postgres",
    name: "PostgreSQL",
    aliases: &["postgresql", "pg", "psql"],
    family: Family::Sql,
    default_port: 5432,
    database: Field {
        label: "Database",
        placeholder: "postgres",
        required: true,
        numeric: false,
        help: None,
    },
    user: Field {
        label: "User",
        placeholder: "postgres",
        required: true,
        numeric: false,
        help: None,
    },
    tls: Tls::Modes(&["disable", "prefer", "require", "verify-ca", "verify-full"]),
    production_note: "Production connections always open read-only and cannot edit rows.",
    file: false,
};

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
};

pub const MYSQL: DriverInfo = DriverInfo {
    id: "mysql",
    name: "MySQL",
    aliases: &["mariadb", "my", "percona"],
    family: Family::Sql,
    default_port: 3306,
    database: Field {
        label: "Database",
        placeholder: "shop",
        required: false,
        numeric: false,
        help: Some("The database queries start in; the sidebar lists every one you can see."),
    },
    user: Field {
        label: "User",
        placeholder: "root",
        required: true,
        numeric: false,
        help: None,
    },
    tls: Tls::Toggle,
    production_note: "Production connections always open read-only and cannot edit rows.",
    file: false,
};

pub const SQLITE: DriverInfo = DriverInfo {
    id: "sqlite",
    name: "SQLite",
    aliases: &["sqlite3", "file", "embedded"],
    family: Family::Sql,
    default_port: 0,
    database: Field {
        label: "File",
        placeholder: "data/app.sqlite",
        required: true,
        numeric: false,
        help: Some("The database file; a relative path starts at the project folder."),
    },
    user: Field {
        label: "User",
        placeholder: "",
        required: false,
        numeric: false,
        help: None,
    },
    tls: Tls::Toggle,
    production_note: "Production connections open the file read-only and cannot edit rows.",
    file: true,
};

/// Every driver, in the order the list shows them.
pub const ALL: &[&DriverInfo] = &[&POSTGRES, &MYSQL, &SQLITE, &REDIS];

/// The id connections have when they do not name one (projects from before
/// there was a choice).
pub const DEFAULT: &str = POSTGRES.id;

pub fn find(id: &str) -> Option<&'static DriverInfo> {
    ALL.iter().copied().find(|driver| driver.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_found() {
        for (i, a) in ALL.iter().enumerate() {
            assert_eq!(find(a.id), Some(*a));
            assert!(ALL[i + 1..].iter().all(|b| b.id != a.id));
        }
        assert_eq!(find(DEFAULT), Some(&POSTGRES));
        assert_eq!(find("nope"), None);
    }

    #[test]
    fn searches_by_name_alias_and_family() {
        assert!(POSTGRES.matches("postg"));
        assert!(POSTGRES.matches("PG"));
        assert!(POSTGRES.matches("sql"));
        assert!(REDIS.matches("key-value"));
        assert!(REDIS.matches(""));
        assert!(!REDIS.matches("postgres"));
    }

    #[test]
    fn validates_the_form_by_its_fields() {
        assert!(POSTGRES.validate("localhost", "kuery", "kuery").is_ok());
        assert!(
            POSTGRES
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
        assert!(SQLITE.validate("", "shop.sqlite", "").is_ok());
        assert!(SQLITE.validate("", "", "").unwrap_err().contains("file"));
    }

    #[test]
    fn maps_tls_choices_to_what_is_stored() {
        let modes = POSTGRES.tls;
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
