//! The gpui↔tokio bridge and the campaign send engine.
//!
//! gpui runs its own executors while the mail stack (lettre, reqwest, aws-sdk)
//! needs tokio, so the app owns exactly one dedicated OS thread running a tokio
//! runtime ("mail runtime"). Communication is via flume channels, which offer
//! both sync and async APIs on both ends — the UI side sends commands
//! synchronously (non-blocking) and awaits events with `recv_async` on gpui's
//! foreground executor; flume's async support is runtime-agnostic, so no tokio
//! is needed on the gpui side.
//!
//! The send engine feeds recipients through a work queue to N workers that
//! render + send with per-account throttling (`governor`), retry with backoff,
//! a consecutive-failure circuit breaker, and cooperative cancellation. Progress
//! is coalesced into ~8 events/second so huge campaigns never flood the UI.

use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use mmm_core::template;
use mmm_providers::{Account, EmailProvider, RenderedEmail, SendError, build_provider};
use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub mod update;
pub use update::UpdateInfo;

// ---- Public API ---------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Command {
    /// Run a provider connectivity/credentials check ("Test connection").
    /// `secret` is the keychain value (SMTP password / API key) supplied by the
    /// caller so the engine needn't touch the keychain.
    TestAccount {
        account: Account,
        secret: String,
    },
    /// Run the interactive OAuth flow for an account (opens the browser) and,
    /// on success, store its tokens in the keychain. `client_secret` is empty
    /// for public clients (e.g. Azure with PKCE).
    ConnectOAuth {
        account: Account,
        client_secret: String,
    },
    /// Start sending a campaign. Boxed because the plan (with all recipients)
    /// is large relative to the other variants.
    StartCampaign(Box<CampaignPlan>),
    /// Cancel the running campaign. Emails already delivered stay delivered.
    CancelCampaign,
    /// Write the last campaign's outcome report to `path` as CSV.
    ExportReport {
        path: PathBuf,
    },
    /// Ask GitHub whether a newer release exists on this build's channel.
    CheckUpdate {
        current_version: String,
    },
    /// Download, verify, and apply a previously discovered update.
    ApplyUpdate(Box<UpdateInfo>),
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Result of a [`Command::TestAccount`], correlated by `account_id`.
    TestResult {
        account_id: String,
        ok: bool,
        message: String,
    },
    /// Result of a [`Command::ConnectOAuth`]. On success the tokens are already
    /// stored in the keychain.
    OAuthConnected {
        account_id: String,
        ok: bool,
        message: String,
    },
    /// Coalesced campaign progress (emitted ~8×/second while running, plus a
    /// final snapshot when the campaign ends).
    CampaignProgress(CampaignProgress),
    /// The campaign reached a terminal state.
    CampaignFinished { summary: CampaignSummary },
    /// Result of a [`Command::ExportReport`].
    ReportExported { ok: bool, message: String },
    /// A newer release is available (result of [`Command::CheckUpdate`]).
    UpdateAvailable(Box<UpdateInfo>),
    /// The check succeeded and this build is current.
    UpdateNotAvailable,
    /// The update check failed softly (offline, rate-limited, …).
    UpdateCheckFailed { message: String },
    /// An update was applied; the app should quit (a new process is starting).
    UpdateApplied,
    /// Applying the update failed.
    UpdateFailed { message: String },
}

/// Everything the engine needs to send one campaign.
#[derive(Debug, Clone)]
pub struct CampaignPlan {
    pub account: Account,
    pub secret: String,
    pub subject_template: String,
    pub body_template: String,
    pub generate_text_alt: bool,
    pub messages_per_second: f32,
    pub retry_limit: u32,
    /// Abort after this many consecutive hard failures. 0 = never.
    pub stop_after_failures: u32,
    pub recipients: Vec<CampaignRecipient>,
}

