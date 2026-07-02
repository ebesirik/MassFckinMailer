use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme, Disableable as _, Icon, IconName, Selectable as _, h_flex,
    button::{Button, ButtonVariants as _},
    input::{Input, InputState},
    progress::Progress,
    sidebar::{Sidebar, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem},
    text::TextView,
    v_flex,
};
use mmm_core::import::{self, RecipientTable, SourceKind};
use mmm_core::mapping::{self, RowStatus, ValidationReport};
use mmm_core::project::{
    AccountRef, CURRENT_VERSION, PROJECT_SUFFIX, Project, RecentProjects, RecipientSource,
    SendingConfig, TemplateSpec,
};
use mmm_core::template::{self, extract_placeholders, normalize_placeholders};
use mmm_engine::{
    CampaignPlan, CampaignProgress, CampaignRecipient, CampaignState, CampaignSummary, Command,
    Event, MailRuntime, OutcomeStatus, RowOutcome,
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

    fn label(&self) -> &'static str {
        match self {
            Self::Accounts => "Accounts",
            Self::Template => "Template",
            Self::Recipients => "Recipients",
            Self::Send => "Send",
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

    fn hint(&self) -> &'static str {
        match self {
            Self::Accounts => {
                "Add a sending account first — SMTP, Mailgun, AWS SES, or Gmail/Outlook. \
                 Secrets are stored in your OS keychain, never in project files."
            }
            Self::Template => {
                "Write your email with placeholders like {{first_name}} (##first_name## works too). \
                 The subject line is a template as well."
            }
            Self::Recipients => {
                "Drop a CSV or Excel file. The email column is detected automatically, \
                 and you map the remaining columns to template fields."
            }
            Self::Send => {
                "Review the pre-flight summary, then send. You can cancel at any time — \
                 emails already delivered stay delivered."
            }
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
        }
    }
}

/// A deferred action that must be confirmed if there are unsaved changes.
enum PendingAction {
    New,
    OpenDialog,
    OpenPath(PathBuf),
}

// ---- Account form (M1) --------------------------------------------------

/// Text inputs and selections for the add-account form. The `InputState`
/// entities must be owned by the view so they stay alive across renders.
struct AccountForm {
    open: bool,
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
            .filter_map(|(field, col)| {
                self.table.column_index(col).map(|idx| (field.clone(), idx))
            })
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

    // M5 — project files
    project_path: Option<PathBuf>,
    project_name: String,
    saved_snapshot: ProjectSnapshot,
    recents: RecentProjects,
    project_notice: Option<Notice>,
}

fn read_trimmed(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().trim().to_string()
}

