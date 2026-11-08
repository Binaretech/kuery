//! Connection passwords, kept in the OS credential store (Keychain on macOS,
//! Credential Manager on Windows, Secret Service on Linux), never in project
//! files.
//!
//! Each password is stored under its connection's id *and* where that
//! connection goes (see [`key`]), so a project file cannot borrow another
//! project's password by reusing its id with a server of its own: a
//! connection whose host, user, TLS or tunnel differ from the one the
//! password was saved for finds none, and asks for it again.

use sha2::{Digest, Sha256};

use crate::project::ConnectionSpec;

const SERVICE: &str = "kuery";
/// The service passwords were saved under before the app was called Kuery.
/// Reading one from there moves it to [`SERVICE`].
const LEGACY_SERVICE: &str = "dbadmin";

/// Raw entries by account name; [`password`], [`save`], [`rebind`] and
/// [`forget`] name them for a connection.
pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>, String>;
    fn set(&self, account: &str, secret: &str) -> Result<(), String>;
    fn delete(&self, account: &str) -> Result<(), String>;
}

/// The account `spec`'s password is stored under: its id and a digest of
/// everything that decides where the password is sent (driver, host, port,
/// database, user, TLS, SSH tunnel and driver options). Its name and
/// environment are left out, which only label it.
pub fn key(spec: &ConnectionSpec) -> String {
    let target = ConnectionSpec {
        name: String::new(),
        environment: None,
        ..spec.clone()
    };
    let canonical = serde_json::to_vec(&target).expect("a connection serializes");
    let digest = Sha256::digest(&canonical);
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{}@{hex}", spec.id)
}

/// `spec`'s saved password.
///
/// One saved before passwords were keyed by their target, under the bare
/// id, is taken the first time a connection with that id asks for it and
/// moved to that connection's key, so it can't be read under the bare id
/// again.
pub fn password(store: &dyn SecretStore, spec: &ConnectionSpec) -> Result<Option<String>, String> {
    let key = key(spec);
    if let Some(secret) = store.get(&key)? {
        return Ok(Some(secret));
    }
    let Some(secret) = store.get(&spec.id)? else {
        return Ok(None);
    };
    // Moved only once it is saved under the new key.
    if store.set(&key, &secret).is_ok() {
        let _ = store.delete(&spec.id);
    }
    Ok(Some(secret))
}

/// Saves `secret` as `spec`'s password.
pub fn save(store: &dyn SecretStore, spec: &ConnectionSpec, secret: &str) -> Result<(), String> {
    store.set(&key(spec), secret)
}

/// Moves the password saved for `old` to `new`, the same connection as the
/// user edited it. Nothing to move is not an error.
pub fn rebind(
    store: &dyn SecretStore,
    old: &ConnectionSpec,
    new: &ConnectionSpec,
) -> Result<(), String> {
    let (from, to) = (key(old), key(new));
    if from == to {
        return Ok(());
    }
    let Some(secret) = password(store, old)? else {
        return Ok(());
    };
    store.set(&to, &secret)?;
    store.delete(&from)
}

/// Removes `spec`'s password, and one still under its bare id.
pub fn forget(store: &dyn SecretStore, spec: &ConnectionSpec) -> Result<(), String> {
    store.delete(&key(spec))?;
    store.delete(&spec.id)
}

/// The OS credential store.
pub struct Keyring;

impl SecretStore for Keyring {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        if let Some(secret) = read(&entry(SERVICE, account)?)? {
            return Ok(Some(secret));
        }
        let legacy = entry(LEGACY_SERVICE, account)?;
        let Some(secret) = read(&legacy)? else {
            return Ok(None);
        };
        // Moved only once it is saved under the new name.
        if entry(SERVICE, account)?.set_password(&secret).is_ok() {
            let _ = legacy.delete_credential();
        }
        Ok(Some(secret))
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        entry(SERVICE, account)?
            .set_password(secret)
            .map_err(describe)
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        for service in [SERVICE, LEGACY_SERVICE] {
            match entry(service, account)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(error) => return Err(describe(error)),
            }
        }
        Ok(())
    }
}

