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
//!
//! The Jira API token (Slice 3) reuses the same store under a distinct
//! `"jira:<base_url>"` account namespace. Like the PG password, the token
//! is NEVER logged: this module makes zero log calls — keep it that way.

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

/// Keyring account for the Jira API token. Distinct namespace from the
/// PG "{user}@{host}:{port}/{database}" account format — the `jira:`
/// prefix can never collide with a PG account string. Keyed on the
/// `base_url` (one token per Jira site), matching config's single
/// `jira` block.
pub fn jira_account_for(base_url: &str) -> String {
    format!("jira:{base_url}")
}

// `set`/`delete` are the write side of the Jira token, called by the Tauri
// `set_jira_token`/`clear_jira_token` commands. The HTTP client only reads
// (`get_jira_token`).
pub fn set_jira_token(base_url: &str, token: &str) -> Result<()> {
    set_password(&jira_account_for(base_url), token)
}

pub fn get_jira_token(base_url: &str) -> Result<String> {
    get_password(&jira_account_for(base_url))
}

pub fn delete_jira_token(base_url: &str) -> Result<()> {
    delete_password(&jira_account_for(base_url))
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
