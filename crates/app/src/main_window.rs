use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, Theme, ThemeMode,
    TitleBar,
    button::{Button, ButtonVariants as _},
    divider::Divider,
    h_flex,
    input::{Input, InputState},
    popover::Popover,
    progress::Progress,
    switch::Switch,
    text::TextView,
    v_flex,
};

use crate::theme::{self, ActiveTokens as _};
use mmm_core::import::{self, RecipientTable, SourceKind, is_email};
use mmm_core::mapping::{self, RowStatus, ValidationReport};
use mmm_core::project::{
    AccountRef, CURRENT_VERSION, PROJECT_SUFFIX, Project, RecentProjects, RecipientSource,
    SendingConfig, TemplateSpec,
};
use mmm_core::settings::{AppSettings, ThemePref};
use mmm_core::template::{self, extract_placeholders, normalize_placeholders};
use rust_i18n::t;

/// Selectable UI languages: (locale code, short label for the picker).
const LANGUAGES: &[(&str, &str)] = &[
    ("en", "EN"),
    ("tr", "TR"),
    ("es", "ES"),
    ("de", "DE"),
    ("fr", "FR"),
    ("it", "IT"),
    ("pt", "PT"),
    ("ru", "RU"),
    ("zh", "中"),
    ("ja", "日"),
    ("nl", "NL"),
    ("pl", "PL"),
];

/// Translate a key with the current locale (no interpolation).
fn tr(key: &str) -> String {
    t!(key).into_owned()
}

/// App version + build, embedded at compile time by `build.rs` (see `MFM_VERSION`).
const APP_VERSION: &str = env!("MFM_VERSION");
use mmm_engine::{
    CampaignPlan, CampaignProgress, CampaignRecipient, CampaignState, CampaignSummary, Command,
    Event, MailRuntime, OutcomeStatus, RowOutcome, UpdateInfo,
};
use mmm_providers::{
    Account, AccountConfig, AccountStore, GmailConfig, MailgunConfig, MailgunRegion, OutlookConfig,
    ProviderKind, SesConfig, SmtpConfig, TlsMode, account::new_account_id, secrets,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Accounts,
    Template,
    Recipients,
    Send,
}

impl Section {
    const ALL: [Section; 4] = [
        Section::Accounts,
        Section::Template,
        Section::Recipients,
        Section::Send,
    ];

    fn label_key(&self) -> &'static str {
        match self {
            Self::Accounts => "nav.accounts",
            Self::Template => "nav.template",
            Self::Recipients => "nav.recipients",
            Self::Send => "nav.send",
        }
    }

    fn icon(&self) -> IconName {
        match self {
            Self::Accounts => IconName::CircleUser,
            Self::Template => IconName::File,
            Self::Recipients => IconName::Inbox,
            Self::Send => IconName::ArrowRight,
        }
    }

    fn hint_key(&self) -> &'static str {
        match self {
            Self::Accounts => "hint.accounts",
            Self::Template => "hint.template",
            Self::Recipients => "hint.recipients",
            Self::Send => "hint.send",
        }
    }
}

/// A transient status line under the account form (test result, save error, …).
#[derive(Debug, Clone)]
struct Notice {
    ok: bool,
    text: String,
}

/// Preset applied when re-parsing recipients for a loaded project: the saved
/// email column header and field→column mapping.
type Preset = (String, BTreeMap<String, String>);

/// A comparable snapshot of the campaign's persistable state. Dirty-tracking
/// compares the current snapshot to the last-saved one, avoiding fragile
/// change-event bookkeeping.
#[derive(Debug, Clone, PartialEq)]
struct ProjectSnapshot {
    subject: String,
    body: String,
    account: Option<String>,
    source_path: Option<String>,
    sheet: Option<String>,
    email_column: Option<String>,
    mapping: BTreeMap<String, String>,
    dedupe: bool,
    sending: SendingConfig,
}

impl ProjectSnapshot {
    /// Matches a brand-new, untouched campaign.
    fn fresh() -> Self {
        Self {
            subject: String::new(),
            body: String::new(),
            account: None,
            source_path: None,
            sheet: None,
            email_column: None,
            mapping: BTreeMap::new(),
            dedupe: true,
            sending: SendingConfig::default(),
        }
    }
}

/// A deferred action that must be confirmed if there are unsaved changes.
enum PendingAction {
    New,
    OpenDialog,
    OpenPath(PathBuf),
}

/// Active resume mode: only re-run the (non-`sent`) rows from a loaded outcome
/// report, matched to the current recipient list by original row index.
struct ResumeInfo {
    /// Original row indices to retry.
    indices: HashSet<usize>,
    failed: usize,
    skipped: usize,
    source: String,
}

// ---- Account form (M1) --------------------------------------------------

/// Text inputs and selections for the add-account form. The `InputState`
/// entities must be owned by the view so they stay alive across renders.
struct AccountForm {
    open: bool,
    /// `Some(id)` when editing an existing account in place (Save reuses the id).
    editing: Option<String>,
    kind: ProviderKind,
    tls: TlsMode,
    region: MailgunRegion,

    display: Entity<InputState>,
    smtp_host: Entity<InputState>,
    smtp_port: Entity<InputState>,
    smtp_username: Entity<InputState>,
    smtp_from: Entity<InputState>,
    smtp_password: Entity<InputState>,
    mg_domain: Entity<InputState>,
    mg_from: Entity<InputState>,
    mg_api_key: Entity<InputState>,
    ses_region: Entity<InputState>,
    ses_from: Entity<InputState>,
    ses_key_id: Entity<InputState>,
    ses_secret: Entity<InputState>,
    oauth_client_id: Entity<InputState>,
    oauth_client_secret: Entity<InputState>,
    oauth_tenant: Entity<InputState>,
    oauth_from: Entity<InputState>,
}

impl AccountForm {
    fn new(window: &mut Window, cx: &mut Context<MainWindow>) -> Self {
        let mk = |window: &mut Window,
                  cx: &mut Context<MainWindow>,
                  placeholder: &'static str,
                  masked: bool| {
            cx.new(|cx| {
                let state = InputState::new(window, cx).placeholder(placeholder);
                if masked { state.masked(true) } else { state }
            })
        };

        Self {
            open: false,
            editing: None,
            kind: ProviderKind::Smtp,
            tls: TlsMode::StartTls,
            region: MailgunRegion::Us,
            display: mk(window, cx, "e.g. Work SMTP", false),
            smtp_host: mk(window, cx, "smtp.example.com", false),
            smtp_port: mk(window, cx, "587", false),
            smtp_username: mk(window, cx, "you@example.com", false),
            smtp_from: mk(window, cx, "you@example.com", false),
            smtp_password: mk(window, cx, "password or app password", true),
            mg_domain: mk(window, cx, "news.example.com", false),
            mg_from: mk(window, cx, "hello@news.example.com", false),
            mg_api_key: mk(window, cx, "Mailgun private API key", true),
            ses_region: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("us-east-1")
                    .default_value("us-east-1")
            }),
            ses_from: mk(window, cx, "verified@example.com", false),
            ses_key_id: mk(window, cx, "AKIA… access key id", true),
            ses_secret: mk(window, cx, "secret access key", true),
            oauth_client_id: mk(window, cx, "OAuth client ID", false),
            oauth_client_secret: mk(window, cx, "client secret (Google desktop apps)", true),
            oauth_tenant: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("common")
                    .default_value("common")
            }),
            oauth_from: mk(window, cx, "you@gmail.com", false),
        }
    }
}

// ---- Template (minimal; the rich editor + preview arrive in M3) ----------

struct TemplateForm {
    subject: Entity<InputState>,
    body: Entity<InputState>,
}

impl TemplateForm {
    fn new(window: &mut Window, cx: &mut Context<MainWindow>) -> Self {
        let subject = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Hey {{first_name}}, {{product}} is live!")
        });
        let body = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("html")
                .line_number(true)
                .placeholder("<p>Hi {{first_name}},</p>")
        });
        Self { subject, body }
    }
}

// ---- Recipients (M2) ----------------------------------------------------

enum RecipientsState {
    Empty,
    Loading,
    Error(String),
    Loaded(Loaded),
}

/// A successfully imported list plus the user's mapping/validation choices.
struct Loaded {
    path: PathBuf,
    /// Non-empty for spreadsheets; drives the sheet picker.
    sheets: Vec<String>,
    sheet: Option<String>,
    table: Rc<RecipientTable>,
    email_col: usize,
    /// Snapshot of template placeholder names to map.
    fields: Vec<String>,
    /// template field -> file column header.
    mapping: BTreeMap<String, String>,
    dedupe: bool,
    report: Rc<ValidationReport>,
}

impl Loaded {
    fn recompute(&mut self) {
        let required: Vec<(String, usize)> = self
            .mapping
            .iter()
            .filter_map(|(field, col)| self.table.column_index(col).map(|idx| (field.clone(), idx)))
            .collect();
        self.report = Rc::new(mapping::validate(&self.table, self.email_col, &required));
    }

    fn sendable(&self) -> usize {
        if self.dedupe {
            self.report.sendable()
        } else {
            self.report.sendable_with_duplicates()
        }
    }
}

/// Result of the background parse, sent back to the UI thread.
struct ParseOutput {
    sheets: Vec<String>,
    sheet: Option<String>,
    table: RecipientTable,
}

pub struct MainWindow {
    active: Section,
    mail: MailRuntime,

    // M1 — accounts
    store: AccountStore,
    form: AccountForm,
    notice: Option<Notice>,
    testing: bool,
    /// Account awaiting OAuth authorization; saved once connected.
    pending_oauth: Option<Account>,

    // M2 — recipients + M3 template
    template: TemplateForm,
    recipients: RecipientsState,
    /// Which recipient row feeds the live template preview.
    preview_row: usize,

    // M4 — sending
    /// Selected sending account id (defaults to the first account).
    selected_account: Option<String>,
    sending: bool,
    progress: Option<CampaignProgress>,
    summary: Option<CampaignSummary>,
    send_notice: Option<Notice>,
    /// M7 — when set, only re-run these report rows.
    resume: Option<ResumeInfo>,
    // Sending settings (persisted per project) + test-send address.
    send_mps: Entity<InputState>,
    send_retry: Entity<InputState>,
    send_stop: Entity<InputState>,
    test_email: Entity<InputState>,

    // M5 — project files
    project_path: Option<PathBuf>,
    project_name: String,
    saved_snapshot: ProjectSnapshot,
    recents: RecentProjects,
    project_notice: Option<Notice>,

    /// Current UI language code (mirrors the global rust-i18n locale).
    language: String,
    /// Light / Dark / Auto (follow system).
    theme_pref: ThemePref,
    /// Title-bar language dropdown open state (controlled `Popover`).
    lang_menu_open: bool,
    /// Title-bar Open/recents dropdown open state (controlled `Popover`).
    open_menu_open: bool,
    /// Kept alive so system-appearance changes keep firing (Auto mode).
    _appearance_sub: Subscription,

    // Auto-update (OTA via GitHub)
    /// A newer release found for this build's channel, if any.
    available_update: Option<UpdateInfo>,
    /// True while an update is downloading / being applied.
    update_applying: bool,
    /// Last update error, shown next to the banner.
    update_error: Option<String>,
}

fn read_trimmed(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().trim().to_string()
}

/// Replace the text of an input field (used to pre-fill the edit form).
fn set_input(
    input: &Entity<InputState>,
    value: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) {
    input.update(cx, |state, cx| state.set_value(value, window, cx));
}

