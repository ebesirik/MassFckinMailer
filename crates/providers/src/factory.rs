//! Build a live [`EmailProvider`] from an [`Account`] plus its keychain secret.

use crate::account::{Account, AccountConfig};
use crate::gmail::GmailProvider;
use crate::mailgun::MailgunProvider;
use crate::outlook::OutlookProvider;
use crate::ses::SesProvider;
use crate::smtp::SmtpProvider;
use crate::{EmailProvider, ProviderError};

/// Construct the concrete provider for an account. `secret` is the value fetched
/// from the keychain (SMTP password or Mailgun API key).
pub fn build_provider(
    account: &Account,
    secret: String,
) -> Result<Box<dyn EmailProvider>, ProviderError> {
    match &account.config {
        AccountConfig::Smtp(config) => Ok(Box::new(SmtpProvider::new(config, secret)?)),
        AccountConfig::Mailgun(config) => Ok(Box::new(MailgunProvider::new(config, secret)?)),
        AccountConfig::Ses(config) => Ok(Box::new(SesProvider::new(config, secret)?)),
        AccountConfig::Gmail(config) => {
            Ok(Box::new(GmailProvider::new(&account.id, config, secret)?))
        }
        AccountConfig::Outlook(config) => {
            Ok(Box::new(OutlookProvider::new(&account.id, config, secret)?))
        }
    }
}
