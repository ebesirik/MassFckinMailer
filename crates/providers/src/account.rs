//! Account model and the global accounts store.
//!
//! Accounts are app-global config, not per-project: non-secret metadata lives in
//! `~/.config/massfckinmailer/accounts.toml`, while secrets (SMTP password,
//! Mailgun API key, …) live in the OS keychain under
//! `massfckinmailer/{account_id}` (see [`crate::secrets`]). A project file only
//! references an account by id + display name, so sharing it leaks nothing.

use crate::ProviderKind;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A configured sending account. Contains only non-secret metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    /// Stable id, e.g. `acct_1a2b3c`. Also the keychain entry name.
    pub id: String,
    /// Human-friendly label shown in the UI and stored in project files.
    pub display: String,
    #[serde(flatten)]
    pub config: AccountConfig,
}

impl Account {
    pub fn kind(&self) -> ProviderKind {
        match self.config {
            AccountConfig::Smtp(_) => ProviderKind::Smtp,
            AccountConfig::Mailgun(_) => ProviderKind::Mailgun,
            AccountConfig::Ses(_) => ProviderKind::Ses,
            AccountConfig::Gmail(_) => ProviderKind::Gmail,
            AccountConfig::Outlook(_) => ProviderKind::Outlook,
        }
    }
}

/// Per-provider non-secret configuration. Internally tagged by `kind` so the
/// TOML reads naturally (`kind = "smtp"` alongside the provider's own fields).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccountConfig {
    Smtp(SmtpConfig),
    Mailgun(MailgunConfig),
    Ses(SesConfig),
    Gmail(GmailConfig),
    Outlook(OutlookConfig),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// Upgrade a plaintext connection via STARTTLS (typically port 587).
    StartTls,
    /// Implicit TLS from the first byte (typically port 465).
    Tls,
    /// No encryption — only for trusted local relays.
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub tls: TlsMode,
    /// SMTP auth username (often the full email address).
    pub username: String,
    /// `From:` address used for outgoing mail.
    pub from: String,
    // Password lives in the keychain.
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailgunRegion {
    Us,
    Eu,
}

impl MailgunRegion {
    /// API base host for this region.
    pub fn api_base(&self) -> &'static str {
        match self {
            Self::Us => "https://api.mailgun.net",
            Self::Eu => "https://api.eu.mailgun.net",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailgunConfig {
    /// Sending domain configured in Mailgun, e.g. `news.example.com`.
    pub domain: String,
    #[serde(default = "default_region")]
    pub region: MailgunRegion,
    /// `From:` address (must belong to the configured domain).
    pub from: String,
    // API key lives in the keychain.
}

fn default_region() -> MailgunRegion {
    MailgunRegion::Us
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SesConfig {
    /// AWS region, e.g. `us-east-1`.
    pub region: String,
    /// `From:` address (must be a verified SES identity).
    pub from: String,
    // Access key id + secret access key live in the keychain (two lines).
}

/// Gmail via OAuth. The user registers their own Google Cloud OAuth client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GmailConfig {
    pub client_id: String,
    /// The authenticated Gmail address (used as `From:`).
    pub from: String,
    // client secret + OAuth tokens live in the keychain (JSON).
}

/// Outlook/Microsoft 365 via OAuth (Microsoft Graph). The user registers their
/// own Azure app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutlookConfig {
    pub client_id: String,
    /// Azure tenant (`common`, `organizations`, or a tenant id).
    #[serde(default = "default_tenant")]
    pub tenant: String,
    /// The authenticated address (used as `From:`).
    pub from: String,
    // OAuth tokens live in the keychain (JSON).
}

fn default_tenant() -> String {
    "common".to_string()
}

/// The on-disk accounts file: a flat list of accounts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountStore {
    #[serde(default, rename = "account")]
    pub accounts: Vec<Account>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("failed to access accounts file: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid accounts file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("failed to serialize accounts: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("could not determine a config directory for this platform")]
    NoConfigDir,
}

