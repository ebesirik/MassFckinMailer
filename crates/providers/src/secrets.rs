//! OS keychain access for account secrets.
//!
//! Every account's secret (SMTP password, Mailgun API key, …) is stored under
//! the service `massfckinmailer` with the account id as the entry name. The
//! backend is the platform-native store (Windows Credential Manager, macOS
//! Keychain, Linux Secret Service) selected via keyring features in Cargo.toml.

const SERVICE: &str = "massfckinmailer";

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("keychain error: {0}")]
    Keyring(#[from] keyring::Error),
}

fn entry(account_id: &str) -> Result<keyring::Entry, SecretError> {
    Ok(keyring::Entry::new(SERVICE, account_id)?)
}

/// Store (or overwrite) the secret for an account.
pub fn set(account_id: &str, secret: &str) -> Result<(), SecretError> {
    entry(account_id)?.set_password(secret)?;
    Ok(())
}

/// Fetch the secret for an account. Returns `Ok(None)` if no entry exists.
pub fn get(account_id: &str) -> Result<Option<String>, SecretError> {
    match entry(account_id)?.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Delete an account's secret. Missing entries are treated as success.
pub fn delete(account_id: &str) -> Result<(), SecretError> {
    match entry(account_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
