//! AWS SES provider via `aws-sdk-sesv2` (`SendEmail`, simple content).
//!
//! Credentials (access key id + secret access key) are stored together in the
//! keychain as two lines; the region and From address are non-secret config.

use crate::account::SesConfig;
use crate::{
    Capabilities, EmailProvider, ProviderError, ProviderKind, RenderedEmail, SendError, SendReceipt,
};
use async_trait::async_trait;
use aws_sdk_sesv2::Client;
use aws_sdk_sesv2::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_sesv2::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_sesv2::types::{Body, Content, Destination, EmailContent, Message};
use tokio_util::sync::CancellationToken;

pub struct SesProvider {
    client: Client,
    from: String,
}

impl SesProvider {
    /// `secret` is the keychain value: access key id on the first line, secret
    /// access key on the second.
    pub fn new(config: &SesConfig, secret: String) -> Result<Self, ProviderError> {
        let (access_key_id, secret_access_key) = secret
            .split_once('\n')
            .map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
            .ok_or_else(|| {
                ProviderError::Config("SES credentials are missing or malformed.".into())
            })?;
        if access_key_id.is_empty() || secret_access_key.is_empty() {
            return Err(ProviderError::Config(
                "SES access key id and secret access key are both required.".into(),
            ));
        }

        let credentials = Credentials::new(
            access_key_id,
            secret_access_key,
            None,
            None,
            "massfckinmailer-ses",
        );
        let conf = aws_sdk_sesv2::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(config.region.clone()))
            .credentials_provider(credentials)
            .build();

        Ok(Self {
            client: Client::from_conf(conf),
            from: config.from.clone(),
        })
    }

    fn content(value: &str) -> Result<Content, SendError> {
        Content::builder()
            .data(value.to_string())
            .charset("UTF-8")
            .build()
            .map_err(|e| SendError::Fatal(e.to_string()))
    }
}

/// Build a helpful message from an SDK error, preferring the service-provided
/// code + message (this is where SES sandbox rejections show up).
fn err_message<E, R>(err: &SdkError<E, R>) -> String
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
    R: std::fmt::Debug,
{
    if let SdkError::ServiceError(svc) = err {
        let e = svc.err();
        match (e.code(), e.message()) {
            (Some(code), Some(msg)) => return format!("{code}: {msg}"),
            (_, Some(msg)) => return msg.to_string(),
            (Some(code), _) => return code.to_string(),
            _ => {}
        }
    }
    err.to_string()
}

fn classify_send<E, R>(err: SdkError<E, R>) -> SendError
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
    R: std::fmt::Debug,
{
    let message = err_message(&err);
    match &err {
        SdkError::TimeoutError(_) | SdkError::DispatchFailure(_) => SendError::Retryable(message),
        SdkError::ServiceError(svc) => {
            let code = svc.err().code().unwrap_or_default();
            let retryable = code.contains("Throttl")
                || code.contains("TooManyRequests")
                || code.contains("LimitExceeded");
            if retryable {
                SendError::Retryable(message)
            } else {
                SendError::Fatal(message)
            }
        }
        _ => SendError::Fatal(message),
    }
}

fn classify_verify<E, R>(err: SdkError<E, R>) -> ProviderError
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
    R: std::fmt::Debug,
{
    let message = err_message(&err);
    match &err {
        SdkError::ServiceError(_) => ProviderError::Auth(message),
        _ => ProviderError::Connection(message),
    }
}

#[async_trait]
impl EmailProvider for SesProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ses
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // SES accounts start around 14 msg/s; the engine clamps to this.
            suggested_rate_per_sec: 14.0,
            immediate_status: true,
        }
    }

    async fn verify(&self) -> Result<(), ProviderError> {
        // GetAccount is a cheap authenticated call. A sandbox account still
        // verifies fine; the sandbox restriction surfaces on send to an
        // unverified recipient.
        match self.client.get_account().send().await {
            Ok(_) => Ok(()),
            Err(e) => Err(classify_verify(e)),
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

        let subject = Self::content(&message.subject)?;
        let html = Self::content(&message.html_body)?;
        let mut body = Body::builder().html(html);
        if let Some(text) = &message.text_alt {
            body = body.text(Self::content(text)?);
        }
        let simple = Message::builder().subject(subject).body(body.build()).build();
        let content = EmailContent::builder().simple(simple).build();
        let destination = Destination::builder()
            .to_addresses(message.to.clone())
            .build();

        let request = self
            .client
            .send_email()
            .from_email_address(self.from.clone())
            .destination(destination)
            .content(content)
            .send();

        tokio::select! {
            _ = cancel.cancelled() => Err(SendError::Cancelled),
            res = request => match res {
                Ok(out) => Ok(SendReceipt {
                    provider_message_id: out.message_id().map(|s| s.to_string()),
                }),
                Err(e) => Err(classify_send(e)),
            },
        }
    }
}