impl MainWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mail = MailRuntime::start();

        // Quietly check GitHub for a newer build on this channel at startup.
        mail.command(Command::CheckUpdate {
            current_version: APP_VERSION.to_string(),
        });

        // Event pump: await engine events on the foreground executor and
        // update this entity. flume's recv_async is runtime-agnostic, so no
        // tokio is needed on this side.
        let events = mail.events();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv_async().await {
                let alive = this
                    .update(cx, |this, cx| {
                        this.on_engine_event(event, cx);
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();

        let store = AccountStore::load().unwrap_or_default();
        let form = AccountForm::new(window, cx);
        let template = TemplateForm::new(window, cx);

        // Apply the saved theme before first paint, and follow the OS appearance
        // while in Auto mode.
        let settings = AppSettings::load();
        apply_theme_mode(settings.theme, window, cx);
        let appearance_sub = cx.observe_window_appearance(window, |this, window, cx| {
            if this.theme_pref == ThemePref::Auto {
                this.apply_theme(window, cx);
            }
        });

        let sc = SendingConfig::default();
        let send_mps = cx.new(|cx| {
            InputState::new(window, cx).default_value(sc.messages_per_second.to_string())
        });
        let send_retry =
            cx.new(|cx| InputState::new(window, cx).default_value(sc.retry_limit.to_string()));
        let send_stop = cx.new(|cx| {
            InputState::new(window, cx).default_value(sc.stop_after_failures.to_string())
        });
        let test_email = cx.new(|cx| InputState::new(window, cx).placeholder("you@example.com"));

        Self {
            active: Section::Accounts,
            mail,
            store,
            form,
            notice: None,
            testing: false,
            pending_oauth: None,
            template,
            recipients: RecipientsState::Empty,
            preview_row: 0,
            selected_account: None,
            sending: false,
            progress: None,
            summary: None,
            send_notice: None,
            resume: None,
            send_mps,
            send_retry,
            send_stop,
            test_email,
            project_path: None,
            project_name: tr("proj.untitled"),
            saved_snapshot: ProjectSnapshot::fresh(),
            recents: RecentProjects::load(),
            project_notice: None,
            language: settings.language,
            theme_pref: settings.theme,
            lang_menu_open: false,
            open_menu_open: false,
            _appearance_sub: appearance_sub,
            available_update: None,
            update_applying: false,
            update_error: None,
        }
    }

    fn on_apply_update(&mut self, cx: &mut Context<Self>) {
        let Some(info) = self.available_update.clone() else {
            return;
        };
        self.update_applying = true;
        self.update_error = None;
        self.mail.command(Command::ApplyUpdate(Box::new(info)));
        cx.notify();
    }

    fn apply_theme(&self, window: &mut Window, cx: &mut App) {
        apply_theme_mode(self.theme_pref, window, cx);
    }

    fn save_settings(&self) {
        let _ = AppSettings {
            language: self.language.clone(),
            theme: self.theme_pref,
        }
        .save();
    }

    fn on_set_theme(&mut self, pref: ThemePref, window: &mut Window, cx: &mut Context<Self>) {
        self.theme_pref = pref;
        apply_theme_mode(pref, window, cx);
        self.save_settings();
        cx.notify();
    }

    fn on_set_language(&mut self, code: &str, cx: &mut Context<Self>) {
        self.language = code.to_string();
        rust_i18n::set_locale(code);
        self.save_settings();
        cx.notify();
    }

    /// Whether the template has any content worth sending.
    fn has_template(&self, cx: &App) -> bool {
        !self.template.subject.read(cx).value().trim().is_empty()
            || !self.template.body.read(cx).value().trim().is_empty()
    }

    /// All prerequisites are met to start a campaign.
    fn is_ready_to_send(&self, cx: &App) -> bool {
        !self.store.accounts.is_empty() && self.has_template(cx) && self.sendable_count() > 0
    }

    /// Completion glyph for a nav step: green check when done, amber alert when
    /// the step needs attention, or nothing when not yet started.
    fn step_status(&self, section: Section, cx: &App) -> Option<(IconName, Hsla)> {
        let done = (IconName::CircleCheck, cx.theme().success);
        let attention = (IconName::TriangleAlert, cx.theme().warning);
        match section {
            Section::Accounts => (!self.store.accounts.is_empty()).then_some(done),
            Section::Template => self.has_template(cx).then_some(done),
            Section::Recipients => match &self.recipients {
                RecipientsState::Loaded(l) => {
                    Some(if l.report.valid > 0 { done } else { attention })
                }
                _ => None,
            },
            Section::Send => self.is_ready_to_send(cx).then_some(done),
        }
    }

    fn on_engine_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::TestResult { ok, message, .. } => {
                self.testing = false;
                // Success is localized; failures keep the provider's detail.
                let text = if ok { tr("acct.test_ok") } else { message };
                self.notice = Some(Notice { ok, text });
            }
            Event::OAuthConnected {
                account_id,
                ok,
                message,
            } => {
                self.testing = false;
                if ok {
                    if let Some(account) = self.pending_oauth.take()
                        && account.id == account_id
                    {
                        self.store.upsert(account);
                        if let Err(e) = self.store.save() {
                            self.notice = Some(Notice {
                                ok: false,
                                text: format!("Connected, but saving the account failed: {e}"),
                            });
                            return;
                        }
                        self.form.open = false;
                        self.form.editing = None;
                    }
                    let _ = message;
                    self.notice = Some(Notice {
                        ok: true,
                        text: tr("acct.connect_ok"),
                    });
                } else {
                    self.pending_oauth = None;
                    self.notice = Some(Notice {
                        ok: false,
                        text: message,
                    });
                }
            }
            Event::CampaignProgress(progress) => {
                self.progress = Some(progress);
            }
            Event::CampaignFinished { summary } => {
                self.summary = Some(summary);
                self.sending = false;
            }
            Event::ReportExported { ok, message } => {
                self.send_notice = Some(Notice { ok, text: message });
            }
            Event::UpdateAvailable(info) => {
                self.available_update = Some(*info);
                self.update_error = None;
            }
            // Silent on startup: being up to date or offline needs no fanfare.
            Event::UpdateNotAvailable | Event::UpdateCheckFailed { .. } => {}
            Event::UpdateApplied => {
                // A new/updated process is starting (or the installer is running);
                // step aside so it can take over.
                cx.quit();
            }
            Event::UpdateFailed { message } => {
                self.update_applying = false;
                self.update_error = Some(message);
            }
        }
    }

    // ---- Account form logic (M1) ----------------------------------------

    fn form_to_account(&self, id: String, cx: &App) -> Result<(Account, String), String> {
        let mut display = read_trimmed(&self.form.display, cx);

        match self.form.kind {
            ProviderKind::Smtp => {
                let host = read_trimmed(&self.form.smtp_host, cx);
                let port = read_trimmed(&self.form.smtp_port, cx);
                let username = read_trimmed(&self.form.smtp_username, cx);
                let from = read_trimmed(&self.form.smtp_from, cx);
                let password = self.form.smtp_password.read(cx).value().to_string();

                if host.is_empty() {
                    return Err(tr("err.smtp_host"));
                }
                let port: u16 = port.parse().map_err(|_| tr("err.port"))?;
                if from.is_empty() {
                    return Err(tr("err.from"));
                }
                if password.is_empty() {
                    return Err(tr("err.password"));
                }
                if display.is_empty() {
                    display = format!("SMTP — {host}");
                }

                Ok((
                    Account {
                        id,
                        display,
                        config: AccountConfig::Smtp(SmtpConfig {
                            host,
                            port,
                            tls: self.form.tls,
                            username,
                            from,
                        }),
                    },
                    password,
                ))
            }
            ProviderKind::Mailgun => {
                let domain = read_trimmed(&self.form.mg_domain, cx);
                let from = read_trimmed(&self.form.mg_from, cx);
                let api_key = self.form.mg_api_key.read(cx).value().to_string();

                if domain.is_empty() {
                    return Err(tr("err.mg_domain"));
                }
                if from.is_empty() {
                    return Err(tr("err.from"));
                }
                if api_key.is_empty() {
                    return Err(tr("err.mg_api_key"));
                }
                if display.is_empty() {
                    display = format!("Mailgun — {domain}");
                }

                Ok((
                    Account {
                        id,
                        display,
                        config: AccountConfig::Mailgun(MailgunConfig {
                            domain,
                            region: self.form.region,
                            from,
                        }),
                    },
                    api_key,
                ))
            }
            ProviderKind::Ses => {
                let region = read_trimmed(&self.form.ses_region, cx);
                let from = read_trimmed(&self.form.ses_from, cx);
                let key_id = self.form.ses_key_id.read(cx).value().trim().to_string();
                let secret = self.form.ses_secret.read(cx).value().trim().to_string();

                if region.is_empty() {
                    return Err(tr("err.region"));
                }
                if from.is_empty() {
                    return Err(tr("err.ses_from"));
                }
                if key_id.is_empty() || secret.is_empty() {
                    return Err(tr("err.ses_creds"));
                }
                if display.is_empty() {
                    display = format!("SES — {region}");
                }

                // Credentials are stored together in the keychain as two lines.
                Ok((
                    Account {
                        id,
                        display,
                        config: AccountConfig::Ses(SesConfig { region, from }),
                    },
                    format!("{key_id}\n{secret}"),
                ))
            }
            ProviderKind::Gmail => {
                let client_id = read_trimmed(&self.form.oauth_client_id, cx);
                let client_secret = self
                    .form
                    .oauth_client_secret
                    .read(cx)
                    .value()
                    .trim()
                    .to_string();
                let from = read_trimmed(&self.form.oauth_from, cx);
                if client_id.is_empty() {
                    return Err(tr("err.client_id"));
                }
                if from.is_empty() {
                    return Err(tr("err.gmail_from"));
                }
                if display.is_empty() {
                    display = format!("Gmail — {from}");
                }
                // For OAuth accounts the returned secret is the client secret,
                // consumed by the Connect flow (tokens are stored afterwards).
                Ok((
                    Account {
                        id,
                        display,
                        config: AccountConfig::Gmail(GmailConfig { client_id, from }),
                    },
                    client_secret,
                ))
            }
            ProviderKind::Outlook => {
                let client_id = read_trimmed(&self.form.oauth_client_id, cx);
                let tenant = read_trimmed(&self.form.oauth_tenant, cx);
                let from = read_trimmed(&self.form.oauth_from, cx);
                if client_id.is_empty() {
                    return Err(tr("err.app_client_id"));
                }
                if from.is_empty() {
                    return Err(tr("err.oauth_from"));
                }
                if display.is_empty() {
                    display = format!("Outlook — {from}");
                }
                let tenant = if tenant.is_empty() {
                    "common".to_string()
                } else {
                    tenant
                };
                Ok((
                    Account {
                        id,
                        display,
                        config: AccountConfig::Outlook(OutlookConfig {
                            client_id,
                            tenant,
                            from,
                        }),
                    },
                    // Azure public client + PKCE: no client secret.
                    String::new(),
                ))
            }
        }
    }

    fn on_test_connection(&mut self, cx: &mut Context<Self>) {
        match self.form_to_account("probe".into(), cx) {
            Ok((account, secret)) => {
                self.testing = true;
                self.notice = Some(Notice {
                    ok: true,
                    text: tr("acct.testing"),
                });
                self.mail.command(Command::TestAccount { account, secret });
            }
            Err(text) => self.notice = Some(Notice { ok: false, text }),
        }
        cx.notify();
    }

    fn on_connect_oauth(&mut self, cx: &mut Context<Self>) {
        let id = self.form.editing.clone().unwrap_or_else(new_account_id);
        match self.form_to_account(id, cx) {
            Ok((account, client_secret)) => {
                self.pending_oauth = Some(account.clone());
                self.testing = true;
                self.notice = Some(Notice {
                    ok: true,
                    text: tr("acct.connecting"),
                });
                self.mail.command(Command::ConnectOAuth {
                    account,
                    client_secret,
                });
            }
            Err(text) => self.notice = Some(Notice { ok: false, text }),
        }
        cx.notify();
    }

    fn on_save_account(&mut self, cx: &mut Context<Self>) {
        let id = self.form.editing.clone().unwrap_or_else(new_account_id);
        match self.form_to_account(id.clone(), cx) {
            Ok((account, secret)) => {
                if let Err(e) = secrets::set(&account.id, &secret) {
                    self.notice = Some(Notice {
                        ok: false,
                        text: format!("{}: {e}", tr("acct.keychain_error")),
                    });
                    cx.notify();
                    return;
                }
                self.store.upsert(account);
                if let Err(e) = self.store.save() {
                    self.notice = Some(Notice {
                        ok: false,
                        text: format!("{}: {e}", tr("acct.file_error")),
                    });
                    cx.notify();
                    return;
                }
                self.form.open = false;
                self.form.editing = None;
                self.notice = Some(Notice {
                    ok: true,
                    text: tr("acct.saved"),
                });
            }
            Err(text) => self.notice = Some(Notice { ok: false, text }),
        }
        cx.notify();
    }

    fn on_delete_account(&mut self, id: String, cx: &mut Context<Self>) {
        let _ = secrets::delete(&id);
        self.store.remove(&id);
        if let Err(e) = self.store.save() {
            self.notice = Some(Notice {
                ok: false,
                text: format!("Failed to update accounts file: {e}"),
            });
        }
        cx.notify();
    }

    /// Open the form pre-filled from an existing account to edit it in place.
    /// Secrets aren't read back from the keychain, so they're re-entered on save.
    fn begin_edit(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(account) = self.store.get(&id).cloned() else {
            return;
        };
        self.form.open = true;
        self.form.editing = Some(account.id.clone());
        self.notice = None;
        set_input(&self.form.display, account.display.clone(), window, cx);
        match &account.config {
            AccountConfig::Smtp(c) => {
                self.form.kind = ProviderKind::Smtp;
                self.form.tls = c.tls;
                set_input(&self.form.smtp_host, c.host.clone(), window, cx);
                set_input(&self.form.smtp_port, c.port.to_string(), window, cx);
                set_input(&self.form.smtp_username, c.username.clone(), window, cx);
                set_input(&self.form.smtp_from, c.from.clone(), window, cx);
                set_input(&self.form.smtp_password, "", window, cx);
            }
            AccountConfig::Mailgun(c) => {
                self.form.kind = ProviderKind::Mailgun;
                self.form.region = c.region;
                set_input(&self.form.mg_domain, c.domain.clone(), window, cx);
                set_input(&self.form.mg_from, c.from.clone(), window, cx);
                set_input(&self.form.mg_api_key, "", window, cx);
            }
            AccountConfig::Ses(c) => {
                self.form.kind = ProviderKind::Ses;
                set_input(&self.form.ses_region, c.region.clone(), window, cx);
                set_input(&self.form.ses_from, c.from.clone(), window, cx);
                set_input(&self.form.ses_key_id, "", window, cx);
                set_input(&self.form.ses_secret, "", window, cx);
            }
            AccountConfig::Gmail(c) => {
                self.form.kind = ProviderKind::Gmail;
                set_input(&self.form.oauth_from, c.from.clone(), window, cx);
                set_input(&self.form.oauth_client_id, c.client_id.clone(), window, cx);
                set_input(&self.form.oauth_client_secret, "", window, cx);
            }
            AccountConfig::Outlook(c) => {
                self.form.kind = ProviderKind::Outlook;
                set_input(&self.form.oauth_from, c.from.clone(), window, cx);
                set_input(&self.form.oauth_client_id, c.client_id.clone(), window, cx);
                set_input(&self.form.oauth_tenant, c.tenant.clone(), window, cx);
            }
        }
        cx.notify();
    }

    // ---- Recipients logic (M2) ------------------------------------------

    /// Placeholder field names from the current subject + body.
    fn template_fields(&self, cx: &App) -> Vec<String> {
        let subject = self.template.subject.read(cx).value();
        let body = self.template.body.read(cx).value();
        let combined = format!(
            "{}\n{}",
            normalize_placeholders(&subject),
            normalize_placeholders(&body)
        );
        extract_placeholders(&combined)
    }

    fn on_choose_file(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr("rcpt.file_prompt").into()),
        });
        cx.spawn(async move |this, cx| {
            let selected = match paths.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            if let Some(path) = selected {
                let _ = this.update(cx, |this, cx| this.spawn_parse(path, None, None, false, cx));
            }
        })
        .detach();
    }

    fn on_select_sheet(&mut self, name: String, cx: &mut Context<Self>) {
        let path = match &self.recipients {
            RecipientsState::Loaded(l) => l.path.clone(),
            _ => return,
        };
        self.spawn_parse(path, Some(name), None, false, cx);
    }

    /// Parse `path` on the background executor, then apply the result. `preset`
    /// (email column + mapping) is supplied when loading a saved project;
    /// `from_load` marks the resulting state as the saved baseline.
    fn spawn_parse(
        &mut self,
        path: PathBuf,
        forced_sheet: Option<String>,
        preset: Option<Preset>,
        from_load: bool,
        cx: &mut Context<Self>,
    ) {
        self.recipients = RecipientsState::Loading;
        cx.notify();

        let parse_path = path.clone();
        cx.spawn(async move |this, cx| {
            let output: Result<ParseOutput, String> = cx
                .background_executor()
                .spawn(async move {
                    match import::source_kind(&parse_path) {
                        Some(SourceKind::Csv) => {
                            let table =
                                import::parse_file(&parse_path, None).map_err(|e| e.to_string())?;
                            Ok(ParseOutput {
                                sheets: Vec::new(),
                                sheet: None,
                                table,
                            })
                        }
                        Some(SourceKind::Excel) => {
                            let sheets = import::excel_sheet_names(&parse_path)
                                .map_err(|e| e.to_string())?;
                            let sheet = forced_sheet.or_else(|| sheets.first().cloned());
                            let table = import::parse_excel(&parse_path, sheet.as_deref())
                                .map_err(|e| e.to_string())?;
                            Ok(ParseOutput {
                                sheets,
                                sheet,
                                table,
                            })
                        }
                        None => Err("Unsupported file type — choose a CSV or Excel file.".into()),
                    }
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.on_parsed(path, output, preset, from_load, cx)
            });
        })
        .detach();
    }

    fn on_parsed(
        &mut self,
        path: PathBuf,
        output: Result<ParseOutput, String>,
        preset: Option<Preset>,
        from_load: bool,
        cx: &mut Context<Self>,
    ) {
        match output {
            Err(e) => self.recipients = RecipientsState::Error(e),
            Ok(output) => {
                let table = Rc::new(output.table);
                let fields = self.template_fields(cx);
                let (email_col, mapping) = match &preset {
                    Some((email_column, mapping)) => {
                        let col = table
                            .column_index(email_column)
                            .unwrap_or_else(|| import::detect_email_column_in(&table).unwrap_or(0));
                        // The saved mapping wins, but auto-map any template field it
                        // doesn't already cover — e.g. projects saved before a
                        // placeholder existed, whose mapping is empty or stale.
                        let mut merged = mapping::auto_map(&fields, &table.headers);
                        merged.extend(mapping.iter().map(|(f, c)| (f.clone(), c.clone())));
                        (col, merged)
                    }
                    None => (
                        import::detect_email_column_in(&table).unwrap_or(0),
                        mapping::auto_map(&fields, &table.headers),
                    ),
                };
                let mut loaded = Loaded {
                    path,
                    sheets: output.sheets,
                    sheet: output.sheet,
                    table,
                    email_col,
                    fields,
                    mapping,
                    dedupe: true,
                    report: Rc::new(ValidationReport::default()),
                };
                loaded.recompute();
                self.recipients = RecipientsState::Loaded(loaded);
                self.preview_row = 0;
                if from_load {
                    self.mark_saved(cx);
                }
            }
        }
        cx.notify();
    }

    fn with_loaded(&mut self, f: impl FnOnce(&mut Loaded)) {
        if let RecipientsState::Loaded(loaded) = &mut self.recipients {
            f(loaded);
        }
    }

    fn set_email_col(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.with_loaded(|l| {
            l.email_col = idx;
            l.recompute();
        });
        cx.notify();
    }

    fn set_mapping(&mut self, field: String, column: Option<String>, cx: &mut Context<Self>) {
        self.with_loaded(|l| {
            match column {
                Some(col) => {
                    l.mapping.insert(field, col);
                }
                None => {
                    l.mapping.remove(&field);
                }
            }
            l.recompute();
        });
        cx.notify();
    }

    fn toggle_dedupe(&mut self, cx: &mut Context<Self>) {
        self.with_loaded(|l| l.dedupe = !l.dedupe);
        cx.notify();
    }

    fn refresh_fields(&mut self, cx: &mut Context<Self>) {
        let fields = self.template_fields(cx);
        self.with_loaded(|l| {
            // Auto-map afresh, but keep any manual mappings for fields that remain.
            let mut merged = mapping::auto_map(&fields, &l.table.headers);
            for (field, col) in &l.mapping {
                if fields.contains(field) {
                    merged.insert(field.clone(), col.clone());
                }
            }
            l.fields = fields;
            l.mapping = merged;
            l.recompute();
        });
        cx.notify();
    }

    // ---- Rendering ------------------------------------------------------

    /// A subtle "update available" card in the sidebar; empty when there's
    /// nothing to offer. Notify-and-click — never auto-applies.
    fn render_update_banner(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(info) = &self.available_update else {
            return div();
        };
        let label = if self.update_applying {
            tr("update.downloading")
        } else {
            tr("update.restart")
        };
        v_flex()
            .m_2()
            .p_2()
            .gap_1()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().primary)
            .bg(cx.theme().accent)
            .child(div().text_xs().child(tr("update.available")))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("v{}", info.version)),
            )
            .child(
                Button::new("update-apply")
                    .primary()
                    .w_full()
                    .label(label)
                    .disabled(self.update_applying)
                    .on_click(cx.listener(|this, _, _, cx| this.on_apply_update(cx))),
            )
            .children(self.update_error.as_ref().map(|err| {
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(err.clone())
            }))
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let faint = cx.tokens().text_faint;
        v_flex()
            .w(px(264.))
            .flex_shrink_0()
            .h_full()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .px(px(14.))
            .py(px(20.))
            .gap(px(22.))
            // Identity block.
            .child(
                h_flex()
                    .items_center()
                    .gap(px(12.))
                    .child(self.app_mark(px(42.), px(20.), px(12.), cx))
                    .child(
                        v_flex()
                            .gap(px(2.))
                            .overflow_hidden()
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("MassFckinMailer"),
                            )
                            .child(
                                h_flex()
                                    .gap(px(4.))
                                    .text_size(px(12.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(tr("nav.subtitle"))
                                    .child(
                                        div()
                                            .font_family(theme::MONO_FONT)
                                            .text_size(px(11.))
                                            .text_color(faint)
                                            .child(format!("· v{APP_VERSION}")),
                                    ),
                            ),
                    ),
            )
            // Steps.
            .child(
                v_flex()
                    .gap(px(3.))
                    .child(
                        div()
                            .px(px(8.))
                            .pb(px(4.))
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(faint)
                            .child(tr("nav.steps").to_uppercase()),
                    )
                    .children(Section::ALL.map(|section| self.step_row(section, cx))),
            )
            // Push the update banner to the bottom.
            .child(div().flex_1())
            .child(self.render_update_banner(cx))
    }

    /// One wizard step row: leading icon + label (flex) + trailing status check.
    fn step_row(&self, section: Section, cx: &mut Context<Self>) -> AnyElement {
        let active = self.active == section;
        let status = self.step_status(section, cx);
        let accent = cx.theme().primary;
        let soft = cx.tokens().accent_soft;
        let surface_2 = cx.tokens().surface_2;
        let text = cx.theme().foreground;

        h_flex()
            .id(SharedString::from(format!("step-{}", section.label_key())))
            .items_center()
            .gap(px(11.))
            .px(px(11.))
            .py(px(9.))
            .rounded(px(10.))
            .cursor_pointer()
            .when(active, |this| this.bg(soft).text_color(accent))
            .when(!active, |this| {
                this.text_color(text).hover(|h| h.bg(surface_2))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.active = section;
                cx.notify();
            }))
            .child(Icon::new(section.icon()).with_size(px(18.)))
            .child(
                div()
                    .flex_1()
                    .text_size(px(14.))
                    .when(active, |this| this.font_weight(FontWeight::SEMIBOLD))
                    .child(tr(section.label_key())),
            )
            .when_some(status, |this, (icon, color)| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_color(color)
                        .child(Icon::new(icon).with_size(px(15.))),
                )
            })
            .into_any_element()
    }

    fn render_content(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let section = self.active;
        div()
            .id("content-scroll")
            .flex_1()
            .h_full()
            .min_w(px(0.))
            .overflow_y_scroll()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(880.))
                    .mx_auto()
                    .px(px(44.))
                    .pt(px(38.))
                    .pb(px(72.))
                    .gap(px(24.))
                    .child(
                        v_flex()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_size(px(26.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(tr(section.label_key())),
                            )
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .text_color(cx.theme().muted_foreground)
                                    .max_w(px(640.))
                                    .child(tr(section.hint_key())),
                            ),
                    )
                    .when(section == Section::Accounts, |this| {
                        this.child(self.render_accounts(cx))
                    })
                    .when(section == Section::Template, |this| {
                        this.child(self.render_template(window, cx))
                    })
                    .when(section == Section::Recipients, |this| {
                        this.child(self.render_recipients(cx))
                    })
                    .when(section == Section::Send, |this| {
                        this.child(self.render_send(cx))
                    }),
            )
    }

    // ---- Accounts UI ----------------------------------------------------

    fn render_accounts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = cx.theme().primary;
        v_flex()
            .gap(px(12.))
            .child(self.render_account_list(cx))
            .when(!self.form.open, |this| {
                this.child(
                    div()
                        .id("add-account")
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(px(8.))
                        .w_full()
                        .py(px(14.))
                        .rounded(px(12.))
                        .border_1()
                        .border_dashed()
                        .border_color(cx.tokens().border_strong)
                        .text_color(cx.theme().foreground)
                        .cursor_pointer()
                        .hover(move |h| h.border_color(accent).text_color(accent))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.form.editing = None;
                            this.form.open = true;
                            this.notice = None;
                            cx.notify();
                        }))
                        .child(Icon::new(IconName::Plus).with_size(px(16.)))
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(tr("acct.add")),
                        ),
                )
            })
            .when(self.form.open, |this| {
                this.child(self.render_account_form(cx))
            })
    }

    fn render_account_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.store.accounts.is_empty() {
            return v_flex().child(
                div()
                    .text_size(px(14.))
                    .text_color(cx.theme().muted_foreground)
                    .child(tr("acct.empty")),
            );
        }

        let rows = self.store.accounts.iter().map(|account| {
            let id = account.id.clone();
            let id_edit = id.clone();
            let id_del = id.clone();
            let detail = match &account.config {
                AccountConfig::Smtp(c) => format!("SMTP · {}:{}", c.host, c.port),
                AccountConfig::Mailgun(c) => format!("Mailgun · {}", c.domain),
                AccountConfig::Ses(c) => format!("AWS SES · {}", c.region),
                AccountConfig::Gmail(c) => format!("Gmail · {}", c.from),
                AccountConfig::Outlook(c) => format!("Outlook · {}", c.from),
            };
            card(cx)
                .flex()
                .items_center()
                .gap(px(14.))
                .px(px(18.))
                .py(px(16.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(42.))
                        .flex_shrink_0()
                        .rounded(px(11.))
                        .bg(cx.tokens().accent_soft)
                        .text_color(cx.theme().primary)
                        .child(Icon::empty().path("icons/mail.svg").with_size(px(20.))),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(account.display.clone()),
                        )
                        .child(
                            div()
                                .font_family(theme::MONO_FONT)
                                .text_size(px(12.5))
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(detail),
                        ),
                )
                .child(
                    h_flex()
                        .gap(px(5.))
                        .flex_shrink_0()
                        .child(
                            icon_btn(
                                SharedString::from(format!("edit-{id}")),
                                "icons/pencil.svg",
                                false,
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.begin_edit(id_edit.clone(), window, cx)
                                },
                            )),
                        )
                        .child(
                            icon_btn(
                                SharedString::from(format!("del-{id}")),
                                "icons/trash.svg",
                                true,
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.on_delete_account(id_del.clone(), cx),
                            )),
                        ),
                )
        });

        v_flex().gap(px(12.)).children(rows)
    }

    fn render_account_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let kind = self.form.kind;
        let is_oauth = matches!(kind, ProviderKind::Gmail | ProviderKind::Outlook);

        let editing = self.form.editing.is_some();
        card(cx)
            .flex()
            .flex_col()
            .gap(px(18.))
            .p(px(22.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if editing {
                                tr("acct.edit_title")
                            } else {
                                tr("acct.new")
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("close-form")
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.form.open = false;
                                this.form.editing = None;
                                this.notice = None;
                                this.pending_oauth = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(field_label("acct.provider", cx))
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .child(kind_button(
                                "k-smtp",
                                "provider.smtp",
                                ProviderKind::Smtp,
                                kind,
                                cx,
                            ))
                            .child(kind_button(
                                "k-mailgun",
                                "provider.mailgun",
                                ProviderKind::Mailgun,
                                kind,
                                cx,
                            ))
                            .child(kind_button(
                                "k-ses",
                                "provider.ses",
                                ProviderKind::Ses,
                                kind,
                                cx,
                            ))
                            .child(kind_button(
                                "k-gmail",
                                "provider.gmail",
                                ProviderKind::Gmail,
                                kind,
                                cx,
                            ))
                            .child(kind_button(
                                "k-outlook",
                                "provider.outlook",
                                ProviderKind::Outlook,
                                kind,
                                cx,
                            )),
                    ),
            )
            .child(labeled("acct.display", &self.form.display, cx))
            .when(kind == ProviderKind::Smtp, |this| {
                this.child(self.render_smtp_fields(cx))
            })
            .when(kind == ProviderKind::Mailgun, |this| {
                this.child(self.render_mailgun_fields(cx))
            })
            .when(kind == ProviderKind::Ses, |this| {
                this.child(self.render_ses_fields(cx))
            })
            .when(kind == ProviderKind::Gmail, |this| {
                this.child(self.render_gmail_fields(cx))
            })
            .when(kind == ProviderKind::Outlook, |this| {
                this.child(self.render_outlook_fields(cx))
            })
            .when_some(self.notice.clone(), |this, notice| {
                let color = if notice.ok {
                    cx.theme().success
                } else {
                    cx.theme().danger
                };
                this.child(div().text_sm().text_color(color).child(notice.text))
            })
            .child(
                h_flex()
                    .gap_2()
                    .when(!is_oauth, |this| {
                        this.child(
                            Button::new("test-conn")
                                .outline()
                                .label(tr("acct.test"))
                                .disabled(self.testing)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.on_test_connection(cx)),
                                ),
                        )
                        .child(
                            Button::new("save-account")
                                .primary()
                                .label(tr("acct.save"))
                                .disabled(self.testing)
                                .on_click(cx.listener(|this, _, _, cx| this.on_save_account(cx))),
                        )
                    })
                    .when(is_oauth, |this| {
                        this.child(
                            Button::new("connect-oauth")
                                .primary()
                                .label(tr("acct.connect"))
                                .disabled(self.testing)
                                .on_click(cx.listener(|this, _, _, cx| this.on_connect_oauth(cx))),
                        )
                    })
                    .child(
                        Button::new("cancel-account")
                            .ghost()
                            .label(tr("common.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.form.open = false;
                                this.form.editing = None;
                                this.notice = None;
                                this.pending_oauth = None;
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_smtp_fields(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tls = self.form.tls;
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .child(labeled("acct.host", &self.form.smtp_host, cx)),
                    )
                    .child(
                        div()
                            .w(px(120.))
                            .child(labeled("acct.port", &self.form.smtp_port, cx)),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(field_label("acct.encryption", cx))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(tls_button(
                                "tls-starttls",
                                "tls.starttls",
                                TlsMode::StartTls,
                                tls,
                                cx,
                            ))
                            .child(tls_button("tls-tls", "tls.tls", TlsMode::Tls, tls, cx))
                            .child(tls_button("tls-none", "tls.none", TlsMode::None, tls, cx)),
                    ),
            )
            .child(labeled("acct.username", &self.form.smtp_username, cx))
            .child(labeled("acct.from", &self.form.smtp_from, cx))
            .child(labeled("acct.password", &self.form.smtp_password, cx))
    }

    fn render_mailgun_fields(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let region = self.form.region;
        v_flex()
            .gap_3()
            .child(labeled("acct.mg_domain", &self.form.mg_domain, cx))
            .child(
                v_flex()
                    .gap_1()
                    .child(field_label("acct.region", cx))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(region_button(
                                "rg-us",
                                "region.us",
                                MailgunRegion::Us,
                                region,
                                cx,
                            ))
                            .child(region_button(
                                "rg-eu",
                                "region.eu",
                                MailgunRegion::Eu,
                                region,
                                cx,
                            )),
                    ),
            )
            .child(labeled("acct.from", &self.form.mg_from, cx))
            .child(labeled("acct.api_key", &self.form.mg_api_key, cx))
    }

    fn render_ses_fields(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_3()
                    .child(div().w(px(160.)).child(labeled(
                        "acct.region",
                        &self.form.ses_region,
                        cx,
                    )))
                    .child(
                        div()
                            .flex_1()
                            .child(labeled("acct.from", &self.form.ses_from, cx)),
                    ),
            )
            .child(labeled("acct.ses_key_id", &self.form.ses_key_id, cx))
            .child(labeled("acct.ses_secret", &self.form.ses_secret, cx))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(tr("acct.ses_note")),
            )
    }

    fn render_gmail_fields(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .child(labeled("acct.gmail_from", &self.form.oauth_from, cx))
            .child(labeled("acct.client_id", &self.form.oauth_client_id, cx))
            .child(labeled(
                "acct.client_secret",
                &self.form.oauth_client_secret,
                cx,
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(tr("acct.gmail_note")),
            )
    }

    fn render_outlook_fields(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .child(labeled("acct.oauth_from", &self.form.oauth_from, cx))
            .child(labeled(
                "acct.app_client_id",
                &self.form.oauth_client_id,
                cx,
            ))
            .child(labeled("acct.tenant", &self.form.oauth_tenant, cx))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(tr("acct.outlook_note")),
            )
    }

    // ---- Template UI (M3) -----------------------------------------------

    /// Context for the live preview: the selected recipient row's mapped values,
    /// with any unmapped placeholder falling back to `[field]` so the preview
    /// never errors on missing data.
    fn preview_context(&self, cx: &App) -> BTreeMap<String, String> {
        let mut context = match &self.recipients {
            RecipientsState::Loaded(l) => l
                .table
                .rows
                .get(self.preview_row)
                .map(|row| mapping::build_context(&l.table, row, &l.mapping))
                .unwrap_or_default(),
            _ => BTreeMap::new(),
        };
        for field in self.template_fields(cx) {
            context
                .entry(field.clone())
                .or_insert_with(|| format!("[{field}]"));
        }
        context
    }

    fn loaded_row_count(&self) -> usize {
        match &self.recipients {
            RecipientsState::Loaded(l) => l.table.row_count(),
            _ => 0,
        }
    }

    fn preview_prev(&mut self, cx: &mut Context<Self>) {
        if self.preview_row > 0 {
            self.preview_row -= 1;
            cx.notify();
        }
    }

    fn preview_next(&mut self, cx: &mut Context<Self>) {
        if self.preview_row + 1 < self.loaded_row_count() {
            self.preview_row += 1;
            cx.notify();
        }
    }

    fn render_template(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap(px(22.))
            .w_full()
            .child(labeled("tmpl.subject", &self.template.subject, cx))
            .child(self.render_placeholder_chips(cx))
            .child(
                h_flex()
                    .gap(px(20.))
                    .w_full()
                    .items_start()
                    .child(
                        v_flex()
                            .gap(px(8.))
                            .flex_1()
                            .min_w(px(0.))
                            .child(field_label("tmpl.body", cx))
                            .child(
                                div()
                                    .h(px(400.))
                                    .w_full()
                                    .rounded(px(12.))
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .overflow_hidden()
                                    .child(Input::new(&self.template.body).h_full()),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap(px(8.))
                            .flex_1()
                            .min_w(px(0.))
                            .child(self.render_preview_nav(cx))
                            .child(self.render_preview_body(window, cx)),
                    ),
            )
    }

    fn render_placeholder_chips(&self, cx: &mut Context<Self>) -> AnyElement {
        let headers = match &self.recipients {
            RecipientsState::Loaded(l) => l.table.headers.clone(),
            _ => Vec::new(),
        };
        if headers.is_empty() {
            return div()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(tr("tmpl.chips_hint"))
                .into_any_element();
        }

        let accent = cx.theme().primary;
        let surface_2 = cx.tokens().surface_2;
        let border = cx.theme().border;
        let mut row = h_flex()
            .gap(px(8.))
            .flex_wrap()
            .items_center()
            .child(field_label("tmpl.insert", cx));
        for (i, header) in headers.iter().enumerate() {
            let insert = format!("{{{{{}}}}}", template::to_placeholder_ident(header));
            row = row.child(
                div()
                    .id(SharedString::from(format!("chip-{i}")))
                    .px(px(11.))
                    .py(px(5.))
                    .rounded(px(8.))
                    .bg(surface_2)
                    .border_1()
                    .border_color(border)
                    .font_family(theme::MONO_FONT)
                    .text_size(px(13.))
                    .cursor_pointer()
                    .hover(move |h| h.border_color(accent).text_color(accent))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let text = insert.clone();
                        this.template
                            .body
                            .update(cx, |state, cx| state.insert(text, window, cx));
                        cx.notify();
                    }))
                    .child(header.clone()),
            );
        }
        row.into_any_element()
    }

    fn render_preview_nav(&self, cx: &mut Context<Self>) -> AnyElement {
        let total = self.loaded_row_count();
        let row = h_flex()
            .items_center()
            .gap(px(8.))
            .w_full()
            .child(field_label("tmpl.preview", cx))
            .child(div().flex_1());
        if total == 0 {
            return row
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(cx.tokens().text_faint)
                        .child(tr("tmpl.preview_nodata")),
                )
                .into_any_element();
        }
        let current = self.preview_row.min(total - 1) + 1;
        row.child(
            Button::new("prev-row")
                .ghost()
                .small()
                .label(tr("common.prev"))
                .disabled(self.preview_row == 0)
                .on_click(cx.listener(|this, _, _, cx| this.preview_prev(cx))),
        )
        .child(
            div()
                .text_size(px(12.5))
                .text_color(cx.theme().muted_foreground)
                .child(t!("tmpl.row_of", current = current, total = total).to_string()),
        )
        .child(
            Button::new("next-row")
                .ghost()
                .small()
                .label(tr("common.next"))
                .disabled(self.preview_row + 1 >= total)
                .on_click(cx.listener(|this, _, _, cx| this.preview_next(cx))),
        )
        .into_any_element()
    }

    fn render_preview_body(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let context = self.preview_context(cx);
        let subject_src = self.template.subject.read(cx).value();
        let body_src = self.template.body.read(cx).value();

        let subject = match template::render(&subject_src, &context) {
            Ok(s) => s,
            Err(e) => format!("⚠ {e}"),
        };
        let body_el: AnyElement = match template::render(&body_src, &context) {
            Ok(html) => div()
                .flex_1()
                .overflow_hidden()
                .child(TextView::html("tpl-preview", html, window, cx))
                .into_any_element(),
            Err(e) => div()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(format!("{}: {e}", tr("tmpl.body_error")))
                .into_any_element(),
        };

        v_flex()
            .h(px(400.))
            .w_full()
            .gap(px(12.))
            .p(px(16.))
            .rounded(px(12.))
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.tokens().surface)
            .overflow_hidden()
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .truncate()
                    .child(t!("tmpl.subject_prefix", subject = subject).to_string()),
            )
            .child(body_el)
    }

    // ---- Recipients UI --------------------------------------------------

    fn render_recipients(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.recipients {
            RecipientsState::Empty => self.render_recipients_empty(cx).into_any_element(),
            RecipientsState::Loading => div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(tr("rcpt.parsing"))
                .into_any_element(),
            RecipientsState::Error(message) => v_flex()
                .gap_3()
                .child(div().text_color(cx.theme().danger).child(message.clone()))
                .child(self.choose_file_button("retry-file", "rcpt.choose_another", cx))
                .into_any_element(),
            RecipientsState::Loaded(loaded) => self.render_loaded(loaded, cx).into_any_element(),
        }
    }

    fn choose_file_button(
        &self,
        id: &'static str,
        label_key: &'static str,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .primary()
            .icon(IconName::Inbox)
            .label(tr(label_key))
            .on_click(cx.listener(|this, _, _, cx| this.on_choose_file(cx)))
    }

    fn render_recipients_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_4()
            .items_start()
            .p_8()
            .max_w(px(560.))
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(tr("rcpt.empty")),
            )
            .child(self.choose_file_button("choose-file", "rcpt.choose", cx))
    }

    fn render_loaded(&self, loaded: &Loaded, cx: &mut Context<Self>) -> impl IntoElement {
        let file_name = loaded
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let dims = t!(
            "rcpt.dims",
            rows = loaded.table.row_count(),
            cols = loaded.table.column_count()
        )
        .to_string();

        v_flex()
            .gap(px(24.))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(12.))
                    .flex_wrap()
                    .child(mono_chip(file_name, cx))
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(cx.theme().muted_foreground)
                            .child(dims),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("change-file")
                            .outline()
                            .small()
                            .icon(IconName::Inbox)
                            .label(tr("rcpt.change"))
                            .on_click(cx.listener(|this, _, _, cx| this.on_choose_file(cx))),
                    ),
            )
            .when(loaded.sheets.len() > 1, |this| {
                this.child(self.render_sheet_picker(loaded, cx))
            })
            .child(self.render_email_picker(loaded, cx))
            .child(self.render_mapping(loaded, cx))
            .child(self.render_validation_summary(loaded, cx))
            .child(self.render_preview(loaded, cx))
    }

    fn render_sheet_picker(&self, loaded: &Loaded, cx: &mut Context<Self>) -> impl IntoElement {
        let current = loaded.sheet.clone();
        let mut chips = Vec::new();
        for (i, name) in loaded.sheets.iter().enumerate() {
            let selected = current.as_deref() == Some(name.as_str());
            let value = name.clone();
            chips.push(seg(
                Button::new(SharedString::from(format!("sheet-{i}")))
                    .label(name.clone())
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.on_select_sheet(value.clone(), cx)),
                    ),
                selected,
            ));
        }
        v_flex()
            .gap(px(9.))
            .child(field_label("rcpt.sheet", cx))
            .child(h_flex().gap(px(6.)).flex_wrap().children(chips))
    }

    fn render_email_picker(&self, loaded: &Loaded, cx: &mut Context<Self>) -> impl IntoElement {
        let email_col = loaded.email_col;
        let mut chips = Vec::new();
        for (i, header) in loaded.table.headers.iter().enumerate() {
            chips.push(seg(
                Button::new(SharedString::from(format!("email-col-{i}")))
                    .label(header.clone())
                    .on_click(cx.listener(move |this, _, _, cx| this.set_email_col(i, cx))),
                i == email_col,
            ));
        }
        v_flex()
            .gap(px(9.))
            .child(field_label("rcpt.email_col", cx))
            .child(h_flex().gap(px(6.)).flex_wrap().children(chips))
    }

    fn render_mapping(&self, loaded: &Loaded, cx: &mut Context<Self>) -> AnyElement {
        if loaded.fields.is_empty() {
            return v_flex()
                .gap(px(9.))
                .child(field_label("rcpt.mapping", cx))
                .child(
                    div()
                        .text_size(px(14.))
                        .text_color(cx.theme().muted_foreground)
                        .child(tr("rcpt.no_fields")),
                )
                .child(
                    Button::new("refresh-fields")
                        .outline()
                        .small()
                        .label(tr("rcpt.refresh"))
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_fields(cx))),
                )
                .into_any_element();
        }

        let mut list = v_flex().gap(px(14.)).child(field_label("rcpt.mapping", cx));
        for field in &loaded.fields {
            let selected = loaded.mapping.get(field).cloned();
            let field_name = field.clone();

            let none_field = field_name.clone();
            let mut chips = h_flex().gap(px(6.)).flex_wrap().child(seg(
                Button::new(SharedString::from(format!("map-{field}-none")))
                    .label(tr("rcpt.none"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_mapping(none_field.clone(), None, cx)
                    })),
                selected.is_none(),
            ));
            for (i, header) in loaded.table.headers.iter().enumerate() {
                let is_selected = selected.as_deref() == Some(header.as_str());
                let col = header.clone();
                let field_for_col = field_name.clone();
                chips = chips.child(seg(
                    Button::new(SharedString::from(format!("map-{field}-{i}")))
                        .label(header.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_mapping(field_for_col.clone(), Some(col.clone()), cx)
                        })),
                    is_selected,
                ));
            }

            list = list.child(
                h_flex()
                    .items_center()
                    .gap(px(16.))
                    .child(
                        div()
                            .w(px(92.))
                            .flex_shrink_0()
                            .font_family(theme::MONO_FONT)
                            .text_size(px(13.))
                            .child(SharedString::from(format!("{{{{{field}}}}}"))),
                    )
                    .child(chips),
            );
        }

        list.into_any_element()
    }

    fn render_validation_summary(
        &self,
        loaded: &Loaded,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let report = &loaded.report;
        let faint = cx.tokens().text_faint;
        // Zero stats read faint; non-zero ones carry their semantic color.
        let dim = |n: usize, color: Hsla| if n == 0 { faint } else { color };
        let sendable = loaded.sendable();

        card(cx)
            .flex()
            .items_center()
            .gap(px(22.))
            .flex_wrap()
            .px(px(18.))
            .py(px(14.))
            .child(stat(
                "rcpt.will_send",
                sendable,
                dim(sendable, cx.theme().success),
            ))
            .child(stat(
                "rcpt.bad_email",
                report.invalid_email,
                dim(report.invalid_email, cx.theme().danger),
            ))
            .child(stat(
                "rcpt.duplicates",
                report.duplicates,
                dim(report.duplicates, cx.theme().warning),
            ))
            .child(stat(
                "rcpt.missing",
                report.missing_fields,
                dim(report.missing_fields, cx.theme().danger),
            ))
            .child(div().flex_1())
            .child(
                Switch::new("dedupe-switch")
                    .checked(loaded.dedupe)
                    .label(tr("rcpt.dedupe"))
                    .on_click(cx.listener(|this, _: &bool, _, cx| this.toggle_dedupe(cx))),
            )
            .child(
                Button::new("refresh-fields")
                    .link()
                    .label(tr("rcpt.refresh"))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_fields(cx))),
            )
    }

    fn render_preview(&self, loaded: &Loaded, cx: &mut Context<Self>) -> impl IntoElement {
        // Choose a bounded set of columns: email + mapped fields, or the first
        // few columns when nothing is mapped yet.
        let email_col = loaded.email_col;
        let mut columns: Vec<(String, usize)> = vec![(tr("rcpt.email"), email_col)];
        if loaded.mapping.is_empty() {
            for (idx, header) in loaded.table.headers.iter().enumerate() {
                if idx == email_col || columns.len() >= 5 {
                    continue;
                }
                columns.push((header.clone(), idx));
            }
        } else {
            for (field, col) in &loaded.mapping {
                if let Some(idx) = loaded.table.column_index(col)
                    && idx != email_col
                {
                    columns.push((field.clone(), idx));
                }
            }
        }

        let muted = cx.theme().muted_foreground;
        let head = |text: String| {
            div()
                .flex_1()
                .text_size(px(11.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(muted)
                .truncate()
                .child(text.to_uppercase())
        };
        let header_row = h_flex()
            .w_full()
            .px(px(18.))
            .py(px(11.))
            .gap(px(12.))
            .bg(cx.tokens().surface_2)
            .child(
                div()
                    .w(px(110.))
                    .text_size(px(11.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(muted)
                    .child(tr("rcpt.status").to_uppercase()),
            )
            .children(columns.iter().map(|(name, _)| head(name.clone())));

        let table = loaded.table.clone();
        let report = loaded.report.clone();
        let row_columns = columns.clone();
        let border = cx.theme().border;
        let count = table.row_count();

        let list = uniform_list("recipients-preview", count, move |range, _window, cx| {
            let mut items = Vec::with_capacity(range.end - range.start);
            for ix in range {
                let (color, label) = status_style(&report.statuses[ix], cx);
                let row = &table.rows[ix];
                let cells = row_columns.iter().enumerate().map(|(ci, (_, idx))| {
                    let cell = div()
                        .flex_1()
                        .text_size(px(14.))
                        .truncate()
                        .child(row.get(*idx).cloned().unwrap_or_default());
                    // The first column is the email address — render it mono.
                    if ci == 0 {
                        cell.font_family(theme::MONO_FONT)
                    } else {
                        cell
                    }
                });
                items.push(
                    h_flex()
                        .w_full()
                        .px(px(18.))
                        .py(px(12.))
                        .gap(px(12.))
                        .border_t_1()
                        .border_color(border)
                        .child(
                            h_flex()
                                .w(px(110.))
                                .items_center()
                                .gap(px(7.))
                                .child(div().size(px(7.)).rounded_full().bg(color))
                                .child(
                                    div()
                                        .text_size(px(12.5))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(color)
                                        .child(label),
                                ),
                        )
                        .children(cells),
                );
            }
            items
        })
        .h(px(320.));

        v_flex()
            .rounded(px(12.))
            .border_1()
            .border_color(cx.theme().border)
            .overflow_hidden()
            .child(header_row)
            .child(list)
    }

    // ---- Send logic (M4) ------------------------------------------------

    /// The account that will send: the explicit selection, else the first saved.
    fn active_account_id(&self) -> Option<String> {
        self.selected_account
            .clone()
            .or_else(|| self.store.accounts.first().map(|a| a.id.clone()))
    }

    /// Count of recipients that will actually be sent, respecting de-dupe and
    /// any active resume filter.
    fn sendable_count(&self) -> usize {
        match &self.recipients {
            RecipientsState::Loaded(l) => self.selected_indices(l).len(),
            _ => 0,
        }
    }

    /// Row indices that will be sent: valid rows (plus duplicates when de-dupe
    /// is off), further restricted to the resume set when resuming.
    fn selected_indices(&self, loaded: &Loaded) -> Vec<usize> {
        loaded
            .report
            .statuses
            .iter()
            .enumerate()
            .filter_map(|(i, status)| {
                let base = matches!(status, RowStatus::Ok)
                    || (matches!(status, RowStatus::Duplicate) && !loaded.dedupe);
                let resume_ok = self.resume.as_ref().is_none_or(|r| r.indices.contains(&i));
                (base && resume_ok).then_some(i)
            })
            .collect()
    }

    /// Assemble a [`CampaignPlan`] from the current account, template, and the
    /// sendable recipient rows.
    /// Sending settings parsed from the pre-flight inputs, falling back to
    /// defaults for empty/invalid entries.
    fn sending_config(&self, cx: &App) -> SendingConfig {
        let d = SendingConfig::default();
        let mps = self
            .send_mps
            .read(cx)
            .value()
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|v| *v > 0.0)
            .unwrap_or(d.messages_per_second);
        let retry_limit = self
            .send_retry
            .read(cx)
            .value()
            .trim()
            .parse::<u32>()
            .unwrap_or(d.retry_limit);
        let stop_after_failures = self
            .send_stop
            .read(cx)
            .value()
            .trim()
            .parse::<u32>()
            .unwrap_or(d.stop_after_failures);
        SendingConfig {
            messages_per_second: mps,
            retry_limit,
            stop_after_failures,
        }
    }

    /// Shared prerequisites for any send: account (+ keychain secret), templates,
    /// and sending settings.
    fn resolve_send_context(
        &self,
        cx: &App,
    ) -> Result<(Account, String, String, String, SendingConfig), String> {
        let account_id = self
            .active_account_id()
            .ok_or("Add and select a sending account first (Accounts step).")?;
        let account = self
            .store
            .get(&account_id)
            .ok_or("Selected account not found.")?
            .clone();
        let secret = secrets::get(&account.id)
            .map_err(|e| e.to_string())?
            .ok_or("No secret is stored for this account — re-add it in the Accounts step.")?;

        let subject = self.template.subject.read(cx).value().to_string();
        let body = self.template.body.read(cx).value().to_string();
        if subject.trim().is_empty() && body.trim().is_empty() {
            return Err("Write a subject or body in the Template step.".into());
        }
        Ok((account, secret, subject, body, self.sending_config(cx)))
    }

    fn build_campaign_plan(&self, cx: &App) -> Result<CampaignPlan, String> {
        let (account, secret, subject, body, cfg) = self.resolve_send_context(cx)?;

        let loaded = match &self.recipients {
            RecipientsState::Loaded(l) => l,
            _ => return Err("Load a recipient list first (Recipients step).".into()),
        };

        let email_col = loaded.email_col;
        let recipients: Vec<CampaignRecipient> = self
            .selected_indices(loaded)
            .into_iter()
            .map(|i| {
                let row = &loaded.table.rows[i];
                CampaignRecipient {
                    index: i,
                    email: row.get(email_col).cloned().unwrap_or_default(),
                    context: mapping::build_context(&loaded.table, row, &loaded.mapping),
                }
            })
            .collect();
        if recipients.is_empty() {
            return Err(if self.resume.is_some() {
                "No matching failed/cancelled rows to resume.".into()
            } else {
                "No valid recipients to send to.".into()
            });
        }

        Ok(CampaignPlan {
            account,
            secret,
            subject_template: subject,
            body_template: body,
            generate_text_alt: true,
            messages_per_second: cfg.messages_per_second,
            retry_limit: cfg.retry_limit,
            stop_after_failures: cfg.stop_after_failures,
            recipients,
        })
    }

    /// A one-recipient plan for the "send test" button, using the current
    /// preview row's data so placeholders render realistically.
    fn build_test_plan(&self, to: &str, cx: &App) -> Result<CampaignPlan, String> {
        let (account, secret, subject, body, cfg) = self.resolve_send_context(cx)?;
        Ok(CampaignPlan {
            account,
            secret,
            subject_template: subject,
            body_template: body,
            generate_text_alt: true,
            messages_per_second: cfg.messages_per_second,
            retry_limit: cfg.retry_limit,
            stop_after_failures: cfg.stop_after_failures,
            recipients: vec![CampaignRecipient {
                index: 0,
                email: to.to_string(),
                context: self.preview_context(cx),
            }],
        })
    }

    fn on_send_test(&mut self, cx: &mut Context<Self>) {
        let to = self.test_email.read(cx).value().trim().to_string();
        if !is_email(&to) {
            self.send_notice = Some(Notice {
                ok: false,
                text: tr("send.test_invalid"),
            });
            cx.notify();
            return;
        }
        match self.build_test_plan(&to, cx) {
            Ok(plan) => {
                self.sending = true;
                self.summary = None;
                self.progress = None;
                self.send_notice = None;
                self.mail.command(Command::StartCampaign(Box::new(plan)));
            }
            Err(text) => self.send_notice = Some(Notice { ok: false, text }),
        }
        cx.notify();
    }

    fn on_send(&mut self, cx: &mut Context<Self>) {
        match self.build_campaign_plan(cx) {
            Ok(plan) => {
                self.sending = true;
                self.summary = None;
                self.progress = None;
                self.send_notice = None;
                self.mail.command(Command::StartCampaign(Box::new(plan)));
            }
            Err(text) => self.send_notice = Some(Notice { ok: false, text }),
        }
        cx.notify();
    }

    fn on_cancel_send(&mut self, cx: &mut Context<Self>) {
        self.mail.command(Command::CancelCampaign);
        cx.notify();
    }

    fn start_again(&mut self, cx: &mut Context<Self>) {
        self.summary = None;
        self.progress = None;
        self.send_notice = None;
        cx.notify();
    }

    fn on_export_report(&mut self, cx: &mut Context<Self>) {
        let dir = match &self.recipients {
            RecipientsState::Loaded(l) => l
                .path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
            _ => std::env::current_dir().unwrap_or_default(),
        };
        let receiver = cx.prompt_for_new_path(&dir, Some("outcome-report.csv"));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = receiver.await {
                let _ = this.update(cx, |this, cx| {
                    this.mail.command(Command::ExportReport { path });
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn on_load_resume_report(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr("send.resume_prompt").into()),
        });
        cx.spawn(async move |this, cx| {
            let selected = match paths.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let Some(path) = selected else { return };
            let parse_path = path.clone();
            let parsed = cx
                .background_executor()
                .spawn(async move { mmm_engine::load_report(&parse_path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.on_resume_report_loaded(path, parsed, cx)
            });
        })
        .detach();
    }

    fn on_resume_report_loaded(
        &mut self,
        path: PathBuf,
        parsed: Result<Vec<RowOutcome>, String>,
        cx: &mut Context<Self>,
    ) {
        match parsed {
            Err(e) => {
                self.send_notice = Some(Notice {
                    ok: false,
                    text: format!("{}: {e}", tr("send.resume_read_err")),
                });
            }
            Ok(rows) => {
                let mut indices = HashSet::new();
                let (mut failed, mut skipped) = (0usize, 0usize);
                for row in &rows {
                    match row.status {
                        OutcomeStatus::Sent => {}
                        OutcomeStatus::Failed => {
                            failed += 1;
                            indices.insert(row.index);
                        }
                        OutcomeStatus::Skipped => {
                            skipped += 1;
                            indices.insert(row.index);
                        }
                    }
                }
                if indices.is_empty() {
                    self.resume = None;
                    self.send_notice = Some(Notice {
                        ok: true,
                        text: tr("send.resume_none"),
                    });
                } else {
                    let source = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("report")
                        .to_string();
                    self.resume = Some(ResumeInfo {
                        indices,
                        failed,
                        skipped,
                        source,
                    });
                    self.send_notice = None;
                }
            }
        }
        cx.notify();
    }

    fn clear_resume(&mut self, cx: &mut Context<Self>) {
        self.resume = None;
        cx.notify();
    }

    // ---- Send UI --------------------------------------------------------

    fn render_send(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.sending {
            self.render_progress(cx).into_any_element()
        } else if let Some(summary) = &self.summary {
            self.render_finished(summary, cx).into_any_element()
        } else {
            self.render_preflight(cx).into_any_element()
        }
    }

    fn render_account_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_account_id();
        let mut chips = Vec::new();
        for account in &self.store.accounts {
            let id = account.id.clone();
            let selected = active.as_deref() == Some(id.as_str());
            chips.push(seg(
                Button::new(SharedString::from(format!("send-acct-{id}")))
                    .label(account.display.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_account = Some(id.clone());
                        cx.notify();
                    })),
                selected,
            ));
        }
        v_flex()
            .gap(px(9.))
            .child(field_label("send.account", cx))
            .child(h_flex().gap(px(8.)).flex_wrap().children(chips))
    }

    fn render_preflight(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let account_label = self
            .active_account_id()
            .and_then(|id| self.store.get(&id).map(|a| a.display.clone()))
            .unwrap_or_else(|| tr("send.none"));
        let subject = self.template.subject.read(cx).value().to_string();
        let subject = if subject.trim().is_empty() {
            tr("send.empty")
        } else {
            subject
        };
        let sendable = self.sendable_count();
        let mps = SendingConfig::default().messages_per_second.max(1.0);
        let eta_secs = (sendable as f32 / mps).ceil() as u64;

        let mut warnings: Vec<String> = Vec::new();
        if self.store.accounts.is_empty() {
            warnings.push(tr("send.warn_no_account"));
        }
        match &self.recipients {
            RecipientsState::Loaded(l) => {
                if sendable == 0 {
                    warnings.push(tr("send.warn_no_valid"));
                }
                let flagged = l.report.invalid_email + l.report.missing_fields;
                if flagged > 0 {
                    warnings.push(t!("send.warn_skipped", n = flagged).to_string());
                }
                if l.dedupe && l.report.duplicates > 0 {
                    warnings.push(t!("send.warn_dupes", n = l.report.duplicates).to_string());
                }
            }
            _ => warnings.push(tr("send.warn_no_list")),
        }

        let can_send = !self.store.accounts.is_empty() && sendable > 0;

        let send_label = if self.resume.is_some() {
            t!("send.resume_btn", n = sendable).to_string()
        } else {
            t!("send.send_btn", n = sendable).to_string()
        };

        v_flex()
            .gap(px(24.))
            .child(self.render_account_picker(cx))
            .child(self.render_resume_control(cx))
            .child(self.render_sending_settings(cx))
            .child(
                card(cx)
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(summary_row("send.f_account", account_label, cx))
                    .child(summary_row("send.f_subject", subject, cx))
                    .child(summary_row(
                        "send.f_recipients",
                        t!("send.will_be_sent", n = sendable).to_string(),
                        cx,
                    ))
                    .child(summary_row(
                        "send.f_duration",
                        format!("~{}", format_duration(eta_secs)),
                        cx,
                    )),
            )
            .when(!warnings.is_empty(), |this| {
                this.child(v_flex().gap_1().children(warnings.into_iter().map(|w| {
                    div()
                        .text_size(px(14.))
                        .text_color(cx.theme().warning)
                        .child(format!("⚠ {w}"))
                })))
            })
            .when_some(self.send_notice.clone(), |this, notice| {
                let color = if notice.ok {
                    cx.theme().success
                } else {
                    cx.theme().danger
                };
                this.child(
                    div()
                        .text_size(px(14.))
                        .text_color(color)
                        .child(notice.text),
                )
            })
            .child(self.render_test_send(cx))
            .child(
                Button::new("send-campaign")
                    .primary()
                    .w_full()
                    .icon(IconName::ArrowRight)
                    .label(send_label)
                    .disabled(!can_send)
                    .on_click(cx.listener(|this, _, _, cx| this.on_send(cx))),
            )
    }

    fn render_sending_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(field_label("send.settings", cx))
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .child(labeled("send.mps", &self.send_mps, cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(labeled("send.retry", &self.send_retry, cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(labeled("send.stop", &self.send_stop, cx)),
                    ),
            )
    }

    fn render_test_send(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(field_label("send.test_label", cx))
            .child(
                h_flex()
                    .gap_2()
                    .items_end()
                    .child(div().flex_1().child(Input::new(&self.test_email)))
                    .child(
                        Button::new("send-test")
                            .outline()
                            .label(tr("send.test_btn"))
                            .disabled(self.store.accounts.is_empty())
                            .on_click(cx.listener(|this, _, _, cx| this.on_send_test(cx))),
                    ),
            )
    }

    fn render_resume_control(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.resume {
            Some(r) => h_flex()
                .items_center()
                .justify_between()
                .gap_3()
                .p_3()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().secondary)
                .child(
                    div().text_sm().child(
                        t!(
                            "send.resuming",
                            source = r.source,
                            failed = r.failed,
                            cancelled = r.skipped
                        )
                        .to_string(),
                    ),
                )
                .child(
                    Button::new("clear-resume")
                        .ghost()
                        .label(tr("common.clear"))
                        .on_click(cx.listener(|this, _, _, cx| this.clear_resume(cx))),
                )
                .into_any_element(),
            None => h_flex()
                .child(
                    Button::new("resume-report")
                        .link()
                        .label(tr("send.resume_load"))
                        .on_click(cx.listener(|this, _, _, cx| this.on_load_resume_report(cx))),
                )
                .into_any_element(),
        }
    }

    fn render_progress(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.progress.clone().unwrap_or(CampaignProgress {
            sent: 0,
            failed: 0,
            skipped: 0,
            total: self.sendable_count(),
            rate_per_sec: 0.0,
            eta_secs: None,
            recent: Vec::new(),
            state: CampaignState::Running,
        });
        let processed = p.sent + p.failed + p.skipped;
        let percentage = if p.total == 0 {
            0.0
        } else {
            processed as f32 / p.total as f32 * 100.0
        };
        let eta = p
            .eta_secs
            .map(|s| t!("send.eta_left", d = format_duration(s)).to_string())
            .unwrap_or_else(|| tr("send.estimating"));

        v_flex()
            .gap_3()
            .max_w(px(720.))
            .child(Progress::new().value(percentage))
            .child(
                h_flex()
                    .gap_5()
                    .flex_wrap()
                    .child(stat("send.sent", p.sent, cx.theme().success))
                    .child(stat("send.failed", p.failed, cx.theme().danger))
                    .child(stat("send.skipped", p.skipped, cx.theme().warning))
                    .child(stat("send.total", p.total, cx.theme().foreground))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{:.0}/s · {eta}", p.rate_per_sec)),
                    ),
            )
            .child(self.render_recent_rows(&p.recent, cx))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Button::new("cancel-campaign")
                            .danger()
                            .label(tr("common.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| this.on_cancel_send(cx))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(tr("send.cancel_hint")),
                    ),
            )
    }

    fn render_finished(
        &self,
        summary: &CampaignSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (headline, color) = match &summary.state {
            CampaignState::Completed => (tr("send.done"), cx.theme().success),
            CampaignState::Cancelled => (tr("send.cancelled"), cx.theme().warning),
            CampaignState::Stopped(reason) => (
                format!("{} — {reason}", tr("send.stopped")),
                cx.theme().danger,
            ),
            CampaignState::Running => (tr("send.finishing"), cx.theme().foreground),
        };
        let recent = self
            .progress
            .as_ref()
            .map(|p| p.recent.clone())
            .unwrap_or_default();

        v_flex()
            .gap_4()
            .max_w(px(720.))
            .child(div().text_lg().text_color(color).child(headline))
            .child(
                h_flex()
                    .gap_5()
                    .flex_wrap()
                    .child(stat("send.sent", summary.sent, cx.theme().success))
                    .child(stat("send.failed", summary.failed, cx.theme().danger))
                    .child(stat("send.skipped", summary.skipped, cx.theme().warning))
                    .child(stat("send.total", summary.total, cx.theme().foreground))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                t!(
                                    "send.in_duration",
                                    d = format_duration(summary.elapsed_secs)
                                )
                                .to_string(),
                            ),
                    ),
            )
            .child(self.render_recent_rows(&recent, cx))
            .when_some(self.send_notice.clone(), |this, notice| {
                let color = if notice.ok {
                    cx.theme().success
                } else {
                    cx.theme().danger
                };
                this.child(div().text_sm().text_color(color).child(notice.text))
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("export-report")
                            .outline()
                            .icon(IconName::ArrowDown)
                            .label(tr("send.export"))
                            .on_click(cx.listener(|this, _, _, cx| this.on_export_report(cx))),
                    )
                    .child(
                        Button::new("send-again")
                            .ghost()
                            .label(tr("send.back"))
                            .on_click(cx.listener(|this, _, _, cx| this.start_again(cx))),
                    ),
            )
    }

    fn render_recent_rows(
        &self,
        recent: &[RowOutcome],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rows = recent.iter().take(12).map(|o| {
            let (color, label) = outcome_style(&o.status, cx);
            h_flex()
                .w_full()
                .px_2()
                .py_1()
                .gap_2()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(div().w(px(72.)).text_xs().text_color(color).child(label))
                .child(div().flex_1().text_sm().truncate().child(o.email.clone()))
                .child(
                    div()
                        .flex_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .truncate()
                        .child(o.error.clone().unwrap_or_default()),
                )
        });

        v_flex()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .overflow_hidden()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .bg(cx.theme().secondary)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(tr("send.recent_activity")),
            )
            .children(rows)
    }

    // ---- Project files (M5) ---------------------------------------------

    /// A snapshot of the persistable campaign state, for dirty comparison.
    fn current_snapshot(&self, cx: &App) -> ProjectSnapshot {
        let (source_path, sheet, email_column, mapping, dedupe) = match &self.recipients {
            RecipientsState::Loaded(l) => (
                Some(l.path.to_string_lossy().to_string()),
                l.sheet.clone(),
                l.table.headers.get(l.email_col).cloned(),
                l.mapping.clone(),
                l.dedupe,
            ),
            _ => (None, None, None, BTreeMap::new(), true),
        };
        ProjectSnapshot {
            subject: self.template.subject.read(cx).value().to_string(),
            body: self.template.body.read(cx).value().to_string(),
            account: self.selected_account.clone(),
            source_path,
            sheet,
            email_column,
            mapping,
            dedupe,
            sending: self.sending_config(cx),
        }
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.current_snapshot(cx) != self.saved_snapshot
    }

    /// Adopt the current state as the saved baseline (call after save/load).
    fn mark_saved(&mut self, cx: &App) {
        self.saved_snapshot = self.current_snapshot(cx);
    }

    fn on_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.guarded(PendingAction::New, window, cx);
    }

    fn on_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.guarded(PendingAction::OpenDialog, window, cx);
    }

    fn open_recent(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.guarded(PendingAction::OpenPath(path), window, cx);
    }

    /// Run `action` now, or after confirming discard when there are unsaved changes.
    fn guarded(&mut self, action: PendingAction, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_dirty(cx) {
            self.perform(action, window, cx);
            return;
        }
        let title = tr("proj.discard_title");
        let detail = tr("proj.discard_detail");
        let yes = tr("proj.discard_yes");
        let cancel = tr("common.cancel");
        let answer = window.prompt(
            PromptLevel::Warning,
            &title,
            Some(&detail),
            &[yes.as_str(), cancel.as_str()],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = answer.await {
                let _ = this.update_in(cx, |this, window, cx| this.perform(action, window, cx));
            }
        })
        .detach();
    }

    fn perform(&mut self, action: PendingAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            PendingAction::New => self.do_new(window, cx),
            PendingAction::OpenDialog => self.open_dialog(window, cx),
            PendingAction::OpenPath(path) => self.load_project(path, window, cx),
        }
    }

    fn do_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.template
            .subject
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.template
            .body
            .update(cx, |s, cx| s.set_value("", window, cx));
        let d = SendingConfig::default();
        self.send_mps.update(cx, |s, cx| {
            s.set_value(d.messages_per_second.to_string(), window, cx)
        });
        self.send_retry.update(cx, |s, cx| {
            s.set_value(d.retry_limit.to_string(), window, cx)
        });
        self.send_stop.update(cx, |s, cx| {
            s.set_value(d.stop_after_failures.to_string(), window, cx)
        });
        self.selected_account = None;
        self.recipients = RecipientsState::Empty;
        self.preview_row = 0;
        self.sending = false;
        self.progress = None;
        self.summary = None;
        self.send_notice = None;
        self.resume = None;
        self.project_path = None;
        self.project_name = tr("proj.untitled");
        self.project_notice = None;
        self.mark_saved(cx);
        cx.notify();
    }

    fn open_dialog(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr("proj.open_prompt").into()),
        });
        cx.spawn_in(_window, async move |this, cx| {
            let selected = match paths.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            if let Some(path) = selected {
                let _ = this.update_in(cx, |this, window, cx| this.load_project(path, window, cx));
            }
        })
        .detach();
    }

    fn load_project(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let project = match Project::load(&path) {
            Ok(p) => p,
            Err(e) => {
                self.project_notice = Some(Notice {
                    ok: false,
                    text: format!("{}: {e}", tr("proj.open_err")),
                });
                cx.notify();
                return;
            }
        };
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();

        // Subject + body (body lives in the sibling HTML file).
        self.template.subject.update(cx, |s, cx| {
            s.set_value(project.template.subject.clone(), window, cx)
        });
        let html_full = resolve(&dir, &project.template.html_path);
        let body = std::fs::read_to_string(&html_full).unwrap_or_default();
        self.template
            .body
            .update(cx, |s, cx| s.set_value(body, window, cx));

        self.send_mps.update(cx, |s, cx| {
            s.set_value(project.sending.messages_per_second.to_string(), window, cx)
        });
        self.send_retry.update(cx, |s, cx| {
            s.set_value(project.sending.retry_limit.to_string(), window, cx)
        });
        self.send_stop.update(cx, |s, cx| {
            s.set_value(project.sending.stop_after_failures.to_string(), window, cx)
        });

        self.selected_account = project.account.as_ref().map(|a| a.id.clone());
        self.project_path = Some(path.clone());
        self.project_name = project.name.clone();
        self.sending = false;
        self.progress = None;
        self.summary = None;
        self.send_notice = None;
        self.resume = None;
        self.project_notice = Some(Notice {
            ok: true,
            text: t!("proj.opened", path = path.display()).to_string(),
        });

        self.recents.push(&path);
        let _ = self.recents.save();

        match &project.recipients {
            Some(source) => {
                let src = resolve(&dir, &source.source_path);
                let preset = Some((source.email_column.clone(), source.mapping.clone()));
                self.spawn_parse(src, source.sheet.clone(), preset, true, cx);
            }
            None => {
                self.recipients = RecipientsState::Empty;
            }
        }

        self.mark_saved(cx);
        cx.notify();
    }

    fn on_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.project_path.clone() {
            Some(path) => self.write_project(path, cx),
            None => self.on_save_as(window, cx),
        }
    }

    fn on_save_as(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let dir = self
            .project_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .or_else(|| match &self.recipients {
                RecipientsState::Loaded(l) => l.path.parent().map(Path::to_path_buf),
                _ => None,
            })
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let suggested = format!("campaign{PROJECT_SUFFIX}");
        let receiver = cx.prompt_for_new_path(&dir, Some(&suggested));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = receiver.await {
                let _ = this.update(cx, |this, cx| this.write_project(path, cx));
            }
        })
        .detach();
    }

    fn write_project(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let base = base_name(&path);
        let html_file = format!("{base}.html");

        // Body → sibling HTML file (keeps the TOML clean).
        let body = self.template.body.read(cx).value().to_string();
        if let Err(e) = std::fs::write(dir.join(&html_file), &body) {
            self.project_notice = Some(Notice {
                ok: false,
                text: format!("{}: {e}", tr("proj.write_err")),
            });
            cx.notify();
            return;
        }

        let account = self
            .selected_account
            .as_ref()
            .and_then(|id| self.store.get(id))
            .map(|a| AccountRef {
                id: a.id.clone(),
                display: a.display.clone(),
            });
        let recipients = match &self.recipients {
            RecipientsState::Loaded(l) => Some(RecipientSource {
                source_path: relative_or_absolute(&dir, &l.path),
                sheet: l.sheet.clone(),
                email_column: l
                    .table
                    .headers
                    .get(l.email_col)
                    .cloned()
                    .unwrap_or_default(),
                mapping: l.mapping.clone(),
            }),
            _ => None,
        };

        let project = Project {
            version: CURRENT_VERSION,
            name: base.clone(),
            account,
            template: TemplateSpec {
                subject: self.template.subject.read(cx).value().to_string(),
                html_path: html_file,
                generate_text_alt: true,
            },
            recipients,
            sending: self.sending_config(cx),
        };

        if let Err(e) = project.save(&path) {
            self.project_notice = Some(Notice {
                ok: false,
                text: format!("{}: {e}", tr("proj.save_err")),
            });
            cx.notify();
            return;
        }

        self.project_path = Some(path.clone());
        self.project_name = base;
        self.recents.push(&path);
        let _ = self.recents.save();
        self.mark_saved(cx);
        self.project_notice = Some(Notice {
            ok: true,
            text: tr("proj.saved"),
        });
        cx.notify();
    }

    /// The accent rounded-square app mark with the white mail glyph.
    fn app_mark(&self, size: Pixels, icon: Pixels, radius: Pixels, cx: &Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .justify_center()
            .size(size)
            .flex_shrink_0()
            .rounded(radius)
            .bg(cx.theme().primary)
            .text_color(cx.theme().primary_foreground)
            .child(Icon::empty().path("icons/mail.svg").with_size(icon))
    }

    /// The custom window title bar: app mark, language dropdown, theme toggle,
    /// file actions, and (appended by `TitleBar`) the min/max/close controls.
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        TitleBar::new()
            .h(px(46.))
            .child(self.app_mark_group(cx))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(6.))
                    .pr(px(4.))
                    .child(self.render_language_menu(cx))
                    .child(self.render_theme_toggle(cx))
                    .child(Divider::vertical().h(px(20.)))
                    .child(self.render_open_menu(cx))
                    .child(
                        Button::new("proj-new")
                            .small()
                            .outline()
                            .label(tr("top.new"))
                            .on_click(cx.listener(|this, _, window, cx| this.on_new(window, cx))),
                    )
                    .child(
                        Button::new("proj-save")
                            .small()
                            .primary()
                            .label(tr("top.save"))
                            .on_click(cx.listener(|this, _, window, cx| this.on_save(window, cx))),
                    )
                    .child(
                        Button::new("proj-save-as")
                            .small()
                            .outline()
                            .label(tr("top.save_as"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.on_save_as(window, cx)),
                            ),
                    )
                    .child(Divider::vertical().h(px(20.))),
            )
    }

    fn app_mark_group(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .items_center()
            .gap(px(9.))
            .child(self.app_mark(px(26.), px(16.), px(7.), cx))
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("MassFckinMailer"),
            )
    }

    /// One dropdown that lists every UI language (replaces the old inline row).
    fn render_language_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = LANGUAGES
            .iter()
            .find(|(code, _)| *code == self.language)
            .map(|(_, label)| *label)
            .unwrap_or("EN");
        let view = cx.entity();
        let selected_code = self.language.clone();
        Popover::new("lang-menu")
            .anchor(Corner::TopRight)
            .open(self.lang_menu_open)
            .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                this.lang_menu_open = *open;
                cx.notify();
            }))
            .trigger(
                Button::new("lang-trigger")
                    .small()
                    .outline()
                    .icon(IconName::Globe)
                    .label(current.to_string()),
            )
            .content(move |_state, _window, _cx| {
                let mut list = v_flex().gap(px(2.)).min_w(px(150.));
                for (code, label) in LANGUAGES {
                    let code = *code;
                    let selected = selected_code == code;
                    let view = view.clone();
                    list = list.child(
                        Button::new(SharedString::from(format!("lang-item-{code}")))
                            .ghost()
                            .w_full()
                            .label(*label)
                            .selected(selected)
                            .on_click(move |_, _window, cx| {
                                view.update(cx, |this, cx| {
                                    this.on_set_language(code, cx);
                                    this.lang_menu_open = false;
                                });
                            }),
                    );
                }
                list
            })
    }

    /// One button: moon in light mode, sun in dark; flips Light <-> Dark.
    /// Left-click flips Light <-> Dark; right-click switches to Auto (follow OS).
    /// The button reads `selected` while in Auto mode as a subtle hint.
    fn render_theme_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dark = cx.theme().is_dark();
        let (icon, next, base_tip) = if dark {
            (IconName::Sun, ThemePref::Light, tr("theme.light"))
        } else {
            (IconName::Moon, ThemePref::Dark, tr("theme.dark"))
        };
        let auto = self.theme_pref == ThemePref::Auto;
        let tip = format!(
            "{} · {}",
            if auto { tr("theme.auto") } else { base_tip },
            tr("theme.auto_hint")
        );
        div()
            .id("theme-toggle-wrap")
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, window, cx| this.on_set_theme(ThemePref::Auto, window, cx)),
            )
            .child(
                Button::new("theme-toggle")
                    .small()
                    .outline()
                    .icon(icon)
                    .tooltip(tip)
                    .selected(auto)
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.on_set_theme(next, window, cx)),
                    ),
            )
    }

    /// The `Open` button as a dropdown: "Open file…" plus recent projects.
    fn render_open_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let recents: Vec<(String, PathBuf)> = self
            .recents
            .paths
            .iter()
            .take(6)
            .map(|e| (base_name(&PathBuf::from(e)), PathBuf::from(e)))
            .collect();
        Popover::new("open-menu")
            .anchor(Corner::TopRight)
            .open(self.open_menu_open)
            .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                this.open_menu_open = *open;
                cx.notify();
            }))
            .trigger(
                Button::new("proj-open")
                    .small()
                    .outline()
                    .label(tr("top.open")),
            )
            .content(move |_state, _window, _cx| {
                let mut list = v_flex().gap(px(2.)).min_w(px(220.));
                let open_view = view.clone();
                list = list.child(
                    Button::new("open-file")
                        .ghost()
                        .w_full()
                        .label(tr("top.open"))
                        .on_click(move |_, window, cx| {
                            open_view.update(cx, |this, cx| {
                                this.open_menu_open = false;
                                this.on_open(window, cx);
                            });
                        }),
                );
                if !recents.is_empty() {
                    list = list.child(Divider::horizontal());
                }
                for (i, (label, path)) in recents.iter().enumerate() {
                    let view = view.clone();
                    let path = path.clone();
                    list = list.child(
                        Button::new(SharedString::from(format!("recent-{i}")))
                            .ghost()
                            .w_full()
                            .label(label.clone())
                            .on_click(move |_, window, cx| {
                                let path = path.clone();
                                view.update(cx, |this, cx| {
                                    this.open_menu_open = false;
                                    this.open_recent(path, window, cx);
                                });
                            }),
                    );
                }
                list
            })
    }

    /// A slim bar under the title bar: campaign name + file path (left), and a
    /// saved-state pill (right).
    fn render_context_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dirty = self.is_dirty(cx);
        let path = self
            .project_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let (dot, label) = if dirty {
            (cx.theme().warning, tr("top.dirty"))
        } else {
            (cx.theme().success, tr("top.saved"))
        };
        h_flex()
            .w_full()
            .items_center()
            .gap(px(11.))
            .h(px(40.))
            .px(px(18.))
            .flex_shrink_0()
            .bg(cx.theme().title_bar)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.project_name.clone()),
            )
            .when_some(path, |this, p| {
                this.child(
                    div()
                        .font_family(theme::MONO_FONT)
                        .text_size(px(11.5))
                        .text_color(cx.tokens().text_faint)
                        .truncate()
                        .child(p),
                )
            })
            .child(div().flex_1())
            .when_some(self.project_notice.clone(), |this, notice| {
                let color = if notice.ok {
                    cx.theme().muted_foreground
                } else {
                    cx.theme().danger
                };
                this.child(
                    div()
                        .text_size(px(12.))
                        .text_color(color)
                        .child(notice.text),
                )
            })
            .child(
                h_flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().size(px(7.)).rounded_full().bg(dot))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(label),
                    ),
            )
    }
}