/// One recipient, pre-resolved to its template context. `index` is the original
/// row index in the source file, kept for the outcome report / resume (M7).
#[derive(Debug, Clone)]
pub struct CampaignRecipient {
    pub index: usize,
    pub email: String,
    pub context: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeStatus {
    Sent,
    Failed,
    Skipped,
}

impl OutcomeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// The result of attempting one recipient.
#[derive(Debug, Clone)]
pub struct RowOutcome {
    pub index: usize,
    pub email: String,
    pub status: OutcomeStatus,
    pub error: Option<String>,
    pub provider_message_id: Option<String>,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CampaignState {
    Running,
    Completed,
    Cancelled,
    Stopped(String),
}

#[derive(Debug, Clone)]
pub struct CampaignProgress {
    pub sent: usize,
    pub failed: usize,
    pub skipped: usize,
    pub total: usize,
    pub rate_per_sec: f32,
    pub eta_secs: Option<u64>,
    /// Most-recent-first tail of the outcome log, for the live table.
    pub recent: Vec<RowOutcome>,
    pub state: CampaignState,
}

#[derive(Debug, Clone)]
pub struct CampaignSummary {
    pub sent: usize,
    pub failed: usize,
    pub skipped: usize,
    pub total: usize,
    pub state: CampaignState,
    pub elapsed_secs: u64,
}

pub struct MailRuntime {
    cmd_tx: flume::Sender<Command>,
    evt_rx: flume::Receiver<Event>,
    thread: Option<JoinHandle<()>>,
}

impl MailRuntime {
    /// Spawn the mail-runtime thread. Call once at app start.
    pub fn start() -> Self {
        let (cmd_tx, cmd_rx) = flume::bounded::<Command>(64);
        let (evt_tx, evt_rx) = flume::bounded::<Event>(512);

        let thread = std::thread::Builder::new()
            .name("mail-runtime".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(4)
                    .enable_all()
                    .build()
                    .expect("failed to build mail runtime");
                runtime.block_on(run_loop(cmd_rx, evt_tx));
            })
            .expect("failed to spawn mail-runtime thread");

        Self {
            cmd_tx,
            evt_rx,
            thread: Some(thread),
        }
    }

    /// Non-blocking; drops the command if the runtime is gone (app shutdown).
    pub fn command(&self, command: Command) {
        let _ = self.cmd_tx.send(command);
    }

