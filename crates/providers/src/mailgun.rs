//! Mailgun provider via the HTTP API (`reqwest`, rustls).
//!
//! We send one request per recipient rather than using Mailgun's batch feature,
//! so the engine gets an accurate per-row status and message id.

use crate::account::MailgunConfig;
use crate::{
    Capabilities, EmailProvider, ProviderError, ProviderKind, RenderedEmail, SendError, SendReceipt,
};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub struct MailgunProvider {
    client: reqwest::Client,
    api_key: String,
    api_base: String,
    domain: String,
    from: String,
}

impl MailgunProvider {
    pub fn new(config: &MailgunConfig, api_key: String) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| ProviderError::Config(e.to_string()))?;
        Ok(Self {
            client,
            api_key,
            api_base: config.region.api_base().to_string(),
            domain: config.domain.clone(),
            from: config.from.clone(),
        })
    }

    fn messages_url(&self) -> String {
        format!("{}/v3/{}/messages", self.api_base, self.domain)
    }

    fn domain_url(&self) -> String {
        format!("{}/v3/domains/{}", self.api_base, self.domain)
    }
}

#[derive(serde::Deserialize)]
struct MailgunSendResponse {
    id: Option<String>,
}

#[async_trait]
impl EmailProvider for MailgunProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Mailgun
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            suggested_rate_per_sec: 25.0,
            immediate_status: true,
        }
    }

    async fn verify(&self) -> Result<(), ProviderError> {
        let resp = self
            .client
            .get(self.domain_url())
            .basic_auth("api", Some(&self.api_key))
            .send()
            .await
            .map_err(|e| ProviderError::Connection(e.to_string()))?;

        match resp.status() {
            s if s.is_success() => Ok(()),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
                Err(ProviderError::Auth("Mailgun rejected the API key".into()))
            }
            reqwest::StatusCode::NOT_FOUND => Err(ProviderError::Config(format!(
                "domain {} not found on Mailgun ({})",
                self.domain, self.api_base
            ))),
            other => Err(ProviderError::Connection(format!(
                "unexpected Mailgun response: {other}"
            ))),
        }
    }

    async fn send(
        &self,
        message: &RenderedEmail,
        cancel: &CancellationToken,
    ) -> Result<SendReceipt, SendError> {
        if cancel.is_cancelled() {
            return Err(SendError::Cancelled);
        }

        let mut form = vec![
            ("from", self.from.clone()),
            ("to", message.to.clone()),
            ("subject", message.subject.clone()),
            ("html", message.html_body.clone()),
        ];
        if let Some(text) = &message.text_alt {
            form.push(("text", text.clone()));
        }

        let request = self
            .client
            .post(self.messages_url())
            .basic_auth("api", Some(&self.api_key))
            .form(&form)
            .send();

        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(SendError::Cancelled),
            res = request => res.map_err(|e| {
                // Timeouts/connection errors are worth retrying.
                SendError::Retryable(e.to_string())
            })?,
        };

        let status = resp.status();
        if status.is_success() {
            let parsed: MailgunSendResponse = resp
                .json()
                .await
                .map_err(|e| SendError::Retryable(format!("bad Mailgun response: {e}")))?;
            return Ok(SendReceipt {
                provider_message_id: parsed.id,
            });
        }

        let body = resp.text().await.unwrap_or_default();
        let detail = if body.is_empty() {
            status.to_string()
        } else {
            format!("{status}: {body}")
        };

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            Err(SendError::Retryable(detail))
        } else {
            Err(SendError::Fatal(detail))
        }
    }
}
