//! Generic SMTP provider via `lettre` (async, rustls). Also covers Gmail/Outlook
//! app-password sending, which is plain SMTP under the hood.

use crate::account::{SmtpConfig, TlsMode};
use crate::{
    Capabilities, EmailProvider, ProviderError, ProviderKind, RenderedEmail, SendError, SendReceipt,
};
use async_trait::async_trait;
use lettre::message::{Mailbox, Message, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::{AsyncSmtpTransport, Error as SmtpError};
use lettre::{AsyncTransport, Tokio1Executor};
use tokio_util::sync::CancellationToken;

pub struct SmtpProvider {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl SmtpProvider {
    /// Build a connection-pooled transport from account config + password.
    pub fn new(config: &SmtpConfig, password: String) -> Result<Self, ProviderError> {
        let builder = match config.tls {
            TlsMode::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host)
                .map_err(|e| ProviderError::Config(e.to_string()))?,
            TlsMode::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host)
                .map_err(|e| ProviderError::Config(e.to_string()))?,
            TlsMode::None => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host),
        };

        let transport = builder
            .port(config.port)
            .credentials(Credentials::new(
                config.username.clone(),
                password,
            ))
            .build();

        let from = config
            .from
            .parse::<Mailbox>()
            .map_err(|e| ProviderError::Config(format!("invalid From address: {e}")))?;

        Ok(Self { transport, from })
    }

    fn build_message(&self, message: &RenderedEmail) -> Result<Message, SendError> {
        let to = message
            .to
            .parse::<Mailbox>()
            .map_err(|e| SendError::Fatal(format!("invalid recipient {}: {e}", message.to)))?;

        let builder = Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(message.subject.clone());

        let body = match &message.text_alt {
            Some(text) => builder.multipart(MultiPart::alternative_plain_html(
                text.clone(),
                message.html_body.clone(),
            )),
            None => builder.singlepart(SinglePart::html(message.html_body.clone())),
        };
        body.map_err(|e| SendError::Fatal(format!("failed to build message: {e}")))
    }
}

/// SMTP errors: permanent codes (auth, bad recipient) are fatal; everything
/// else (transient 4xx, timeouts, connection drops) is worth retrying.
fn classify(err: SmtpError) -> SendError {
    if err.is_permanent() {
        SendError::Fatal(err.to_string())
    } else {
        SendError::Retryable(err.to_string())
    }
}

#[async_trait]
impl EmailProvider for SmtpProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Smtp
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            suggested_rate_per_sec: 10.0,
            immediate_status: true,
        }
    }

    async fn verify(&self) -> Result<(), ProviderError> {
        match self.transport.test_connection().await {
            Ok(true) => Ok(()),
            Ok(false) => Err(ProviderError::Connection(
                "server did not accept the connection".into(),
            )),
            Err(e) if e.is_permanent() => Err(ProviderError::Auth(e.to_string())),
            Err(e) => Err(ProviderError::Connection(e.to_string())),
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
        let email = self.build_message(message)?;

        tokio::select! {
            _ = cancel.cancelled() => Err(SendError::Cancelled),
            res = self.transport.send(email) => match res {
                Ok(response) => Ok(SendReceipt {
                    provider_message_id: response
                        .message()
                        .next()
                        .map(|s| s.to_string()),
                }),
                Err(e) => Err(classify(e)),
            },
        }
    }
}