    /// Clone of the event stream. Await with `recv_async` from any executor.
    pub fn events(&self) -> flume::Receiver<Event> {
        self.evt_rx.clone()
    }
}

impl Drop for MailRuntime {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ---- Command loop -------------------------------------------------------

async fn run_loop(cmd_rx: flume::Receiver<Command>, evt_tx: flume::Sender<Event>) {
    let mut current: Option<CancellationToken> = None;
    // Retained across commands so ExportReport can serialize the last run.
    let report: Arc<Mutex<Vec<RowOutcome>>> = Arc::new(Mutex::new(Vec::new()));

    while let Ok(command) = cmd_rx.recv_async().await {
        match command {
            Command::TestAccount { account, secret } => {
                tokio::spawn(test_account(account, secret, evt_tx.clone()));
            }
            Command::ConnectOAuth {
                account,
                client_secret,
            } => {
                tokio::spawn(connect_oauth(account, client_secret, evt_tx.clone()));
            }
            Command::StartCampaign(plan) => {
                if let Some(token) = current.take() {
                    token.cancel();
                }
                let token = CancellationToken::new();
                current = Some(token.clone());
                let events = evt_tx.clone();
                let store = report.clone();
                tokio::spawn(async move {
                    let plan = *plan;
                    match build_provider(&plan.account, plan.secret.clone()) {
                        Ok(boxed) => {
                            let provider: Arc<dyn EmailProvider> = Arc::from(boxed);
                            run_campaign(provider, plan, token, events, store).await;
                        }
                        Err(e) => {
                            let _ = events
                                .send_async(Event::CampaignFinished {
                                    summary: CampaignSummary {
                                        sent: 0,
                                        failed: 0,
                                        skipped: plan.recipients.len(),
                                        total: plan.recipients.len(),
                                        state: CampaignState::Stopped(format!(
                                            "Could not start: {e}"
                                        )),
                                        elapsed_secs: 0,
                                    },
                                })
                                .await;
                        }
                    }
                });
            }
            Command::CancelCampaign => {
                if let Some(token) = &current {
                    token.cancel();
                }
            }
            Command::ExportReport { path } => {
                let result = export_csv(&report, &path);
                let event = match result {
                    Ok(n) => Event::ReportExported {
                        ok: true,
                        message: format!("Exported {n} rows to {}", path.display()),
                    },
                    Err(e) => Event::ReportExported {
                        ok: false,
                        message: e,
                    },
                };
                let _ = evt_tx.send_async(event).await;
            }
            Command::CheckUpdate { current_version } => {
                let events = evt_tx.clone();
                tokio::spawn(async move {
                    let event = match update::check(&current_version).await {
                        Ok(Some(info)) => Event::UpdateAvailable(Box::new(info)),
                        Ok(None) => Event::UpdateNotAvailable,
                        Err(message) => Event::UpdateCheckFailed { message },
                    };
                    let _ = events.send_async(event).await;
                });
            }
            Command::ApplyUpdate(info) => {
                let events = evt_tx.clone();
                tokio::spawn(async move {
                    let event = match update::apply(&info).await {
                        Ok(()) => Event::UpdateApplied,
                        Err(message) => Event::UpdateFailed { message },
                    };
                    let _ = events.send_async(event).await;
                });
            }
            Command::Shutdown => {
                if let Some(token) = current.take() {
                    token.cancel();
                }
                break;
            }
        }
    }
}

async fn connect_oauth(account: Account, client_secret: String, events: flume::Sender<Event>) {
    let account_id = account.id.clone();
    let (ok, message) = match mmm_providers::oauth::connect(&account, &client_secret).await {
        Ok(tokens) => match mmm_providers::oauth::store_tokens(&account.id, &tokens) {
            Ok(()) => (true, "Connected and authorized.".to_string()),
            Err(e) => (
                false,
                format!("Authorized, but could not store tokens: {e}"),
            ),
        },
        Err(e) => (false, e),
    };
    let _ = events
        .send_async(Event::OAuthConnected {
            account_id,
            ok,
            message,
        })
        .await;
}

async fn test_account(account: Account, secret: String, events: flume::Sender<Event>) {
    let account_id = account.id.clone();
    let (ok, message) = match build_provider(&account, secret) {
        Ok(provider) => match provider.verify().await {
            Ok(()) => (true, "Connection succeeded.".to_string()),
            Err(e) => (false, e.to_string()),
        },
        Err(e) => (false, e.to_string()),
    };
    let _ = events
        .send_async(Event::TestResult {
            account_id,
            ok,
            message,
        })
        .await;
}

// ---- Campaign engine ----------------------------------------------------

struct Templates {
    subject: String,
    body: String,
    generate_text_alt: bool,
}

/// Shared, campaign-scoped state accessed by every worker and the reporter.
struct Shared {
    provider: Arc<dyn EmailProvider>,
    templates: Templates,
    limiter: DefaultDirectRateLimiter,
    rx: flume::Receiver<CampaignRecipient>,
    token: CancellationToken,
    retry_limit: u32,
    stop_after_failures: u32,
    report: Arc<Mutex<Vec<RowOutcome>>>,
    sent: AtomicUsize,
    failed: AtomicUsize,
    skipped: AtomicUsize,
    consecutive_failures: AtomicUsize,
    stopped_by_breaker: AtomicBool,
}

/// Run one campaign to completion. `provider` is injected (built from the plan's
/// account by the caller) so this is unit-testable with a mock provider.
async fn run_campaign(
    provider: Arc<dyn EmailProvider>,
    plan: CampaignPlan,
    token: CancellationToken,
    events: flume::Sender<Event>,
    report: Arc<Mutex<Vec<RowOutcome>>>,
) {
    let total = plan.recipients.len();
    let start = Instant::now();

    {
        let mut log = report.lock().unwrap();
        log.clear();
        log.reserve(total);
    }

    if total == 0 {
        let summary = CampaignSummary {
            sent: 0,
            failed: 0,
            skipped: 0,
            total: 0,
            state: CampaignState::Completed,
            elapsed_secs: 0,
        };
        let _ = events.send_async(Event::CampaignFinished { summary }).await;
        return;
    }

    let (tx, rx) = flume::unbounded::<CampaignRecipient>();
    for recipient in plan.recipients {
        let _ = tx.send(recipient);
    }
    drop(tx);

    let capabilities = provider.capabilities();
    let rate = quota_rate(
        plan.messages_per_second,
        capabilities.suggested_rate_per_sec,
    );
    let limiter = RateLimiter::direct(Quota::per_second(rate));
    let workers = total.clamp(1, 4);

    let shared = Arc::new(Shared {
        provider,
        templates: Templates {
            subject: plan.subject_template,
            body: plan.body_template,
            generate_text_alt: plan.generate_text_alt,
        },
        limiter,
        rx: rx.clone(),
        token: token.clone(),
        retry_limit: plan.retry_limit,
        stop_after_failures: plan.stop_after_failures,
        report: report.clone(),
        sent: AtomicUsize::new(0),
        failed: AtomicUsize::new(0),
        skipped: AtomicUsize::new(0),
        consecutive_failures: AtomicUsize::new(0),
        stopped_by_breaker: AtomicBool::new(false),
    });

    let mut set = JoinSet::new();
    for _ in 0..workers {
        let shared = shared.clone();
        set.spawn(async move { worker(shared).await });
    }

    // Reporter: emit a coalesced snapshot on a fixed cadence while workers run.
    let mut interval = tokio::time::interval(Duration::from_millis(125));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if set.is_empty() {
            break;
        }
        tokio::select! {
            _ = interval.tick() => {
                let progress = snapshot(&shared, total, start, CampaignState::Running);
                let _ = events.send_async(Event::CampaignProgress(progress)).await;
            }
            _ = set.join_next() => {}
        }
    }

