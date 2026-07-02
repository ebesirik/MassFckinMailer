//! Provider abstraction. Concrete implementations (SMTP via lettre, Mailgun,
//! SES, Gmail/Outlook OAuth) land in M1/M6 — this crate defines the contract.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

pub mod account;
pub mod factory;
pub mod gmail;
pub mod mailgun;
pub mod oauth;
pub mod outlook;
pub mod secrets;
pub mod ses;
pub mod smtp;

pub use account::{
    Account, AccountConfig, AccountStore, GmailConfig, MailgunConfig, MailgunRegion, OutlookConfig,
    SesConfig, SmtpConfig, TlsMode,
};
pub use factory::build_provider;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Smtp,
    Mailgun,
    Ses,
    Gmail,
    Outlook,
}

impl ProviderKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Smtp => "Generic SMTP",
            Self::Mailgun => "Mailgun",
            Self::Ses => "AWS SES",
            Self::Gmail => "Gmail (OAuth)",
            Self::Outlook => "Outlook (OAuth)",
        }
    }
}

/// What the engine may assume about a provider.
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    /// Safe default send rate; the engine clamps user config to this.
    pub suggested_rate_per_sec: f32,
    /// Whether per-message delivery status is available immediately.
    pub immediate_status: bool,
}

/// A fully rendered, ready-to-send message.
#[derive(Debug, Clone)]
pub struct RenderedEmail {
    pub to: String,
    pub subject: String,
    pub html_body: String,
    pub text_alt: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SendReceipt {
    /// Provider-side message id, when available (Mailgun/SES return one).
    pub provider_message_id: Option<String>,
}

/// Classification drives retry logic in the engine.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// Worth retrying with backoff: rate limits, 5xx, timeouts.
    #[error("retryable send failure: {0}")]
    Retryable(String),
    /// Do not retry: auth failure, invalid recipient, rejected content.
    #[error("fatal send failure: {0}")]
    Fatal(String),
    /// Cancelled via token — not a failure.
    #[error("send cancelled")]
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("connection failed: {0}")]
    Connection(String),
}

#[async_trait]
pub trait EmailProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn capabilities(&self) -> Capabilities;

    /// Cheap connectivity/credentials check — backs the "Test connection" button.
    async fn verify(&self) -> Result<(), ProviderError>;

    async fn send(
        &self,
        message: &RenderedEmail,
        cancel: &CancellationToken,
    ) -> Result<SendReceipt, SendError>;
}
