//! The project file: everything needed to reproduce a campaign, as human-readable TOML.
//! Secrets are NEVER stored here — accounts are referenced by id, secrets live in the OS keychain.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Suggested file name suffix: `my-campaign.mmproj.toml`
pub const PROJECT_SUFFIX: &str = ".mmproj.toml";

pub const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub version: u32,
    pub name: String,
    pub account: AccountRef,
    pub template: TemplateSpec,
    pub recipients: RecipientSource,
    #[serde(default)]
    pub sending: SendingConfig,
}

/// Reference to a globally configured account. `display` is informational only,
/// so a project file shared with someone else reveals nothing sensitive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountRef {
    pub id: String,
    pub display: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemplateSpec {
    /// Subject line — also a template.
    pub subject: String,
    /// Path to the HTML body template, relative to the project file.
    pub html_path: String,
    /// Auto-generate a plain-text alternative part from the HTML.
    #[serde(default = "default_true")]
    pub generate_text_alt: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecipientSource {
    /// CSV or XLSX file, relative to the project file (or absolute).
    pub source_path: String,
    /// Sheet name (XLSX only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    /// Column holding the recipient email address.
    pub email_column: String,
    /// template field name -> file column name
    #[serde(default)]
    pub mapping: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendingConfig {
    /// Clamped to provider capability at send time.
    pub messages_per_second: f32,
    /// Max retries per message for retryable errors.
    pub retry_limit: u32,
    /// Abort after this many consecutive hard failures. 0 = never.
    pub stop_after_failures: u32,
}

impl Default for SendingConfig {
    fn default() -> Self {
        Self {
            messages_per_second: 5.0,
            retry_limit: 3,
            stop_after_failures: 25,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("failed to read project file: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid project file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("failed to serialize project: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("unsupported project version {found} (this app supports up to {supported})")]
    UnsupportedVersion { found: u32, supported: u32 },
}

impl Project {
    pub fn load(path: &Path) -> Result<Self, ProjectError> {
        let text = std::fs::read_to_string(path)?;
        let project: Project = toml::from_str(&text)?;
        if project.version > CURRENT_VERSION {
            return Err(ProjectError::UnsupportedVersion {
                found: project.version,
                supported: CURRENT_VERSION,
            });
        }
        Ok(project)
    }

    pub fn save(&self, path: &Path) -> Result<(), ProjectError> {
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Project {
        Project {
            version: CURRENT_VERSION,
            name: "Spring launch".into(),
            account: AccountRef {
                id: "acct_9f3a".into(),
                display: "Mailgun — news.example.com".into(),
            },
            template: TemplateSpec {
                subject: "Hey {{first_name}}!".into(),
                html_path: "template.html".into(),
                generate_text_alt: true,
            },
            recipients: RecipientSource {
                source_path: "list.xlsx".into(),
                sheet: Some("Sheet1".into()),
                email_column: "E-mail".into(),
                mapping: [("first_name".to_string(), "First Name".to_string())]
                    .into_iter()
                    .collect(),
            },
            sending: SendingConfig::default(),
        }
    }

    #[test]
    fn toml_round_trip() {
        let project = sample();
        let text = toml::to_string_pretty(&project).unwrap();
        let parsed: Project = toml::from_str(&text).unwrap();
        assert_eq!(project, parsed);
    }

    #[test]
    fn minimal_toml_parses_with_defaults() {
        let text = r#"
            version = 1
            name = "Test"

            [account]
            id = "a1"
            display = "SMTP"

            [template]
            subject = "Hi {{name}}"
            html_path = "t.html"

            [recipients]
            source_path = "list.csv"
            email_column = "email"
        "#;
        let project: Project = toml::from_str(text).unwrap();
        assert!(project.template.generate_text_alt);
        assert_eq!(project.sending.retry_limit, 3);
        assert!(project.recipients.mapping.is_empty());
    }
}
