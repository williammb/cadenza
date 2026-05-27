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

use thiserror::Error;

const SERVICE: &str = "cadenza";

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("keyring: {0}")]
    Keyring(#[from] keyring::Error),
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
    let entry = keyring::Entry::new(SERVICE, account)?;
    entry.set_password(password)?;
    Ok(())
}

pub fn get_password(account: &str) -> Result<String> {
    let entry = keyring::Entry::new(SERVICE, account)?;
    match entry.get_password() {
        Ok(s) => Ok(s),
        Err(keyring::Error::NoEntry) => Err(SecretsError::NotFound(account.to_string())),
        Err(e) => Err(SecretsError::Keyring(e)),
    }
}

pub fn delete_password(account: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, account)?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()), // idempotent
        Err(e) => Err(SecretsError::Keyring(e)),
    }
}