fn entry(service: &str, account: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(service, account).map_err(describe)
}

fn read(entry: &keyring::Entry) -> Result<Option<String>, String> {
    match entry.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(describe(error)),
    }
}

fn describe(error: keyring::Error) -> String {
    format!("System keychain: {error}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::project::SshSpec;

    #[derive(Default)]
    struct Memory(Mutex<HashMap<String, String>>);

    impl SecretStore for Memory {
        fn get(&self, account: &str) -> Result<Option<String>, String> {
            Ok(self.0.lock().unwrap().get(account).cloned())
        }
        fn set(&self, account: &str, secret: &str) -> Result<(), String> {
            self.0.lock().unwrap().insert(account.into(), secret.into());
            Ok(())
        }
        fn delete(&self, account: &str) -> Result<(), String> {
            self.0.lock().unwrap().remove(account);
            Ok(())
        }
    }

    fn spec() -> ConnectionSpec {
        toml::from_str(
            r#"
            id = "prod"
            name = "prod"
            host = "db.internal"
            port = 5432
            database = "shop"
            user = "app"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn another_server_with_the_same_id_gets_no_password() {
        let store = Memory::default();
        save(&store, &spec(), "secret").unwrap();
        let elsewhere = ConnectionSpec {
            host: "attacker.example".into(),
            ..spec()
        };
        assert_eq!(password(&store, &elsewhere).unwrap(), None);
        assert_eq!(
            password(&store, &spec()).unwrap().as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn anything_that_changes_where_the_password_goes_changes_the_key() {
        let base = key(&spec());
        let changed = [
            ConnectionSpec {
                port: 6543,
                ..spec()
            },
            ConnectionSpec {
                user: "admin".into(),
                ..spec()
            },
            ConnectionSpec {
                database: "other".into(),
                ..spec()
            },
            ConnectionSpec {
                driver: "mysql".into(),
                ..spec()
            },
            ConnectionSpec {
                sslmode: Some("disable".into()),
                ..spec()
            },
            ConnectionSpec {
                ssh: Some(SshSpec {
                    host: "bastion".into(),
                    port: None,
                    user: String::new(),
                    identity_file: String::new(),
                }),
                ..spec()
            },
            ConnectionSpec {
                options: [("tlsAllowInvalidCertificates".into(), "true".into())].into(),
                ..spec()
            },
        ];
        for other in changed {
            assert_ne!(key(&other), base, "{other:?}");
        }
    }

    #[test]
    fn renaming_or_relabelling_keeps_the_key() {
        let relabelled = ConnectionSpec {
            name: "Production".into(),
            environment: Some("production".into()),
            ..spec()
        };
        assert_eq!(key(&relabelled), key(&spec()));
    }

    #[test]
    fn a_password_under_the_bare_id_moves_to_the_first_connection_that_reads_it() {
        let store = Memory::default();
        store.set("prod", "old").unwrap();
        assert_eq!(password(&store, &spec()).unwrap().as_deref(), Some("old"));
        assert_eq!(store.get("prod").unwrap(), None);
        let elsewhere = ConnectionSpec {
            host: "attacker.example".into(),
            ..spec()
        };
        assert_eq!(password(&store, &elsewhere).unwrap(), None);
    }

    #[test]
    fn rebinding_follows_an_edit() {
        let store = Memory::default();
        save(&store, &spec(), "secret").unwrap();
        let moved = ConnectionSpec {
            host: "db2.internal".into(),
            ..spec()
        };
        rebind(&store, &spec(), &moved).unwrap();
        assert_eq!(password(&store, &moved).unwrap().as_deref(), Some("secret"));
        assert_eq!(password(&store, &spec()).unwrap(), None);
    }

    #[test]
    fn forgetting_removes_both_keys() {
        let store = Memory::default();
        save(&store, &spec(), "secret").unwrap();
        store.set("prod", "old").unwrap();
        forget(&store, &spec()).unwrap();
        assert!(store.0.lock().unwrap().is_empty());
    }
}
