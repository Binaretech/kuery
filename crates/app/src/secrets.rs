//! Connection passwords, kept in the OS credential store (Keychain on macOS,
//! Credential Manager on Windows, Secret Service on Linux), never in project
//! files. Each password is stored under the connection's id.

const SERVICE: &str = "dbadmin";

pub trait SecretStore: Send + Sync {
    fn get(&self, id: &str) -> Result<Option<String>, String>;
    fn set(&self, id: &str, secret: &str) -> Result<(), String>;
    fn delete(&self, id: &str) -> Result<(), String>;
}

/// The OS credential store.
pub struct Keyring;

impl SecretStore for Keyring {
    fn get(&self, id: &str) -> Result<Option<String>, String> {
        match entry(id)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(describe(error)),
        }
    }

    fn set(&self, id: &str, secret: &str) -> Result<(), String> {
        entry(id)?.set_password(secret).map_err(describe)
    }

    fn delete(&self, id: &str) -> Result<(), String> {
        match entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(describe(error)),
        }
    }
}

fn entry(id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, id).map_err(describe)
}

fn describe(error: keyring::Error) -> String {
    format!("System keychain: {error}")
}