    // Anything still queued (a cancel drained the workers early) is skipped so
    // the outcome report accounts for every recipient.
    while let Ok(recipient) = rx.try_recv() {
        shared.skipped.fetch_add(1, Ordering::Relaxed);
        record(
            &shared,
            RowOutcome {
                index: recipient.index,
                email: recipient.email,
                status: OutcomeStatus::Skipped,
                error: Some("cancelled".into()),
                provider_message_id: None,
                timestamp_ms: now_ms(),
            },
        );
    }

    let state = if shared.stopped_by_breaker.load(Ordering::Relaxed) {
        CampaignState::Stopped(format!(
            "Stopped after {} consecutive failures to protect sender reputation.",
            plan.stop_after_failures
        ))
    } else if token.is_cancelled() {
        CampaignState::Cancelled
    } else {
        CampaignState::Completed
    };

    let final_progress = snapshot(&shared, total, start, state.clone());
    let _ = events
        .send_async(Event::CampaignProgress(final_progress))
        .await;

    let summary = CampaignSummary {
        sent: shared.sent.load(Ordering::Relaxed),
        failed: shared.failed.load(Ordering::Relaxed),
        skipped: shared.skipped.load(Ordering::Relaxed),
        total,
        state,
        elapsed_secs: start.elapsed().as_secs(),
    };
    let _ = events.send_async(Event::CampaignFinished { summary }).await;
}

async fn worker(shared: Arc<Shared>) {
    while let Ok(recipient) = shared.rx.recv_async().await {
        if shared.token.is_cancelled() {
            skip(&shared, recipient, "cancelled");
            break;
        }

        // Throttle (cancellable so a cancel doesn't wait out the delay).
        tokio::select! {
            _ = shared.token.cancelled() => {
                skip(&shared, recipient, "cancelled");
                break;
            }
            _ = shared.limiter.until_ready() => {}
        }

        let rendered = match render_email(&shared.templates, &recipient) {
            Ok(email) => email,
            Err(error) => {
                fail(&shared, &recipient, error);
                if breaker_tripped(&shared) {
                    break;
                }
                continue;
            }
        };

        let (status, error, message_id) = send_with_retry(
            &shared.provider,
            &rendered,
            &shared.token,
            shared.retry_limit,
        )
        .await;

        match status {
            OutcomeStatus::Sent => {
                shared.sent.fetch_add(1, Ordering::Relaxed);
                shared.consecutive_failures.store(0, Ordering::Relaxed);
                record(
                    &shared,
                    RowOutcome {
                        index: recipient.index,
                        email: recipient.email,
                        status: OutcomeStatus::Sent,
                        error: None,
                        provider_message_id: message_id,
                        timestamp_ms: now_ms(),
                    },
                );
            }
            OutcomeStatus::Failed => {
                fail(&shared, &recipient, error.unwrap_or_default());
                if breaker_tripped(&shared) {
                    break;
                }
            }
            OutcomeStatus::Skipped => {
                // Cancelled mid-send.
                skip(&shared, recipient, "cancelled");
                break;
            }
        }
    }
}

/// Increment failures, log the outcome, and update the circuit breaker.
fn fail(shared: &Arc<Shared>, recipient: &CampaignRecipient, error: String) {
    shared.failed.fetch_add(1, Ordering::Relaxed);
    let consecutive = shared.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
    record(
        shared,
        RowOutcome {
            index: recipient.index,
            email: recipient.email.clone(),
            status: OutcomeStatus::Failed,
            error: Some(error),
            provider_message_id: None,
            timestamp_ms: now_ms(),
        },
    );
    if shared.stop_after_failures > 0 && consecutive >= shared.stop_after_failures as usize {
        shared.stopped_by_breaker.store(true, Ordering::Relaxed);
        shared.token.cancel();
    }
}

fn skip(shared: &Arc<Shared>, recipient: CampaignRecipient, reason: &str) {
    shared.skipped.fetch_add(1, Ordering::Relaxed);
    record(
        shared,
        RowOutcome {
            index: recipient.index,
            email: recipient.email,
            status: OutcomeStatus::Skipped,
            error: Some(reason.to_string()),
            provider_message_id: None,
            timestamp_ms: now_ms(),
        },
    );
}

fn breaker_tripped(shared: &Arc<Shared>) -> bool {
    shared.stopped_by_breaker.load(Ordering::Relaxed)
}

fn record(shared: &Arc<Shared>, outcome: RowOutcome) {
    shared.report.lock().unwrap().push(outcome);
}

fn render_email(
    templates: &Templates,
    recipient: &CampaignRecipient,
) -> Result<RenderedEmail, String> {
    let subject =
        template::render(&templates.subject, &recipient.context).map_err(|e| e.to_string())?;
    let html_body =
        template::render(&templates.body, &recipient.context).map_err(|e| e.to_string())?;
    let text_alt = templates
        .generate_text_alt
        .then(|| template::html_to_text(&html_body));
    Ok(RenderedEmail {
        to: recipient.email.clone(),
        subject,
        html_body,
        text_alt,
    })
}

/// Send with exponential backoff (1s, 4s, 16s, …) on retryable errors, up to
/// `retry_limit` retries. Returns `(status, error, provider_message_id)`.
async fn send_with_retry(
    provider: &Arc<dyn EmailProvider>,
    email: &RenderedEmail,
    token: &CancellationToken,
    retry_limit: u32,
) -> (OutcomeStatus, Option<String>, Option<String>) {
    let mut attempt: u32 = 0;
    loop {
        if token.is_cancelled() {
            return (OutcomeStatus::Skipped, Some("cancelled".into()), None);
        }
        match provider.send(email, token).await {
            Ok(receipt) => return (OutcomeStatus::Sent, None, receipt.provider_message_id),
            Err(SendError::Cancelled) => {
                return (OutcomeStatus::Skipped, Some("cancelled".into()), None);
            }
            Err(SendError::Fatal(e)) => return (OutcomeStatus::Failed, Some(e), None),
            Err(SendError::Retryable(e)) => {
                if attempt >= retry_limit {
                    return (OutcomeStatus::Failed, Some(e), None);
                }
                let backoff = Duration::from_secs(4u64.pow(attempt).min(60));
                tokio::select! {
                    _ = token.cancelled() => {
                        return (OutcomeStatus::Skipped, Some("cancelled".into()), None);
                    }
                    _ = tokio::time::sleep(backoff) => {}
                }
                attempt += 1;
            }
        }
    }
}

fn snapshot(
    shared: &Arc<Shared>,
    total: usize,
    start: Instant,
    state: CampaignState,
) -> CampaignProgress {
    let sent = shared.sent.load(Ordering::Relaxed);
    let failed = shared.failed.load(Ordering::Relaxed);
    let skipped = shared.skipped.load(Ordering::Relaxed);
    let processed = sent + failed + skipped;

    let elapsed = start.elapsed().as_secs_f32();
    let rate = if elapsed > 0.0 {
        sent as f32 / elapsed
    } else {
        0.0
    };
    let remaining = total.saturating_sub(processed);
    let eta = if rate > 0.0 && remaining > 0 {
        Some((remaining as f32 / rate).ceil() as u64)
    } else {
        None
    };

    let recent: Vec<RowOutcome> = {
        let log = shared.report.lock().unwrap();
        log.iter().rev().take(50).cloned().collect()
    };

    CampaignProgress {
        sent,
        failed,
        skipped,
        total,
        rate_per_sec: rate,
        eta_secs: eta,
        recent,
        state,
    }
}

/// Clamp the requested rate to the provider's suggestion and to [1, u32::MAX].
fn quota_rate(requested: f32, suggested: f32) -> NonZeroU32 {
    let clamped = requested.min(suggested).round();
    let value = if clamped.is_finite() && clamped >= 1.0 {
        clamped as u32
    } else {
        1
    };
    NonZeroU32::new(value.max(1)).unwrap()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse an outcome report CSV previously written by [`export_csv`]. Used to
/// resume a campaign by re-running its non-`sent` rows (M7).
pub fn load_report(path: &std::path::Path) -> Result<Vec<RowOutcome>, String> {
    let mut reader = csv::Reader::from_path(path).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|e| e.to_string())?;
        let index: usize = record
            .get(0)
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| "outcome report: missing/invalid row index".to_string())?;
        let email = record.get(1).unwrap_or_default().to_string();
        let status = match record.get(2).unwrap_or_default().trim() {
            "sent" => OutcomeStatus::Sent,
            "failed" => OutcomeStatus::Failed,
            _ => OutcomeStatus::Skipped,
        };
        let error = record.get(3).filter(|s| !s.is_empty()).map(String::from);
        let provider_message_id = record.get(4).filter(|s| !s.is_empty()).map(String::from);
        let timestamp_ms = record
            .get(5)
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        rows.push(RowOutcome {
            index,
            email,
            status,
            error,
            provider_message_id,
            timestamp_ms,
        });
    }
    Ok(rows)
}

