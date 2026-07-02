use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme, Disableable as _, Icon, IconName, Selectable as _, h_flex,
    button::{Button, ButtonVariants as _},
    input::{Input, InputState},
    progress::Progress,
    sidebar::{Sidebar, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem},
    v_flex,
};
use mmm_core::import::{self, RecipientTable, SourceKind};
use mmm_core::mapping::{self, RowStatus, ValidationReport};
use mmm_core::template::{extract_placeholders, normalize_placeholders};
use mmm_engine::{Command, Event, MailRuntime};
use mmm_providers::{
    Account, AccountConfig, AccountStore, MailgunConfig, MailgunRegion, ProviderKind, SmtpConfig,
    TlsMode, account::new_account_id, secrets,
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum JobState {
    Idle,
    Running {
        done: u32,
        total: u32,
    },
    Finished {
        done: u32,
        total: u32,
        cancelled: bool,
    },
}

/// A transient status line under the account form (test result, save error, …).
#[derive(Debug, Clone)]
struct Notice {
    ok: bool,
    text: String,
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
                .placeholder("<p>Hi {{first_name}},</p>")
                .multi_line(true)
                .rows(10)
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
    job: JobState,
    mail: MailRuntime,

    // M1 — accounts
    store: AccountStore,
    form: AccountForm,
    notice: Option<Notice>,
    testing: bool,

    // M2 — recipients + a minimal template to drive field mapping
    template: TemplateForm,
    recipients: RecipientsState,
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
            job: JobState::Idle,
            mail,
            store,
            form,
            notice: None,
            testing: false,
            template,
            recipients: RecipientsState::Empty,
        }
    }

    fn on_engine_event(&mut self, event: Event) {
        match event {
            Event::JobProgress { done, total } => self.job = JobState::Running { done, total },
            Event::JobFinished {
                done,
                total,
                cancelled,
            } => {
                self.job = JobState::Finished {
                    done,
                    total,
                    cancelled,
                }
            }
            Event::TestResult { ok, message, .. } => {
                self.testing = false;
                self.notice = Some(Notice { ok, text: message });
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
            other => Err(format!("{} accounts are not supported yet.", other.label())),
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
        self.recipients = RecipientsState::Loading;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let selected = match paths.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let Some(path) = selected else {
                // Cancelled — return to the empty state.
                let _ = this.update(cx, |this, cx| {
                    this.recipients = RecipientsState::Empty;
                    cx.notify();
                });
                return;
            };

            let parse_path = path.clone();
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
                            let sheet = sheets.first().cloned();
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

            let _ = this.update(cx, |this, cx| this.on_parsed(path, output, cx));
        })
        .detach();
    }

    fn on_select_sheet(&mut self, name: String, cx: &mut Context<Self>) {
        let (path, sheets) = match &self.recipients {
            RecipientsState::Loaded(l) => (l.path.clone(), l.sheets.clone()),
            _ => return,
        };
        self.recipients = RecipientsState::Loading;
        cx.notify();

        let parse_path = path.clone();
        let parse_name = name.clone();
        cx.spawn(async move |this, cx| {
            let table: Result<RecipientTable, String> = cx
                .background_executor()
                .spawn(async move {
                    import::parse_excel(&parse_path, Some(&parse_name)).map_err(|e| e.to_string())
                })
                .await;

            let _ = this.update(cx, |this, cx| match table {
                Ok(table) => this.on_parsed(
                    path,
                    Ok(ParseOutput {
                        sheets,
                        sheet: Some(name),
                        table,
                    }),
                    cx,
                ),
                Err(e) => {
                    this.recipients = RecipientsState::Error(e);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn on_parsed(&mut self, path: PathBuf, output: Result<ParseOutput, String>, cx: &mut Context<Self>) {
        match output {
            Err(e) => self.recipients = RecipientsState::Error(e),
            Ok(output) => {
                let table = Rc::new(output.table);
                let email_col = import::detect_email_column_in(&table).unwrap_or(0);
                let fields = self.template_fields(cx);
                let mapping = mapping::auto_map(&fields, &table.headers);
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

    fn render_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                this.child(self.render_template(cx))
            })
            .when(section == Section::Recipients, |this| {
                this.child(self.render_recipients(cx))
            })
            .when(section == Section::Send, |this| {
                this.child(self.render_bridge_demo(cx))
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
                            .child(kind_button("k-smtp", "Generic SMTP", ProviderKind::Smtp, kind, cx))
                            .child(kind_button("k-mailgun", "Mailgun", ProviderKind::Mailgun, kind, cx)),
                    ),
            )
            .child(labeled("Display name (optional)", &self.form.display, cx))
            .when(kind == ProviderKind::Smtp, |this| {
                this.child(self.render_smtp_fields(cx))
            })
            .when(kind == ProviderKind::Mailgun, |this| {
                this.child(self.render_mailgun_fields(cx))
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
                    .child(
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
                    .child(
                        Button::new("cancel-account")
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.form.open = false;
                                this.notice = None;
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

    // ---- Template UI (minimal) ------------------------------------------

    fn render_template(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_4()
            .max_w(px(760.))
            .child(labeled("Subject", &self.template.subject, cx))
            .child(
                v_flex()
                    .gap_1()
                    .child(field_label("Body (HTML)", cx))
                    .child(Input::new(&self.template.body)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "Placeholders like {{first_name}} (or ##first_name##) become mappable \
                         fields in Recipients. A rich editor with live preview arrives in M3.",
                    ),
            )
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

    /// M0 proof that UI → tokio → UI works: a simulated 200-email campaign
    /// with live progress and cancellation. Becomes the real send flow in M4.
    fn render_bridge_demo(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (done, total, status) = match self.job {
            JobState::Idle => (0, 200, "Ready.".to_string()),
            JobState::Running { done, total } => {
                (done, total, format!("Sending… {done} of {total}"))
            }
            JobState::Finished {
                done,
                total,
                cancelled,
            } => {
                let label = if cancelled {
                    format!("Cancelled — {done} of {total} sent.")
                } else {
                    format!("Done — all {total} sent.")
                };
                (done, total, label)
            }
        };
        let running = matches!(self.job, JobState::Running { .. });
        let percentage = if total == 0 {
            0.0
        } else {
            done as f32 / total as f32 * 100.0
        };

        v_flex()
            .gap_3()
            .mt_4()
            .p_4()
            .max_w(px(560.))
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .child(div().text_sm().child("Engine bridge demo (M0)"))
            .child(Progress::new().value(percentage))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(status),
            )
            .child(
                h_flex()
                    .gap_2()
                    .when(!running, |this| {
                        this.child(
                            Button::new("start-dummy")
                                .primary()
                                .label("Simulate sending 200 emails")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.job = JobState::Running {
                                        done: 0,
                                        total: 200,
                                    };
                                    this.mail.command(Command::StartDummyJob { total: 200 });
                                    cx.notify();
                                })),
                        )
                    })
                    .when(running, |this| {
                        this.child(Button::new("cancel-dummy").label("Cancel").on_click(
                            cx.listener(|this, _, _, _| {
                                this.mail.command(Command::CancelJob);
                            }),
                        ))
                    }),
            )
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_sidebar(cx))
            .child(self.render_content(cx))
    }
}