impl MainWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mail = MailRuntime::start();

        // Event pump: await engine events on the foreground executor and
        // update this entity. flume's recv_async is runtime-agnostic, so no
        // tokio is needed on this side.
        let events = mail.events();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv_async().await {
                let alive = this
                    .update(cx, |this, cx| {
                        this.on_engine_event(event);
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
            project_path: None,
            project_name: "Untitled campaign".into(),
            saved_snapshot: ProjectSnapshot::fresh(),
            recents: RecentProjects::load(),
            project_notice: None,
        }
    }

    fn on_engine_event(&mut self, event: Event) {
        match event {
            Event::TestResult { ok, message, .. } => {
                self.testing = false;
                self.notice = Some(Notice { ok, text: message });
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
                    }
                    self.notice = Some(Notice { ok: true, text: message });
                } else {
                    self.pending_oauth = None;
                    self.notice = Some(Notice { ok: false, text: message });
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
                    return Err("SMTP host is required.".into());
                }
                let port: u16 = port
                    .parse()
                    .map_err(|_| "Port must be a number between 1 and 65535.".to_string())?;
                if from.is_empty() {
                    return Err("A From address is required.".into());
                }
                if password.is_empty() {
                    return Err("A password is required.".into());
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
                    return Err("Mailgun sending domain is required.".into());
                }
                if from.is_empty() {
                    return Err("A From address is required.".into());
                }
                if api_key.is_empty() {
                    return Err("A Mailgun API key is required.".into());
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
                    return Err("AWS region is required.".into());
                }
                if from.is_empty() {
                    return Err("A verified From address is required.".into());
                }
                if key_id.is_empty() || secret.is_empty() {
                    return Err("Both the access key id and secret access key are required.".into());
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
                let client_secret = self.form.oauth_client_secret.read(cx).value().trim().to_string();
                let from = read_trimmed(&self.form.oauth_from, cx);
                if client_id.is_empty() {
                    return Err("OAuth client ID is required.".into());
                }
                if from.is_empty() {
                    return Err("Your Gmail address (From) is required.".into());
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
                    return Err("Application (client) ID is required.".into());
                }
                if from.is_empty() {
                    return Err("Your address (From) is required.".into());
                }
                if display.is_empty() {
                    display = format!("Outlook — {from}");
                }
                let tenant = if tenant.is_empty() { "common".to_string() } else { tenant };
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
                    text: "Testing connection…".into(),
                });
                self.mail.command(Command::TestAccount { account, secret });
            }
            Err(text) => self.notice = Some(Notice { ok: false, text }),
        }
        cx.notify();
    }

    fn on_connect_oauth(&mut self, cx: &mut Context<Self>) {
        match self.form_to_account(new_account_id(), cx) {
            Ok((account, client_secret)) => {
                self.pending_oauth = Some(account.clone());
                self.testing = true;
                self.notice = Some(Notice {
                    ok: true,
                    text: "Opening your browser to authorize… complete sign-in there.".into(),
                });
                self.mail
                    .command(Command::ConnectOAuth { account, client_secret });
            }
            Err(text) => self.notice = Some(Notice { ok: false, text }),
        }
        cx.notify();
    }

    fn on_save_account(&mut self, cx: &mut Context<Self>) {
        let id = new_account_id();
        match self.form_to_account(id.clone(), cx) {
            Ok((account, secret)) => {
                if let Err(e) = secrets::set(&account.id, &secret) {
                    self.notice = Some(Notice {
                        ok: false,
                        text: format!("Could not store secret in keychain: {e}"),
                    });
                    cx.notify();
                    return;
                }
                self.store.upsert(account);
                if let Err(e) = self.store.save() {
                    self.notice = Some(Notice {
                        ok: false,
                        text: format!("Account saved to keychain but writing accounts file failed: {e}"),
                    });
                    cx.notify();
                    return;
                }
                self.form.open = false;
                self.notice = Some(Notice {
                    ok: true,
                    text: "Account saved.".into(),
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
            prompt: Some("Choose recipient list".into()),
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

            let _ = this
                .update(cx, |this, cx| this.on_parsed(path, output, preset, from_load, cx));
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
                        let col = table.column_index(email_column).unwrap_or_else(|| {
                            import::detect_email_column_in(&table).unwrap_or(0)
                        });
                        (col, mapping.clone())
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

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Sidebar::left()
            .w(px(220.))
            .header(
                SidebarHeader::new()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size_8()
                            .flex_shrink_0()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().primary)
                            .text_color(cx.theme().primary_foreground)
                            .child(Icon::new(IconName::Inbox)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .overflow_hidden()
                            .child("MassFckinMailer")
                            .child(div().text_xs().child("Campaign setup")),
                    ),
            )
            .child(
                SidebarGroup::new("Steps").child(SidebarMenu::new().children(
                    Section::ALL.map(|section| {
                        SidebarMenuItem::new(section.label())
                            .icon(section.icon())
                            .active(self.active == section)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.active = section;
                                cx.notify();
                            }))
                    }),
                )),
            )
    }

    fn render_content(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let section = self.active;
        v_flex()
            .flex_1()
            .gap_4()
            .p_8()
            .child(div().text_xl().child(section.label()))
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .max_w(px(620.))
                    .child(section.hint()),
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
            })
    }

    // ---- Accounts UI ----------------------------------------------------

    fn render_accounts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_4()
            .max_w(px(620.))
            .child(self.render_account_list(cx))
            .when(!self.form.open, |this| {
                this.child(
                    h_flex().child(
                        Button::new("add-account")
                            .primary()
                            .icon(IconName::Plus)
                            .label("Add account")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.form.open = true;
                                this.notice = None;
                                cx.notify();
                            })),
                    ),
                )
            })
            .when(self.form.open, |this| this.child(self.render_account_form(cx)))
    }

    fn render_account_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.store.accounts.is_empty() {
            return v_flex().child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("No accounts yet. Add one below to start."),
            );
        }

        let rows = self.store.accounts.iter().map(|account| {
            let id = account.id.clone();
            let detail = match &account.config {
                AccountConfig::Smtp(c) => format!("SMTP · {}:{}", c.host, c.port),
                AccountConfig::Mailgun(c) => format!("Mailgun · {}", c.domain),
                AccountConfig::Ses(c) => format!("AWS SES · {}", c.region),
                AccountConfig::Gmail(c) => format!("Gmail · {}", c.from),
                AccountConfig::Outlook(c) => format!("Outlook · {}", c.from),
            };
            h_flex()
                .items_center()
                .justify_between()
                .gap_3()
                .p_3()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().border)
                .child(
                    v_flex()
                        .gap_1()
                        .child(div().text_sm().child(account.display.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(detail),
                        ),
                )
                .child(
                    Button::new(SharedString::from(format!("del-{id}")))
                        .ghost()
                        .icon(IconName::Delete)
                        .tooltip("Remove account")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.on_delete_account(id.clone(), cx);
                        })),
                )
        });

        v_flex().gap_2().children(rows)
    }

    fn render_account_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let kind = self.form.kind;
        let is_oauth = matches!(kind, ProviderKind::Gmail | ProviderKind::Outlook);

        v_flex()
            .gap_4()
            .p_4()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .child(div().text_sm().child("New account"))
            .child(
                v_flex()
                    .gap_1()
                    .child(field_label("Provider", cx))
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .child(kind_button("k-smtp", "Generic SMTP", ProviderKind::Smtp, kind, cx))
                            .child(kind_button("k-mailgun", "Mailgun", ProviderKind::Mailgun, kind, cx))
                            .child(kind_button("k-ses", "AWS SES", ProviderKind::Ses, kind, cx))
                            .child(kind_button("k-gmail", "Gmail", ProviderKind::Gmail, kind, cx))
                            .child(kind_button("k-outlook", "Outlook", ProviderKind::Outlook, kind, cx)),
                    ),
            )
            .child(labeled("Display name (optional)", &self.form.display, cx))
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
                                .label("Test connection")
                                .disabled(self.testing)
                                .on_click(cx.listener(|this, _, _, cx| this.on_test_connection(cx))),
                        )
                        .child(
                            Button::new("save-account")
                                .primary()
                                .label("Save account")
                                .disabled(self.testing)
                                .on_click(cx.listener(|this, _, _, cx| this.on_save_account(cx))),
                        )
                    })
                    .when(is_oauth, |this| {
                        this.child(
                            Button::new("connect-oauth")
                                .primary()
                                .label("Connect & authorize")
                                .disabled(self.testing)
                                .on_click(cx.listener(|this, _, _, cx| this.on_connect_oauth(cx))),
                        )
                    })
                    .child(
                        Button::new("cancel-account")
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.form.open = false;
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
                    .child(div().flex_1().child(labeled("Host", &self.form.smtp_host, cx)))
                    .child(
                        div()
                            .w(px(120.))
                            .child(labeled("Port", &self.form.smtp_port, cx)),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(field_label("Encryption", cx))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(tls_button("tls-starttls", "STARTTLS", TlsMode::StartTls, tls, cx))
                            .child(tls_button("tls-tls", "SSL/TLS", TlsMode::Tls, tls, cx))
                            .child(tls_button("tls-none", "None", TlsMode::None, tls, cx)),
                    ),
            )
            .child(labeled("Username", &self.form.smtp_username, cx))
            .child(labeled("From address", &self.form.smtp_from, cx))
            .child(labeled("Password", &self.form.smtp_password, cx))
    }

    fn render_mailgun_fields(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let region = self.form.region;
        v_flex()
            .gap_3()
            .child(labeled("Sending domain", &self.form.mg_domain, cx))
            .child(
                v_flex()
                    .gap_1()
                    .child(field_label("Region", cx))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(region_button("rg-us", "US", MailgunRegion::Us, region, cx))
                            .child(region_button("rg-eu", "EU", MailgunRegion::Eu, region, cx)),
                    ),
            )
            .child(labeled("From address", &self.form.mg_from, cx))
            .child(labeled("API key", &self.form.mg_api_key, cx))
    }

    fn render_ses_fields(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .w(px(160.))
                            .child(labeled("Region", &self.form.ses_region, cx)),
                    )
                    .child(div().flex_1().child(labeled("From address", &self.form.ses_from, cx))),
            )
            .child(labeled("Access key id", &self.form.ses_key_id, cx))
            .child(labeled("Secret access key", &self.form.ses_secret, cx))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "The From address must be a verified SES identity. New accounts are in \
                         the sandbox — recipients must also be verified until you request production access.",
                    ),
            )
    }

    fn render_gmail_fields(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .child(labeled("From (your Gmail address)", &self.form.oauth_from, cx))
            .child(labeled("OAuth client ID", &self.form.oauth_client_id, cx))
            .child(labeled("Client secret", &self.form.oauth_client_secret, cx))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "In Google Cloud: enable the Gmail API, create an OAuth client of type \
                         \"Desktop app\", and paste its client ID + secret here. \"Connect\" opens \
                         your browser to grant the gmail.send scope; personal Gmail caps at ~500/day.",
                    ),
            )
    }

    fn render_outlook_fields(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .child(labeled("From (your address)", &self.form.oauth_from, cx))
            .child(labeled("Application (client) ID", &self.form.oauth_client_id, cx))
            .child(labeled("Tenant (common / organizations / id)", &self.form.oauth_tenant, cx))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "In Azure: register an app, add the delegated Mail.Send permission, and \
                         under \"Mobile and desktop applications\" add redirect URI http://localhost. \
                         No client secret is needed (public client + PKCE). \"Connect\" opens your browser.",
                    ),
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
            context.entry(field.clone()).or_insert_with(|| format!("[{field}]"));
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
            .gap_4()
            .size_full()
            .child(labeled("Subject", &self.template.subject, cx))
            .child(self.render_placeholder_chips(cx))
            .child(
                h_flex()
                    .gap_4()
                    .w_full()
                    .items_start()
                    .child(
                        v_flex()
                            .gap_1()
                            .flex_1()
                            .child(field_label("Body (HTML)", cx))
                            .child(
                                div()
                                    .h(px(440.))
                                    .child(Input::new(&self.template.body).h_full()),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .flex_1()
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
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(
                    "Load a recipient list in the Recipients step to insert placeholder chips. \
                     You can still type {{field}} manually.",
                )
                .into_any_element();
        }

        let mut row = h_flex()
            .gap_2()
            .flex_wrap()
            .items_center()
            .child(field_label("Insert field", cx));
        for (i, header) in headers.iter().enumerate() {
            let insert = format!("{{{{{}}}}}", template::to_placeholder_ident(header));
            row = row.child(
                Button::new(SharedString::from(format!("chip-{i}")))
                    .label(header.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let text = insert.clone();
                        this.template
                            .body
                            .update(cx, |state, cx| state.insert(text, window, cx));
                        cx.notify();
                    })),
            );
        }
        row.into_any_element()
    }

    fn render_preview_nav(&self, cx: &mut Context<Self>) -> AnyElement {
        let total = self.loaded_row_count();
        if total == 0 {
            return h_flex()
                .gap_2()
                .items_center()
                .child(field_label("Preview", cx))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("(no data loaded — showing field names)"),
                )
                .into_any_element();
        }
        let current = self.preview_row.min(total - 1) + 1;
        h_flex()
            .gap_2()
            .items_center()
            .child(field_label("Preview", cx))
            .child(
                Button::new("prev-row")
                    .ghost()
                    .label("‹ Prev")
                    .disabled(self.preview_row == 0)
                    .on_click(cx.listener(|this, _, _, cx| this.preview_prev(cx))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("Row {current} of {total}")),
            )
            .child(
                Button::new("next-row")
                    .ghost()
                    .label("Next ›")
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
                .child(format!("Body error: {e}"))
                .into_any_element(),
        };

        v_flex()
            .h(px(440.))
            .gap_2()
            .p_3()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .overflow_hidden()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .truncate()
                    .child(format!("Subject: {subject}")),
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
                .child("Parsing file…")
                .into_any_element(),
            RecipientsState::Error(message) => v_flex()
                .gap_3()
                .child(div().text_color(cx.theme().danger).child(message.clone()))
                .child(self.choose_file_button("retry-file", "Choose another file", cx))
                .into_any_element(),
            RecipientsState::Loaded(loaded) => self.render_loaded(loaded, cx).into_any_element(),
        }
    }

    fn choose_file_button(
        &self,
        id: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .primary()
            .icon(IconName::Inbox)
            .label(label)
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
                    .child("Import a CSV or Excel (.xlsx/.xls/.ods) file of recipients."),
            )
            .child(self.choose_file_button("choose-file", "Choose CSV or Excel file", cx))
    }

    fn render_loaded(&self, loaded: &Loaded, cx: &mut Context<Self>) -> impl IntoElement {
        let file_name = loaded
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let summary = format!(
            "{} · {} rows · {} columns",
            file_name,
            loaded.table.row_count(),
            loaded.table.column_count()
        );

        v_flex()
            .gap_4()
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(div().text_sm().child(summary))
                    .child(self.choose_file_button("change-file", "Change file", cx)),
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
            chips.push(
                Button::new(SharedString::from(format!("sheet-{i}")))
                    .label(name.clone())
                    .selected(selected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.on_select_sheet(value.clone(), cx)
                    })),
            );
        }
        v_flex()
            .gap_1()
            .child(field_label("Sheet", cx))
            .child(h_flex().gap_2().flex_wrap().children(chips))
    }

    fn render_email_picker(&self, loaded: &Loaded, cx: &mut Context<Self>) -> impl IntoElement {
        let email_col = loaded.email_col;
        let mut chips = Vec::new();
        for (i, header) in loaded.table.headers.iter().enumerate() {
            chips.push(
                Button::new(SharedString::from(format!("email-col-{i}")))
                    .label(header.clone())
                    .selected(i == email_col)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_email_col(i, cx))),
            );
        }
        v_flex()
            .gap_1()
            .child(field_label("Email column", cx))
            .child(h_flex().gap_2().flex_wrap().children(chips))
    }

    fn render_mapping(&self, loaded: &Loaded, cx: &mut Context<Self>) -> AnyElement {
        if loaded.fields.is_empty() {
            return v_flex()
                .gap_2()
                .child(field_label("Field mapping", cx))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            "No template fields yet. Add placeholders like {{first_name}} in the \
                             Template step, then refresh.",
                        ),
                )
                .child(
                    Button::new("refresh-fields")
                        .outline()
                        .label("Refresh from template")
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_fields(cx))),
                )
                .into_any_element();
        }

        let mut list = v_flex().gap_3().child(field_label("Field mapping", cx));
        for field in &loaded.fields {
            let selected = loaded.mapping.get(field).cloned();
            let field_name = field.clone();

            let none_field = field_name.clone();
            let mut chips = h_flex().gap_2().flex_wrap().child(
                Button::new(SharedString::from(format!("map-{field}-none")))
                    .label("— none —")
                    .selected(selected.is_none())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_mapping(none_field.clone(), None, cx)
                    })),
            );
            for (i, header) in loaded.table.headers.iter().enumerate() {
                let is_selected = selected.as_deref() == Some(header.as_str());
                let col = header.clone();
                let field_for_col = field_name.clone();
                chips = chips.child(
                    Button::new(SharedString::from(format!("map-{field}-{i}")))
                        .label(header.clone())
                        .selected(is_selected)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_mapping(field_for_col.clone(), Some(col.clone()), cx)
                        })),
                );
            }

            list = list.child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .child(SharedString::from(format!("{{{{{field}}}}}"))),
                    )
                    .child(chips),
            );
        }

        list.child(
            Button::new("refresh-fields")
                .ghost()
                .label("Refresh from template")
                .on_click(cx.listener(|this, _, _, cx| this.refresh_fields(cx))),
        )
        .into_any_element()
    }

    fn render_validation_summary(&self, loaded: &Loaded, cx: &mut Context<Self>) -> impl IntoElement {
        let report = &loaded.report;
        let theme = cx.theme();

        h_flex()
            .items_center()
            .gap_5()
            .flex_wrap()
            .child(stat("Will send", loaded.sendable(), theme.success))
            .child(stat("Bad email", report.invalid_email, theme.danger))
            .child(stat("Duplicates", report.duplicates, theme.warning))
            .child(stat("Missing data", report.missing_fields, theme.danger))
            .child(
                Button::new("toggle-dedupe")
                    .outline()
                    .selected(loaded.dedupe)
                    .label(if loaded.dedupe {
                        "De-duplicate: on"
                    } else {
                        "De-duplicate: off"
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_dedupe(cx))),
            )
    }

    fn render_preview(&self, loaded: &Loaded, cx: &mut Context<Self>) -> impl IntoElement {
        // Choose a bounded set of columns: email + mapped fields, or the first
        // few columns when nothing is mapped yet.
        let email_col = loaded.email_col;
        let mut columns: Vec<(String, usize)> = vec![("Email".to_string(), email_col)];
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

        let header_row = h_flex()
            .w_full()
            .px_2()
            .py_1p5()
            .gap_2()
            .bg(cx.theme().secondary)
            .child(div().w(px(96.)).text_xs().child("Status"))
            .children(columns.iter().map(|(name, _)| {
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .truncate()
                    .child(name.clone())
            }));

        let table = loaded.table.clone();
        let report = loaded.report.clone();
        let row_columns = columns.clone();
        let border = cx.theme().border;
        let count = table.row_count();

        let list = uniform_list(
            "recipients-preview",
            count,
            move |range, _window, cx| {
                let mut items = Vec::with_capacity(range.end - range.start);
                for ix in range {
                    let (color, label) = status_style(&report.statuses[ix], cx);
                    let row = &table.rows[ix];
                    let cells = row_columns.iter().map(|(_, idx)| {
                        div()
                            .flex_1()
                            .text_sm()
                            .truncate()
                            .child(row.get(*idx).cloned().unwrap_or_default())
                    });
                    items.push(
                        h_flex()
                            .w_full()
                            .px_2()
                            .py_1()
                            .gap_2()
                            .border_b_1()
                            .border_color(border)
                            .child(div().w(px(96.)).text_xs().text_color(color).child(label))
                            .children(cells),
                    );
                }
                items
            },
        )
        .h(px(320.));

        v_flex()
            .rounded(cx.theme().radius)
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

    /// Count of recipients that will actually be sent (valid rows, plus
    /// duplicates when de-dupe is off).
    fn sendable_count(&self) -> usize {
        match &self.recipients {
            RecipientsState::Loaded(l) => l.sendable(),
            _ => 0,
        }
    }

    /// Assemble a [`CampaignPlan`] from the current account, template, and the
    /// sendable recipient rows.
    fn build_campaign_plan(&self, cx: &App) -> Result<CampaignPlan, String> {
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

        let loaded = match &self.recipients {
            RecipientsState::Loaded(l) => l,
            _ => return Err("Load a recipient list first (Recipients step).".into()),
        };

        let email_col = loaded.email_col;
        let mut recipients = Vec::new();
        for (i, status) in loaded.report.statuses.iter().enumerate() {
            let include = match status {
                RowStatus::Ok => true,
                RowStatus::Duplicate => !loaded.dedupe,
                _ => false,
            };
            if !include {
                continue;
            }
            let row = &loaded.table.rows[i];
            recipients.push(CampaignRecipient {
                index: i,
                email: row.get(email_col).cloned().unwrap_or_default(),
                context: mapping::build_context(&loaded.table, row, &loaded.mapping),
            });
        }
        if recipients.is_empty() {
            return Err("No valid recipients to send to.".into());
        }

        let subject = self.template.subject.read(cx).value().to_string();
        let body = self.template.body.read(cx).value().to_string();
        if subject.trim().is_empty() && body.trim().is_empty() {
            return Err("Write a subject or body in the Template step.".into());
        }

        let cfg = SendingConfig::default();
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
            chips.push(
                Button::new(SharedString::from(format!("send-acct-{id}")))
                    .label(account.display.clone())
                    .selected(selected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_account = Some(id.clone());
                        cx.notify();
                    })),
            );
        }
        v_flex()
            .gap_1()
            .child(field_label("Sending account", cx))
            .child(h_flex().gap_2().flex_wrap().children(chips))
    }

    fn render_preflight(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let account_label = self
            .active_account_id()
            .and_then(|id| self.store.get(&id).map(|a| a.display.clone()))
            .unwrap_or_else(|| "— none —".to_string());
        let subject = self.template.subject.read(cx).value().to_string();
        let subject = if subject.trim().is_empty() {
            "(empty)".to_string()
        } else {
            subject
        };
        let sendable = self.sendable_count();
        let mps = SendingConfig::default().messages_per_second.max(1.0);
        let eta_secs = (sendable as f32 / mps).ceil() as u64;

        let mut warnings: Vec<String> = Vec::new();
        if self.store.accounts.is_empty() {
            warnings.push("No sending account — add one in the Accounts step.".into());
        }
        match &self.recipients {
            RecipientsState::Loaded(l) => {
                if sendable == 0 {
                    warnings.push("No valid recipients to send to.".into());
                }
                let flagged = l.report.invalid_email + l.report.missing_fields;
                if flagged > 0 {
                    warnings.push(format!("{flagged} rows will be skipped (bad email or missing data)."));
                }
                if l.dedupe && l.report.duplicates > 0 {
                    warnings.push(format!(
                        "{} duplicate rows will be skipped (de-dupe is on).",
                        l.report.duplicates
                    ));
                }
            }
            _ => warnings.push("No recipient list loaded — add one in the Recipients step.".into()),
        }

        let can_send = !self.store.accounts.is_empty() && sendable > 0;

        v_flex()
            .gap_4()
            .max_w(px(620.))
            .child(self.render_account_picker(cx))
            .child(
                v_flex()
                    .gap_2()
                    .p_4()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(summary_row("Account", account_label, cx))
                    .child(summary_row("Subject", subject, cx))
                    .child(summary_row(
                        "Recipients",
                        format!("{sendable} will be sent"),
                        cx,
                    ))
                    .child(summary_row(
                        "Est. duration",
                        format!("~{}", format_duration(eta_secs)),
                        cx,
                    )),
            )
            .when(!warnings.is_empty(), |this| {
                this.child(
                    v_flex().gap_1().children(warnings.into_iter().map(|w| {
                        div()
                            .text_sm()
                            .text_color(cx.theme().warning)
                            .child(format!("⚠ {w}"))
                    })),
                )
            })
            .when_some(self.send_notice.clone(), |this, notice| {
                let color = if notice.ok {
                    cx.theme().success
                } else {
                    cx.theme().danger
                };
                this.child(div().text_sm().text_color(color).child(notice.text))
            })
            .child(
                Button::new("send-campaign")
                    .primary()
                    .icon(IconName::ArrowRight)
                    .label(format!("Send {sendable} emails"))
                    .disabled(!can_send)
                    .on_click(cx.listener(|this, _, _, cx| this.on_send(cx))),
            )
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
            .map(|s| format!("~{} left", format_duration(s)))
            .unwrap_or_else(|| "estimating…".to_string());

        v_flex()
            .gap_3()
            .max_w(px(720.))
            .child(Progress::new().value(percentage))
            .child(
                h_flex()
                    .gap_5()
                    .flex_wrap()
                    .child(stat("Sent", p.sent, cx.theme().success))
                    .child(stat("Failed", p.failed, cx.theme().danger))
                    .child(stat("Skipped", p.skipped, cx.theme().warning))
                    .child(stat("Total", p.total, cx.theme().foreground))
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
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.on_cancel_send(cx))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Cancel stops upcoming emails; those already delivered stay delivered."),
                    ),
            )
    }

    fn render_finished(&self, summary: &CampaignSummary, cx: &mut Context<Self>) -> impl IntoElement {
        let (headline, color) = match &summary.state {
            CampaignState::Completed => ("Campaign complete".to_string(), cx.theme().success),
            CampaignState::Cancelled => ("Campaign cancelled".to_string(), cx.theme().warning),
            CampaignState::Stopped(reason) => (format!("Stopped — {reason}"), cx.theme().danger),
            CampaignState::Running => ("Finishing…".to_string(), cx.theme().foreground),
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
                    .child(stat("Sent", summary.sent, cx.theme().success))
                    .child(stat("Failed", summary.failed, cx.theme().danger))
                    .child(stat("Skipped", summary.skipped, cx.theme().warning))
                    .child(stat("Total", summary.total, cx.theme().foreground))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("in {}", format_duration(summary.elapsed_secs))),
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
                            .label("Export report (CSV)")
                            .on_click(cx.listener(|this, _, _, cx| this.on_export_report(cx))),
                    )
                    .child(
                        Button::new("send-again")
                            .ghost()
                            .label("Back to pre-flight")
                            .on_click(cx.listener(|this, _, _, cx| this.start_again(cx))),
                    ),
            )
    }

    fn render_recent_rows(&self, recent: &[RowOutcome], cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child("Recent activity"),
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
        let answer = window.prompt(
            PromptLevel::Warning,
            "Discard unsaved changes?",
            Some("Your current campaign has changes that haven't been saved."),
            &["Discard changes", "Cancel"],
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
        self.selected_account = None;
        self.recipients = RecipientsState::Empty;
        self.preview_row = 0;
        self.sending = false;
        self.progress = None;
        self.summary = None;
        self.send_notice = None;
        self.project_path = None;
        self.project_name = "Untitled campaign".into();
        self.project_notice = None;
        self.mark_saved(cx);
        cx.notify();
    }

    fn open_dialog(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open campaign project".into()),
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
                    text: format!("Could not open project: {e}"),
                });
                cx.notify();
                return;
            }
        };
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();

        // Subject + body (body lives in the sibling HTML file).
        self.template
            .subject
            .update(cx, |s, cx| s.set_value(project.template.subject.clone(), window, cx));
        let html_full = resolve(&dir, &project.template.html_path);
        let body = std::fs::read_to_string(&html_full).unwrap_or_default();
        self.template
            .body
            .update(cx, |s, cx| s.set_value(body, window, cx));

        self.selected_account = project.account.as_ref().map(|a| a.id.clone());
        self.project_path = Some(path.clone());
        self.project_name = project.name.clone();
        self.sending = false;
        self.progress = None;
        self.summary = None;
        self.send_notice = None;
        self.project_notice = Some(Notice {
            ok: true,
            text: format!("Opened {}", path.display()),
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
                text: format!("Could not write template file: {e}"),
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
                email_column: l.table.headers.get(l.email_col).cloned().unwrap_or_default(),
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
            sending: SendingConfig::default(),
        };

        if let Err(e) = project.save(&path) {
            self.project_notice = Some(Notice {
                ok: false,
                text: format!("Could not save project: {e}"),
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
            text: "Project saved.".into(),
        });
        cx.notify();
    }

    fn render_topbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dirty = self.is_dirty(cx);
        v_flex()
            .w_full()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_2()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_sm().child(self.project_name.clone()))
                            .when(dirty, |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().warning)
                                        .child("• unsaved"),
                                )
                            })
                            .when_some(self.project_notice.clone(), |this, notice| {
                                let color = if notice.ok {
                                    cx.theme().muted_foreground
                                } else {
                                    cx.theme().danger
                                };
                                this.child(div().text_xs().text_color(color).child(notice.text))
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("proj-new")
                                    .ghost()
                                    .label("New")
                                    .on_click(cx.listener(|this, _, window, cx| this.on_new(window, cx))),
                            )
                            .child(
                                Button::new("proj-open")
                                    .ghost()
                                    .label("Open")
                                    .on_click(cx.listener(|this, _, window, cx| this.on_open(window, cx))),
                            )
                            .child(
                                Button::new("proj-save")
                                    .primary()
                                    .label("Save")
                                    .on_click(cx.listener(|this, _, window, cx| this.on_save(window, cx))),
                            )
                            .child(
                                Button::new("proj-save-as")
                                    .ghost()
                                    .label("Save As")
                                    .on_click(cx.listener(|this, _, window, cx| this.on_save_as(window, cx))),
                            ),
                    ),
            )
            .when(!self.recents.paths.is_empty(), |this| {
                this.child(self.render_recents_row(cx))
            })
    }

    fn render_recents_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut chips = Vec::new();
        for (i, entry) in self.recents.paths.iter().take(6).enumerate() {
            let path = PathBuf::from(entry);
            let label = base_name(&path);
            chips.push(
                Button::new(SharedString::from(format!("recent-{i}")))
                    .ghost()
                    .label(label)
                    .tooltip(SharedString::from(entry.clone()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_recent(path.clone(), window, cx)
                    })),
            );
        }
        h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .px_4()
            .pb_2()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Recent"),
            )
            .children(chips)
    }
}

