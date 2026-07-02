//! The project file: everything needed to reproduce a campaign, as human-readable TOML.
//! Secrets are NEVER stored here — accounts are referenced by id, secrets live in the OS keychain.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Suggested file name suffix: `my-campaign.mmproj.toml`
pub const PROJECT_SUFFIX: &str = ".mmproj.toml";

pub const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub version: u32,
    pub name: String,
    /// Set once a sending account has been chosen. Omitted from the file until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountRef>,
    pub template: TemplateSpec,
    /// Set once a recipient list has been imported. Omitted until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipients: Option<RecipientSource>,
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

/// The recently-opened/saved project list, persisted in the config directory
/// (`{config_dir}/massfckinmailer/recent.toml`), most-recent-first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RecentProjects {
    #[serde(default)]
    pub paths: Vec<String>,
}

impl RecentProjects {
    /// How many entries to keep.
    pub const MAX: usize = 10;

    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("massfckinmailer").join("recent.toml"))
    }

    /// Load the list; any error (missing file, parse failure) yields an empty list.
    pub fn load() -> Self {
        let Some(path) = Self::default_path() else {
            return Self::default();
        };
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), ProjectError> {
        let Some(path) = Self::default_path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Move `path` to the front, de-duplicating and capping at [`Self::MAX`].
    pub fn push(&mut self, path: &Path) {
        let entry = path.to_string_lossy().to_string();
        self.paths.retain(|p| p != &entry);
        self.paths.insert(0, entry);
        self.paths.truncate(Self::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Project {
        Project {
            version: CURRENT_VERSION,
            name: "Spring launch".into(),
            account: Some(AccountRef {
                id: "acct_9f3a".into(),
                display: "Mailgun — news.example.com".into(),
            }),
            template: TemplateSpec {
                subject: "Hey {{first_name}}!".into(),
                html_path: "template.html".into(),
                generate_text_alt: true,
            },
            recipients: Some(RecipientSource {
                source_path: "list.xlsx".into(),
                sheet: Some("Sheet1".into()),
                email_column: "E-mail".into(),
                mapping: [("first_name".to_string(), "First Name".to_string())]
                    .into_iter()
                    .collect(),
            }),
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
        assert_eq!(project.recipients.unwrap().mapping.len(), 0);
    }

    #[test]
    fn recent_projects_dedup_and_cap() {
        let mut recent = RecentProjects::default();
        for i in 0..12 {
            recent.push(Path::new(&format!("/p/{i}.mmproj.toml")));
        }
        assert_eq!(recent.paths.len(), RecentProjects::MAX);
        // Most-recent-first.
        assert_eq!(recent.paths[0], "/p/11.mmproj.toml");
        // Re-pushing an existing entry moves it to the front without duplicating.
        recent.push(Path::new("/p/5.mmproj.toml"));
        assert_eq!(recent.paths[0], "/p/5.mmproj.toml");
        assert_eq!(recent.paths.iter().filter(|p| *p == "/p/5.mmproj.toml").count(), 1);
    }

    #[test]
    fn partial_project_omits_optional_tables() {
        let project = Project {
            version: CURRENT_VERSION,
            name: "Draft".into(),
            account: None,
            template: TemplateSpec {
                subject: "Hi".into(),
                html_path: "draft.html".into(),
                generate_text_alt: true,
            },
            recipients: None,
            sending: SendingConfig::default(),
        };
        let text = toml::to_string_pretty(&project).unwrap();
        assert!(!text.contains("[account]"));
        assert!(!text.contains("[recipients]"));
        let parsed: Project = toml::from_str(&text).unwrap();
        assert_eq!(project, parsed);
    }
}
