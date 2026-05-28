//! OS keyring wrapper for the Postgres password.
//!
//! Per CLAUDE.md hard constraints: the Postgres password must NEVER
//! touch the disk in cleartext. Windows uses Credential Manager, macOS
//! uses Keychain, Linux uses libsecret — all behind the `keyring`
//! crate's portable API.
//!
//! The service name `"cadenza"` and a user-supplied account string
//! (typically `"{user}@{host}:{port}/{database}"`) jointly identify
//! the entry, so multiple Cadenza profiles on the same machine don't
//! collide.

use std::sync::Once;

use keyring_core::{Entry, Error as KeyringError};
use thiserror::Error;

const SERVICE: &str = "cadenza";

/// keyring 4 no longer bundles a default credential store: the process
/// must register one before any `Entry` is used. We register the
/// OS-native store (Windows Credential Manager / macOS Keychain /
/// Linux keyutils) once, lazily, on first access — so callers in
/// `commands.rs` and the migration runner don't each need an init hook.
static STORE_INIT: Once = Once::new();

fn ensure_store() {
    STORE_INIT.call_once(|| {
        // Best-effort: if registration fails, the subsequent Entry call
        // surfaces a `NoDefaultStore` error rather than panicking here.
        // `true` selects the persistent Secret Service (libsecret) store
        // on Linux instead of the non-persistent kernel keyutils — matching
        // keyring 3's behavior and this module's doc above. No-op on
        // Windows/macOS, which always use the OS-native store.
        let _ = keyring::use_native_store(true);
    });
}

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("keyring: {0}")]
    Keyring(#[from] KeyringError),
    #[error("password not set for account: {0}")]
    NotFound(String),
}

pub type Result<T> = std::result::Result<T, SecretsError>;

/// Build the account string used as the keyring key. Kept here so
/// `commands.rs` and the migration runner agree on the format.
pub fn account_for(user: &str, host: &str, port: u16, database: &str) -> String {
    format!("{user}@{host}:{port}/{database}")
}

pub fn set_password(account: &str, password: &str) -> Result<()> {
    ensure_store();
    let entry = Entry::new(SERVICE, account)?;
    entry.set_password(password)?;
    Ok(())
}

pub fn get_password(account: &str) -> Result<String> {
    ensure_store();
    let entry = Entry::new(SERVICE, account)?;
    match entry.get_password() {
        Ok(s) => Ok(s),
        Err(KeyringError::NoEntry) => Err(SecretsError::NotFound(account.to_string())),
        Err(e) => Err(SecretsError::Keyring(e)),
    }
}

pub fn delete_password(account: &str) -> Result<()> {
    ensure_store();
    let entry = Entry::new(SERVICE, account)?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(KeyringError::NoEntry) => Ok(()), // idempotent
        Err(e) => Err(SecretsError::Keyring(e)),
    }
}