fn export_csv(
    report: &Arc<Mutex<Vec<RowOutcome>>>,
    path: &std::path::Path,
) -> Result<usize, String> {
    let rows = report.lock().unwrap().clone();
    let mut writer = csv::Writer::from_path(path).map_err(|e| e.to_string())?;
    writer
        .write_record([
            "row",
            "email",
            "status",
            "error",
            "provider_message_id",
            "timestamp_ms",
        ])
        .map_err(|e| e.to_string())?;
    for outcome in &rows {
        writer
            .write_record([
                outcome.index.to_string(),
                outcome.email.clone(),
                outcome.status.as_str().to_string(),
                outcome.error.clone().unwrap_or_default(),
                outcome.provider_message_id.clone().unwrap_or_default(),
                outcome.timestamp_ms.to_string(),
            ])
            .map_err(|e| e.to_string())?;
    }
    writer.flush().map_err(|e| e.to_string())?;
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use mmm_providers::{
        Account, AccountConfig, Capabilities, ProviderError, ProviderKind, SendReceipt, SmtpConfig,
        TlsMode,
    };
    use std::collections::HashSet;

    /// A provider that records what it sent and fails a configured set of
    /// addresses. `fatal` addresses fail permanently; everything else succeeds.
    struct MockProvider {
        fatal: HashSet<String>,
        sent: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl EmailProvider for MockProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Smtp
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                suggested_rate_per_sec: 1000.0,
                immediate_status: true,
            }
        }
        async fn verify(&self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn send(
            &self,
            message: &RenderedEmail,
            _cancel: &CancellationToken,
        ) -> Result<SendReceipt, SendError> {
            if self.fatal.contains(&message.to) {
                return Err(SendError::Fatal("rejected".into()));
            }
            self.sent.lock().unwrap().push(message.to.clone());
            Ok(SendReceipt {
                provider_message_id: Some(format!("id-{}", message.to)),
            })
        }
    }

    fn account() -> Account {
        Account {
            id: "acct_test".into(),
            display: "test".into(),
            config: AccountConfig::Smtp(SmtpConfig {
                host: "localhost".into(),
                port: 25,
                tls: TlsMode::None,
                username: String::new(),
                from: "from@example.com".into(),
            }),
        }
    }

    fn recipient(i: usize, email: &str, name: &str) -> CampaignRecipient {
        CampaignRecipient {
            index: i,
            email: email.into(),
            context: [("name".to_string(), name.to_string())]
                .into_iter()
                .collect(),
        }
    }

    fn plan(recipients: Vec<CampaignRecipient>, stop_after_failures: u32) -> CampaignPlan {
        CampaignPlan {
            account: account(),
            secret: String::new(),
            subject_template: "Hi {{name}}".into(),
            body_template: "<p>Hello {{name}}</p>".into(),
            generate_text_alt: true,
            messages_per_second: 1000.0,
            retry_limit: 0,
            stop_after_failures,
            recipients,
        }
    }

    async fn drain_to_finish(events: &flume::Receiver<Event>) -> CampaignSummary {
        loop {
            if let Event::CampaignFinished { summary } = events.recv_async().await.unwrap() {
                return summary;
            }
        }
    }

    #[tokio::test]
    async fn sends_all_recipients() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let provider: Arc<dyn EmailProvider> = Arc::new(MockProvider {
            fatal: HashSet::new(),
            sent: sent.clone(),
        });
        let report = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = flume::unbounded();

        run_campaign(
            provider,
            plan(
                vec![
                    recipient(0, "a@x.com", "Ada"),
                    recipient(1, "b@x.com", "Bob"),
                    recipient(2, "c@x.com", "Cy"),
                ],
                0,
            ),
            CancellationToken::new(),
            tx,
            report.clone(),
        )
        .await;

        let summary = drain_to_finish(&rx).await;
        assert_eq!(summary.state, CampaignState::Completed);
        assert_eq!(summary.sent, 3);
        assert_eq!(summary.failed, 0);
        assert_eq!(sent.lock().unwrap().len(), 3);
        // Report renders the message and captures provider ids.
        let report = report.lock().unwrap();
        assert_eq!(report.len(), 3);
        assert!(report.iter().all(|o| o.provider_message_id.is_some()));
    }

    #[tokio::test]
    async fn records_fatal_failures() {
        let provider: Arc<dyn EmailProvider> = Arc::new(MockProvider {
            fatal: ["b@x.com".to_string()].into_iter().collect(),
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let report = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = flume::unbounded();

        run_campaign(
            provider,
            plan(
                vec![
                    recipient(0, "a@x.com", "Ada"),
                    recipient(1, "b@x.com", "Bob"),
                ],
                0,
            ),
            CancellationToken::new(),
            tx,
            report,
        )
        .await;

        let summary = drain_to_finish(&rx).await;
        assert_eq!(summary.sent, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.state, CampaignState::Completed);
    }

    #[tokio::test]
    async fn circuit_breaker_stops_campaign() {
        // Every send fails fatally with a low threshold: should stop early.
        let all: HashSet<String> = (0..20).map(|i| format!("r{i}@x.com")).collect();
        let provider: Arc<dyn EmailProvider> = Arc::new(MockProvider {
            fatal: all,
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let report = Arc::new(Mutex::new(Vec::new()));
        let recipients: Vec<_> = (0..20)
            .map(|i| recipient(i, &format!("r{i}@x.com"), "X"))
            .collect();
        let (tx, rx) = flume::unbounded();

        // Single worker path keeps "consecutive" deterministic; use 1 recipient
        // per... actually with 20 recipients and threshold 3 the breaker trips.
        run_campaign(
            provider,
            plan(recipients, 3),
            CancellationToken::new(),
            tx,
            report,
        )
        .await;

        let summary = drain_to_finish(&rx).await;
        assert!(matches!(summary.state, CampaignState::Stopped(_)));
        // Not everything was attempted; some were skipped by the breaker's cancel.
        assert!(summary.skipped > 0);
        assert_eq!(
            summary.sent + summary.failed + summary.skipped,
            summary.total
        );
    }

    #[test]
    fn exports_report_csv() {
        let report = Arc::new(Mutex::new(vec![
            RowOutcome {
                index: 0,
                email: "a@x.com".into(),
                status: OutcomeStatus::Sent,
                error: None,
                provider_message_id: Some("id-1".into()),
                timestamp_ms: 111,
            },
            RowOutcome {
                index: 1,
                email: "b@x.com".into(),
                status: OutcomeStatus::Failed,
                error: Some("bad, comma \"quoted\"".into()),
                provider_message_id: None,
                timestamp_ms: 222,
            },
        ]));
        let path = std::env::temp_dir().join(format!("mmm_report_{}.csv", now_ms()));
        let count = export_csv(&report, &path).unwrap();
        assert_eq!(count, 2);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("row,email,status,error,provider_message_id,timestamp_ms\n"));
        assert!(text.contains("0,a@x.com,sent,,id-1,111"));
        // The comma/quote in the error is CSV-escaped by the writer.
        assert!(text.contains("\"bad, comma \"\"quoted\"\"\""));

        // Round-trips back through the resume parser.
        let loaded = load_report(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].index, 0);
        assert_eq!(loaded[0].status, OutcomeStatus::Sent);
        assert_eq!(loaded[0].provider_message_id.as_deref(), Some("id-1"));
        assert_eq!(loaded[1].status, OutcomeStatus::Failed);
        assert_eq!(loaded[1].error.as_deref(), Some("bad, comma \"quoted\""));
    }

    #[tokio::test]
    async fn cancel_before_start_skips_all() {
        let provider: Arc<dyn EmailProvider> = Arc::new(MockProvider {
            fatal: HashSet::new(),
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let report = Arc::new(Mutex::new(Vec::new()));
        let token = CancellationToken::new();
        token.cancel();
        let (tx, rx) = flume::unbounded();

        run_campaign(
            provider,
            plan(
                (0..10)
                    .map(|i| recipient(i, &format!("r{i}@x.com"), "X"))
                    .collect(),
                0,
            ),
            token,
            tx,
            report,
        )
        .await;

        let summary = drain_to_finish(&rx).await;
        assert_eq!(summary.state, CampaignState::Cancelled);
        assert_eq!(summary.sent, 0);
        assert_eq!(summary.skipped, 10);
    }
}