impl AccountStore {
    /// Default path: `{config_dir}/massfckinmailer/accounts.toml`.
    pub fn default_path() -> Result<PathBuf, StoreError> {
        let dir = dirs::config_dir().ok_or(StoreError::NoConfigDir)?;
        Ok(dir.join("massfckinmailer").join("accounts.toml"))
    }

    /// Load from the default path. A missing file is treated as an empty store.
    pub fn load() -> Result<Self, StoreError> {
        Self::load_from(&Self::default_path()?)
    }

    pub fn load_from(path: &std::path::Path) -> Result<Self, StoreError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Persist to the default path, creating parent directories as needed.
    pub fn save(&self) -> Result<(), StoreError> {
        self.save_to(&Self::default_path()?)
    }

    pub fn save_to(&self, path: &std::path::Path) -> Result<(), StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    /// Insert a new account or replace an existing one with the same id.
    pub fn upsert(&mut self, account: Account) {
        match self.accounts.iter_mut().find(|a| a.id == account.id) {
            Some(slot) => *slot = account,
            None => self.accounts.push(account),
        }
    }

    /// Remove an account by id, returning it if present.
    pub fn remove(&mut self, id: &str) -> Option<Account> {
        let idx = self.accounts.iter().position(|a| a.id == id)?;
        Some(self.accounts.remove(idx))
    }
}

/// Generate a short, unique-enough account id from the current time.
pub fn new_account_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("acct_{:x}", nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn smtp_account() -> Account {
        Account {
            id: "acct_1".into(),
            display: "Work SMTP".into(),
            config: AccountConfig::Smtp(SmtpConfig {
                host: "smtp.example.com".into(),
                port: 587,
                tls: TlsMode::StartTls,
                username: "me@example.com".into(),
                from: "me@example.com".into(),
            }),
        }
    }

    fn mailgun_account() -> Account {
        Account {
            id: "acct_2".into(),
            display: "Mailgun — news.example.com".into(),
            config: AccountConfig::Mailgun(MailgunConfig {
                domain: "news.example.com".into(),
                region: MailgunRegion::Eu,
                from: "hello@news.example.com".into(),
            }),
        }
    }

    fn ses_account() -> Account {
        Account {
            id: "acct_3".into(),
            display: "SES us-east-1".into(),
            config: AccountConfig::Ses(SesConfig {
                region: "us-east-1".into(),
                from: "hello@example.com".into(),
            }),
        }
    }

    #[test]
    fn store_toml_round_trip() {
        let store = AccountStore {
            accounts: vec![smtp_account(), mailgun_account(), ses_account()],
        };
        let text = toml::to_string_pretty(&store).unwrap();
        let parsed: AccountStore = toml::from_str(&text).unwrap();
        assert_eq!(store, parsed);
        assert_eq!(parsed.accounts[2].kind(), ProviderKind::Ses);
    }

    #[test]
    fn kind_reflects_config() {
        assert_eq!(smtp_account().kind(), ProviderKind::Smtp);
        assert_eq!(mailgun_account().kind(), ProviderKind::Mailgun);
    }

    #[test]
    fn mailgun_region_defaults_to_us() {
        let text = r#"
            [[account]]
            id = "a"
            display = "d"
            kind = "mailgun"
            domain = "x.com"
            from = "h@x.com"
        "#;
        let store: AccountStore = toml::from_str(text).unwrap();
        match &store.accounts[0].config {
            AccountConfig::Mailgun(c) => assert_eq!(c.region, MailgunRegion::Us),
            _ => panic!("expected mailgun"),
        }
    }

    #[test]
    fn upsert_and_remove() {
        let mut store = AccountStore::default();
        store.upsert(smtp_account());
        store.upsert(smtp_account()); // same id -> replace, not duplicate
        assert_eq!(store.accounts.len(), 1);
        assert!(store.get("acct_1").is_some());
        assert!(store.remove("acct_1").is_some());
        assert!(store.accounts.is_empty());
    }

    #[test]
    fn missing_file_is_empty_store() {
        let path = std::env::temp_dir().join("mmm_does_not_exist_12345.toml");
        let store = AccountStore::load_from(&path).unwrap();
        assert!(store.accounts.is_empty());
    }
}
