//! Outlook / Microsoft 365 provider: send via Microsoft Graph (`me/sendMail`)
//! using an OAuth access token. Graph takes a JSON message (not raw MIME).

use crate::account::OutlookConfig;
use crate::oauth::{self, Endpoints, Tokens};
use crate::{
    Capabilities, EmailProvider, ProviderError, ProviderKind, RenderedEmail, SendError,
    SendReceipt, secrets,
};
use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const SEND_URL: &str = "https://graph.microsoft.com/v1.0/me/sendMail";

pub struct OutlookProvider {
    account_id: String,
    client: reqwest::Client,
    endpoints: Endpoints,
    client_id: String,
    tokens: Mutex<Tokens>,
}

impl OutlookProvider {
    pub fn new(
        account_id: &str,
        config: &OutlookConfig,
        secret: String,
    ) -> Result<Self, ProviderError> {
        let tokens = Tokens::from_json(&secret).map_err(ProviderError::Config)?;
        Ok(Self {
            account_id: account_id.to_string(),
            client: reqwest::Client::new(),
            endpoints: oauth::microsoft_endpoints(&config.tenant),
            client_id: config.client_id.clone(),
            tokens: Mutex::new(tokens),
        })
    }

    async fn access_token(&self) -> Result<String, String> {
        let mut tokens = self.tokens.lock().await;
        if tokens.is_expired() {
            oauth::refresh(&self.client, &self.endpoints, &self.client_id, &mut tokens).await?;
            let _ = secrets::set(&self.account_id, &tokens.to_json());
        }
        Ok(tokens.access_token.clone())
    }
}

#[async_trait]
impl EmailProvider for OutlookProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Outlook
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Personal/365 mailboxes throttle bulk sending; keep it gentle.
            suggested_rate_per_sec: 2.0,
            immediate_status: true,
        }
    }

    async fn verify(&self) -> Result<(), ProviderError> {
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
        let token = self.access_token().await.map_err(SendError::Retryable)?;

        let payload = serde_json::json!({
            "message": {
                "subject": message.subject,
                "body": { "contentType": "HTML", "content": message.html_body },
                "toRecipients": [ { "emailAddress": { "address": message.to } } ],
            },
            "saveToSentItems": true,
        });

        let request = self
            .client
            .post(SEND_URL)
            .bearer_auth(token)
            .json(&payload)
            .send();

        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(SendError::Cancelled),
            res = request => res.map_err(|e| SendError::Retryable(e.to_string()))?,
        };

        let status = resp.status();
        // Graph sendMail returns 202 Accepted with no body and no message id.
        if status.is_success() {
            return Ok(SendReceipt {
                provider_message_id: None,
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
