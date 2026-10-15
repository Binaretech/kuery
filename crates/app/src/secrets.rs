//! Connection passwords, kept in the OS credential store (Keychain on macOS,
//! Credential Manager on Windows, Secret Service on Linux), never in project
//! files. Each password is stored under the connection's id.

const SERVICE: &str = "kuery";
/// The service passwords were saved under before the app was called Kuery.
/// Reading one from there moves it to [`SERVICE`].
const LEGACY_SERVICE: &str = "dbadmin";

pub trait SecretStore: Send + Sync {
    fn get(&self, id: &str) -> Result<Option<String>, String>;
    fn set(&self, id: &str, secret: &str) -> Result<(), String>;
    fn delete(&self, id: &str) -> Result<(), String>;
}

/// The OS credential store.
pub struct Keyring;

impl SecretStore for Keyring {
    fn get(&self, id: &str) -> Result<Option<String>, String> {
        if let Some(secret) = read(&entry(SERVICE, id)?)? {
            return Ok(Some(secret));
        }
        let legacy = entry(LEGACY_SERVICE, id)?;
        let Some(secret) = read(&legacy)? else {
            return Ok(None);
        };
        // Moved only once it is saved under the new name.
        if entry(SERVICE, id)?.set_password(&secret).is_ok() {
            let _ = legacy.delete_credential();
        }
        Ok(Some(secret))
    }

    fn set(&self, id: &str, secret: &str) -> Result<(), String> {
        entry(SERVICE, id)?.set_password(secret).map_err(describe)
    }

    fn delete(&self, id: &str) -> Result<(), String> {
        for service in [SERVICE, LEGACY_SERVICE] {
            match entry(service, id)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(error) => return Err(describe(error)),
            }
        }
        Ok(())
    }
}

fn entry(service: &str, id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(service, id).map_err(describe)
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
