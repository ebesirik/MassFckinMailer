//! Gmail provider: send via the Gmail API (`users.messages.send`) using an
//! OAuth access token. The RFC 822 message is built with lettre and base64url
//! encoded per the API.

use crate::account::GmailConfig;
use crate::oauth::{self, Endpoints, Tokens};
use crate::{
    Capabilities, EmailProvider, ProviderError, ProviderKind, RenderedEmail, SendError, SendReceipt,
    secrets,
};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use lettre::message::{Mailbox, Message, MultiPart, SinglePart};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const SEND_URL: &str = "https://gmail.googleapis.com/gmail/v1/users/me/messages/send";

pub struct GmailProvider {
    account_id: String,
    client: reqwest::Client,
    endpoints: Endpoints,
    client_id: String,
    from: Mailbox,
    tokens: Mutex<Tokens>,
}

impl GmailProvider {
    /// `secret` is the keychain JSON produced by the OAuth connect flow.
    pub fn new(account_id: &str, config: &GmailConfig, secret: String) -> Result<Self, ProviderError> {
        let tokens = Tokens::from_json(&secret).map_err(ProviderError::Config)?;
        let from = config
            .from
            .parse::<Mailbox>()
            .map_err(|e| ProviderError::Config(format!("invalid From address: {e}")))?;
        Ok(Self {
            account_id: account_id.to_string(),
            client: reqwest::Client::new(),
            endpoints: oauth::google_endpoints(),
            client_id: config.client_id.clone(),
            from,
            tokens: Mutex::new(tokens),
        })
    }

    /// Return a valid access token, refreshing (and persisting) if expired.
    async fn access_token(&self) -> Result<String, String> {
        let mut tokens = self.tokens.lock().await;
        if tokens.is_expired() {
            oauth::refresh(&self.client, &self.endpoints, &self.client_id, &mut tokens).await?;
            // Persist rotated tokens so a future launch starts fresh.
            let _ = secrets::set(&self.account_id, &tokens.to_json());
        }
        Ok(tokens.access_token.clone())
    }

    fn build_mime(&self, message: &RenderedEmail) -> Result<Vec<u8>, SendError> {
        let to = message
            .to
            .parse::<Mailbox>()
            .map_err(|e| SendError::Fatal(format!("invalid recipient {}: {e}", message.to)))?;
        let builder = Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(message.subject.clone());
        let email = match &message.text_alt {
            Some(text) => builder.multipart(MultiPart::alternative_plain_html(
                text.clone(),
                message.html_body.clone(),
            )),
            None => builder.singlepart(SinglePart::html(message.html_body.clone())),
        }
        .map_err(|e| SendError::Fatal(format!("failed to build message: {e}")))?;
        Ok(email.formatted())
    }
}

#[async_trait]
impl EmailProvider for GmailProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Gmail
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Personal Gmail has low daily caps; keep the rate gentle.
            suggested_rate_per_sec: 2.0,
            immediate_status: true,
        }
    }

    async fn verify(&self) -> Result<(), ProviderError> {
        // A successful refresh proves the stored credentials are valid without
        // needing a broader scope than gmail.send.
        self.access_token().await.map_err(ProviderError::Auth)?;
        Ok(())
    }

    async fn send(
        &self,
        message: &RenderedEmail,
        cancel: &CancellationToken,
    ) -> Result<SendReceipt, SendError> {
        if cancel.is_cancelled() {
            return Err(SendError::Cancelled);
        }
        let raw = URL_SAFE_NO_PAD.encode(self.build_mime(message)?);
        let token = self
            .access_token()
            .await
            .map_err(SendError::Retryable)?;

        let request = self
            .client
            .post(SEND_URL)
            .bearer_auth(token)
            .json(&serde_json::json!({ "raw": raw }))
            .send();

        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(SendError::Cancelled),
            res = request => res.map_err(|e| SendError::Retryable(e.to_string()))?,
        };

        let status = resp.status();
        if status.is_success() {
            let id = resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v.get("id").and_then(|i| i.as_str().map(String::from)));
            return Ok(SendReceipt {
                provider_message_id: id,
            });
        }

        let body = resp.text().await.unwrap_or_default();
        let detail = format!("{status}: {body}");
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            Err(SendError::Retryable(detail))
        } else {
            Err(SendError::Fatal(detail))
        }
    }
}