/// The campaign base name from a project path: strips the `.mmproj.toml`
/// suffix, else falls back to the file stem.
fn base_name(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("campaign");
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

fn field_label(text: &'static str, cx: &Context<MainWindow>) -> impl IntoElement {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

/// A labelled text field.
fn labeled(
    label: &'static str,
    input: &Entity<InputState>,
    cx: &Context<MainWindow>,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(field_label(label, cx))
        .child(Input::new(input))
}

/// A small "label: N" statistic with a coloured count.
fn stat(label: &'static str, value: usize, color: Hsla) -> impl IntoElement {
    h_flex()
        .gap_1p5()
        .items_baseline()
        .child(div().text_lg().text_color(color).child(value.to_string()))
        .child(div().text_xs().child(label))
}

fn status_style(status: &RowStatus, cx: &App) -> (Hsla, SharedString) {
    let theme = cx.theme();
    match status {
        RowStatus::Ok => (theme.success, "OK".into()),
        RowStatus::InvalidEmail => (theme.danger, "Bad email".into()),
        RowStatus::Duplicate => (theme.warning, "Duplicate".into()),
        RowStatus::MissingFields(_) => (theme.danger, "Missing".into()),
    }
}

fn outcome_style(status: &OutcomeStatus, cx: &App) -> (Hsla, SharedString) {
    let theme = cx.theme();
    match status {
        OutcomeStatus::Sent => (theme.success, "sent".into()),
        OutcomeStatus::Failed => (theme.danger, "failed".into()),
        OutcomeStatus::Skipped => (theme.warning, "skipped".into()),
    }
}

/// A "Label   value" line for the pre-flight summary card.
fn summary_row(label: &'static str, value: String, cx: &Context<MainWindow>) -> impl IntoElement {
    h_flex()
        .gap_3()
        .items_baseline()
        .child(
            div()
                .w(px(110.))
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(div().flex_1().text_sm().truncate().child(value))
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
    label: &'static str,
    value: ProviderKind,
    current: ProviderKind,
    cx: &mut Context<MainWindow>,
) -> Button {
    Button::new(id)
        .label(label)
        .selected(value == current)
        .on_click(cx.listener(move |this, _, _, cx| {
            this.form.kind = value;
            this.notice = None;
            cx.notify();
        }))
}

fn tls_button(
    id: &'static str,
    label: &'static str,
    value: TlsMode,
    current: TlsMode,
    cx: &mut Context<MainWindow>,
) -> Button {
    Button::new(id)
        .label(label)
        .selected(value == current)
        .on_click(cx.listener(move |this, _, _, cx| {
            this.form.tls = value;
            cx.notify();
        }))
}

fn region_button(
    id: &'static str,
    label: &'static str,
    value: MailgunRegion,
    current: MailgunRegion,
    cx: &mut Context<MainWindow>,
) -> Button {
    Button::new(id)
        .label(label)
        .selected(value == current)
        .on_click(cx.listener(move |this, _, _, cx| {
            this.form.region = value;
            cx.notify();
        }))
}

impl Render for MainWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_topbar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.render_sidebar(cx))
                    .child(self.render_content(window, cx)),
            )
    }
}