/// Resolve a [`ThemePref`] to a concrete light/dark mode and apply it. `Auto`
/// maps the OS window appearance.
fn apply_theme_mode(pref: ThemePref, window: &mut Window, cx: &mut App) {
    let mode = match pref {
        ThemePref::Light => ThemeMode::Light,
        ThemePref::Dark => ThemeMode::Dark,
        ThemePref::Auto => ThemeMode::from(window.appearance()),
    };
    Theme::change(mode, Some(window), cx);
    // `Theme::change` re-applies its own light/dark config; layer our palette on
    // top so every widget picks up the redesign tokens.
    theme::apply_palette(mode, cx);
}

/// The campaign base name from a project path: strips the `.mmproj.toml`
/// suffix, else falls back to the file stem.
fn base_name(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("campaign");
    if let Some(stripped) = name.strip_suffix(PROJECT_SUFFIX) {
        stripped.to_string()
    } else {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("campaign")
            .to_string()
    }
}

/// Resolve a stored path against the project directory (absolute paths as-is).
fn resolve(dir: &Path, stored: &str) -> PathBuf {
    let p = Path::new(stored);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        dir.join(p)
    }
}

/// Store `file` relative to `dir` when they share a directory, else absolute —
/// so projects kept next to their data stay portable.
fn relative_or_absolute(dir: &Path, file: &Path) -> String {
    match file.file_name() {
        Some(name) if file.parent() == Some(dir) => name.to_string_lossy().to_string(),
        _ => file.to_string_lossy().to_string(),
    }
}

