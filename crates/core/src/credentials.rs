//! Secrets live only in the OS credential store (Windows Credential Manager / macOS Keychain)
//! through the `keyring` crate. Config files hold a reference (the connection id), never a
//! password. `BULKDB_CREDENTIALS=memory` (tests, CI) switches to an in-memory store.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::{CoreError, Result};

pub const SERVICE: &str = "BulkDB";

pub trait CredentialStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
}

pub fn db_password_key(conn_id: &str) -> String {
    format!("{conn_id}/db")
}
pub fn ssh_password_key(conn_id: &str) -> String {
    format!("{conn_id}/ssh")
}
pub fn ssh_passphrase_key(conn_id: &str) -> String {
    format!("{conn_id}/ssh-passphrase")
}

pub struct KeyringStore;

impl CredentialStore for KeyringStore {
    fn get(&self, key: &str) -> Result<Option<String>> {
        let entry = keyring::Entry::new(SERVICE, key).map_err(|e| CoreError::Internal(format!("keyring: {e}")))?;
        match entry.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(CoreError::Internal(format!("keyring: {e}"))),
        }
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        let entry = keyring::Entry::new(SERVICE, key).map_err(|e| CoreError::Internal(format!("keyring: {e}")))?;
        entry.set_password(value).map_err(|e| CoreError::Internal(format!("keyring: {e}")))
    }
    fn delete(&self, key: &str) -> Result<()> {
        let entry = keyring::Entry::new(SERVICE, key).map_err(|e| CoreError::Internal(format!("keyring: {e}")))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(CoreError::Internal(format!("keyring: {e}"))),
        }
    }
}

#[derive(Default)]
pub struct MemoryStore {
    map: Mutex<HashMap<String, String>>,
}

impl CredentialStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.map.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        self.map.lock().unwrap().insert(key.to_string(), value.to_string());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<()> {
        self.map.lock().unwrap().remove(key);
        Ok(())
    }
}

/// Keyring unless `BULKDB_CREDENTIALS=memory`.
pub fn default_store() -> Box<dyn CredentialStore> {
    if std::env::var("BULKDB_CREDENTIALS").map(|v| v == "memory").unwrap_or(false) {
        Box::new(MemoryStore::default())
    } else {
        Box::new(KeyringStore)
    }
}