/// A muted caption from an i18n key.
fn field_label(key: &'static str, cx: &Context<MainWindow>) -> impl IntoElement {
    div()
        .text_size(px(13.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground)
        .child(tr(key))
}

/// A card surface: surface fill, hairline border, large radius, soft shadow.
fn card(cx: &Context<MainWindow>) -> Div {
    div()
        .bg(cx.tokens().surface)
        .border_1()
        .border_color(cx.theme().border)
        .rounded(px(14.))
        .shadow(cx.tokens().card_shadow())
}

/// A rounded `surface_2` pill rendering a technical string (filename, token).
fn mono_chip(text: impl Into<SharedString>, cx: &Context<MainWindow>) -> Div {
    div()
        .px(px(11.))
        .py(px(6.))
        .rounded(px(8.))
        .bg(cx.tokens().surface_2)
        .border_1()
        .border_color(cx.theme().border)
        .font_family(theme::MONO_FONT)
        .text_size(px(13.))
        .child(text.into())
}

/// Style a button as one option of a segmented control: selected = filled
/// accent, unselected = muted `surface_2`.
fn seg(button: Button, selected: bool) -> Button {
    let button = button.small();
    // Unselected keeps the default `Secondary` variant (muted `surface_2`).
    if selected { button.primary() } else { button }
}

/// A 34px bordered icon button (custom SVG). `danger` turns it red on hover.
/// The caller attaches `.on_click(...)`.
fn icon_btn(
    id: impl Into<ElementId>,
    icon_path: &'static str,
    danger: bool,
    cx: &Context<MainWindow>,
) -> Stateful<Div> {
    let hover_fg = if danger {
        cx.theme().danger
    } else {
        cx.theme().foreground
    };
    let hover_bg = cx.tokens().surface_2;
    div()
        .id(id.into())
        .flex()
        .items_center()
        .justify_center()
        .size(px(34.))
        .flex_shrink_0()
        .rounded(px(8.))
        .border_1()
        .border_color(cx.theme().border)
        .text_color(cx.theme().muted_foreground)
        .cursor_pointer()
        .hover(move |h| h.bg(hover_bg).text_color(hover_fg).border_color(hover_fg))
        .child(Icon::empty().path(icon_path).with_size(px(16.)))
}

/// A labelled text field; `label_key` is an i18n key.
fn labeled(
    label_key: &'static str,
    input: &Entity<InputState>,
    cx: &Context<MainWindow>,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(field_label(label_key, cx))
        .child(Input::new(input))
}

/// A statistic: a big coloured count over/with a muted label; `label_key` is i18n.
fn stat(label_key: &'static str, value: usize, color: Hsla) -> impl IntoElement {
    h_flex()
        .gap(px(6.))
        .items_baseline()
        .child(
            div()
                .text_size(px(18.))
                .font_weight(FontWeight::BOLD)
                .text_color(color)
                .child(value.to_string()),
        )
        .child(div().text_size(px(13.)).child(tr(label_key)))
}

fn status_style(status: &RowStatus, cx: &App) -> (Hsla, SharedString) {
    let theme = cx.theme();
    match status {
        RowStatus::Ok => (theme.success, tr("status.ok").into()),
        RowStatus::InvalidEmail => (theme.danger, tr("status.bad_email").into()),
        RowStatus::Duplicate => (theme.warning, tr("status.duplicate").into()),
        RowStatus::MissingFields(_) => (theme.danger, tr("status.missing").into()),
    }
}

fn outcome_style(status: &OutcomeStatus, cx: &App) -> (Hsla, SharedString) {
    let theme = cx.theme();
    match status {
        OutcomeStatus::Sent => (theme.success, tr("outcome.sent").into()),
        OutcomeStatus::Failed => (theme.danger, tr("outcome.failed").into()),
        OutcomeStatus::Skipped => (theme.warning, tr("outcome.skipped").into()),
    }
}

/// A "Label … value" line for the pre-flight summary card (value in mono, right
/// aligned, with a hairline divider); `label_key` is i18n.
fn summary_row(
    label_key: &'static str,
    value: String,
    cx: &Context<MainWindow>,
) -> impl IntoElement {
    h_flex()
        .items_center()
        .gap(px(12.))
        .px(px(20.))
        .py(px(14.))
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .flex_1()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(tr(label_key)),
        )
        .child(
            div()
                .font_family(theme::MONO_FONT)
                .text_size(px(14.))
                .max_w(px(380.))
                .truncate()
                .child(value),
        )
}

/// Human-readable duration like "45s", "3m 20s", "1h 5m".
fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn kind_button(
    id: &'static str,
    label_key: &'static str,
    value: ProviderKind,
    current: ProviderKind,
    cx: &mut Context<MainWindow>,
) -> Button {
    seg(
        Button::new(id)
            .label(tr(label_key))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.form.kind = value;
                this.notice = None;
                cx.notify();
            })),
        value == current,
    )
}

fn tls_button(
    id: &'static str,
    label_key: &'static str,
    value: TlsMode,
    current: TlsMode,
    cx: &mut Context<MainWindow>,
) -> Button {
    seg(
        Button::new(id)
            .label(tr(label_key))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.form.tls = value;
                cx.notify();
            })),
        value == current,
    )
}

fn region_button(
    id: &'static str,
    label_key: &'static str,
    value: MailgunRegion,
    current: MailgunRegion,
    cx: &mut Context<MainWindow>,
) -> Button {
    seg(
        Button::new(id)
            .label(tr(label_key))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.form.region = value;
                cx.notify();
            })),
        value == current,
    )
}

impl Render for MainWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_title_bar(cx))
            .child(self.render_context_bar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.render_sidebar(cx))
                    .child(self.render_content(window, cx)),
            )
    }
}
