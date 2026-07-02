# MassFckinMailer — Design & Implementation Plan

Cross-platform desktop mass mailer. Rust + gpui + gpui-component. Non-blocking, smooth, lightweight.

## 1. Confirmed decisions

| Decision | Choice |
|---|---|
| v1 providers | Generic SMTP, Mailgun API, AWS SES, Gmail/Outlook OAuth |
| Project file format | TOML (human readable, comments allowed) |
| Credential storage | OS keychain (Windows Credential Manager / macOS Keychain / Secret Service) |
| Templates | HTML with live preview (+ auto-generated plain-text alternative) |
| Placeholder syntax | `{{fieldName}}` primary; `##fieldName##` accepted and normalized |

## 2. Tech stack

| Concern | Crate | Notes |
|---|---|---|
| UI framework | `gpui` | Pre-1.0, breaking changes between versions — pin exact version |
| UI components | `gpui-component` (0.5.1 current) | 60+ components: virtualized Table/List, code editor, WebView (wry), Markdown/simple-HTML rendering, charts |
| Async runtime | gpui executors + dedicated `tokio` runtime thread | gpui has its own foreground/background executors; lettre/reqwest/aws-sdk need tokio. Bridge via channels (see §4) |
| SMTP | `lettre` (tokio + rustls) | Covers generic SMTP incl. Gmail/Outlook app passwords |
| HTTP (Mailgun, Gmail/Graph APIs) | `reqwest` (rustls) | |
| AWS SES | `aws-sdk-sesv2` | Official SDK, tokio-based |
| OAuth | `oauth2` + loopback redirect listener | PKCE flow, no client secret shipped |
| Templating | `minijinja` | Lightweight, `{{var}}` native; pre-pass converts `##var##` → `{{var}}` |
| CSV | `csv` | Streaming reader |
| Excel | `calamine` | .xlsx/.xls/.ods read-only — exactly what we need |
| Project file | `toml` + `serde` | |
| Secrets | `keyring` | Cross-platform keychain abstraction |
| Channels | `flume` | UI↔engine command/event channels (bounded, works across gpui & tokio) |
| Rate limiting | `governor` | Per-account send throttle |
| Cancellation | `tokio_util::sync::CancellationToken` | |
| Errors/logging | `thiserror`/`anyhow`, `tracing` | |

## 3. Workspace layout

```
massfckinmailer/
├── Cargo.toml            # workspace
├── crates/
│   ├── core/             # domain types, project file, templating, import, field mapping — NO ui, NO tokio
│   ├── providers/        # EmailProvider trait + smtp/mailgun/ses/gmail/outlook impls (tokio)
│   ├── engine/           # send engine: queue, throttle, retry, progress events, cancel (tokio)
│   └── app/              # gpui app: views, state, tokio-bridge
└── assets/               # icons, help snippets
```

`core` stays pure and unit-testable. `providers` and `engine` are tokio-only. `app` owns the gpui side and the bridge.

## 4. Async model (the critical design point)

gpui runs its own executors; the email stack (lettre, reqwest, aws-sdk) requires tokio. Do not mix runtimes per-call. Instead:

1. On app start, spawn one dedicated thread running a multi-threaded tokio runtime ("mail runtime").
2. UI → engine: commands over a `flume` channel (`StartSend`, `Cancel`, `TestAccount`, …). flume is runtime-agnostic (sync + async APIs on both ends), which is exactly what a gpui↔tokio bridge needs.
3. Engine → UI: progress events over a bounded `flume` channel; the gpui side drains it with `cx.spawn` on the foreground executor and updates entities → views re-render. Coalesce events (e.g. batch progress every 50–100 ms) so 100k-row sends don't flood the UI thread.
4. File parsing (CSV/XLSX) runs on gpui's `background_executor` (no tokio needed) — UI never blocks.

Everything long-running is cancellable via `CancellationToken` propagated into the engine.

## 5. Provider abstraction

```rust
#[async_trait]
pub trait EmailProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn capabilities(&self) -> Capabilities; // max msg/sec hint, batch support, progress granularity
    async fn verify(&self) -> Result<(), ProviderError>;          // "Test connection" button
    async fn send(&self, msg: &RenderedEmail, cancel: &CancellationToken)
        -> Result<SendReceipt, SendError>;
}
```

- **SMTP**: lettre `AsyncSmtpTransport`, connection pooling/reuse. Fields: host, port, TLS mode, user, password(keychain).
- **Mailgun**: `POST /v3/{domain}/messages` via reqwest; API key in keychain; supports batch, but v1 sends per-recipient for accurate per-row status.
- **SES**: `SendEmail` (SESv2); access key/secret in keychain; surface SES sandbox-mode errors with a helpful message.
- **Gmail**: OAuth PKCE → `users.messages.send` (RFC 2822 base64url). **Caveat**: requires the user to create their own Google Cloud OAuth client ID (we ship instructions, user pastes client ID). Same pattern for **Outlook** via Microsoft Graph `sendMail` (Azure app registration).
- `SendError` classified as `Retryable` (429/5xx/timeouts) vs `Fatal` (auth, invalid recipient) — drives retry logic.

Accounts are global app config (not per-project): `~/.config/massfckinmailer/accounts.toml` holds non-secret metadata; secrets live in keychain under `massfckinmailer/{account_id}`. Projects reference accounts by id + display name, so a shared project file leaks nothing.

## 6. Project file (TOML)

```toml
version = 1
name = "Spring launch"

[account]
id = "acct_9f3a"            # reference only — secrets stay in keychain
display = "Mailgun — news.example.com"

[template]
subject = "Hey {{first_name}}, {{product}} is live!"
html_path = "template.html"  # sibling file, editable in-app; keeps TOML clean
generate_text_alt = true

[recipients]
source_path = "list.xlsx"
sheet = "Sheet1"             # xlsx only
email_column = "E-mail"      # auto-detected, user-confirmable

[recipients.mapping]         # file column -> template field
first_name = "First Name"
product    = "Product"

[sending]
messages_per_second = 5      # clamped to provider capability
retry_limit = 3
stop_after_failures = 25     # circuit breaker, 0 = never
```

Saved as `project.mmproj.toml` next to `template.html`. Recipient data is referenced, never embedded.

## 7. Recipient import & field mapping

- CSV: streaming via `csv`; delimiter sniffing (`,` `;` `\t`). XLSX: `calamine`, first row = headers, sheet picker if multiple.
- **Email column detection**: scan first ~20 data rows per column with `^[^@\s]+@[^@\s]+\.[^@\s]{2,}$`; highest match-ratio column ≥ 0.8 wins; user can override in the mapping UI.
- Mapping UI: template fields (extracted by scanning the template for placeholders) on one side, file columns on the other; auto-match by normalized name (case/space/underscore-insensitive), rest mapped manually via dropdowns.
- Validation pass before send: rows with invalid/empty email or missing mapped fields are flagged in the preview table (skip / fix / abort). Duplicate emails flagged with a de-dupe toggle.

## 8. Templating

- `minijinja` with a restricted environment (no file access). Undefined variable = validation error before sending, not a silent blank.
- Pre-pass converts `##name##` → `{{name}}` so both syntaxes work.
- Editor: gpui-component code editor (HTML highlighting) side-by-side with live preview. Preview options: gpui-component's native simple-HTML renderer (light) with a wry WebView behind a feature flag for full-fidelity rendering. Preview renders with data from a selectable sample row.
- Subject line is a template too.

## 9. Send engine

State machine per campaign: `Idle → Validating → Running → (Completed | Cancelled | Stopped)`.

- Recipients feed a work queue; N workers (default: min(4, provider hint)) pull, render, send.
- `governor` rate limiter per account enforces `messages_per_second`.
- Retryable errors: exponential backoff (1s/4s/16s), max `retry_limit`; 429s also pause the limiter briefly.
- Circuit breaker: `stop_after_failures` consecutive hard failures aborts with a clear message (protects sender reputation).
- **Cancel**: token checked between sends; in-flight requests aborted; already-sent stays sent — UI copy must say "Cancel stops upcoming emails; X already delivered".
- Progress events: `{sent, failed, skipped, retrying, total, rate, eta}` + per-row status ring buffer for the live table.
- Outcome report: per-row status exportable as CSV (email, status, error, timestamp, provider message-id).
- **Resume**: a campaign outcome report can be re-loaded to re-run only failed/cancelled rows (planned — M7). Report format is designed for this from day one: keep the original row index + all mapped fields so resume needs no re-mapping.

## 10. UI flow

Left rail with four steps, acting as both nav and checklist (steps show ✓/! state):

1. **Accounts** — list + Add wizard per provider type; inline setup instructions (e.g. "how to get a Mailgun API key", Gmail OAuth walkthrough); Test connection button.
2. **Template** — subject field + HTML editor + live preview; placeholder chips inserted by click once a list is loaded.
3. **Recipients** — file drop/picker → mapping screen → validated preview in a virtualized table (fine with 100k rows).
4. **Send** — pre-flight summary card (account, subject, recipient count, est. duration, warnings) → big Send button → progress view: bar, counters, live per-row status, Cancel. Optional "send test to myself first".

Project menu: New / Open / Save / Recent. Dirty-state tracking with save prompt. First-run empty states explain each step (the "help" requirement, kept lightweight).

## 11. Milestones

| # | Milestone | Contents |
|---|---|---|
| M0 | Skeleton | Workspace, gpui window, nav rail, theme, tokio bridge thread + channel plumbing proven with a dummy async task |
| M1 | Accounts | Account model, keychain storage, SMTP + Mailgun impls, add-account UI, Test connection |
| M2 | Recipients | CSV/XLSX import, email detection, mapping UI, validation, virtualized preview table |
| M3 | Template | Editor + preview, minijinja rendering, placeholder extraction, sample-row preview |
| M4 | Send engine | Queue/workers/throttle/retry/cancel, progress UI, outcome report export — **end-to-end usable via SMTP/Mailgun** |
| M5 | Project files | TOML save/load, recent projects, dirty tracking |
| M6 | SES + OAuth providers | aws-sdk-sesv2; Gmail/Outlook OAuth PKCE + token refresh in keychain |
| M7 | Resume | Re-run failed/cancelled rows from an outcome report |
| M8 | Polish | Empty-state help, error message pass, packaging (msi/dmg/AppImage), app icon |

M1–M4 order front-loads risk: the tokio↔gpui bridge and the send engine are the novel parts; SES/OAuth are additive.

## 12. Risks & mitigations

- **gpui is pre-1.0**: pin exact versions of `gpui`/`gpui-component`; upgrade deliberately.
- **Gmail/Outlook OAuth friction**: users must register their own OAuth app; mitigate with step-by-step in-app instructions. Fallback: app-password SMTP works day one.
- **Deliverability/ToS**: Gmail/Outlook personal accounts have low daily caps (~500/day) and bulk sending can flag accounts — show provider-specific warnings in pre-flight.
- **WebView (wry) weight**: keep behind a feature flag; native simple-HTML preview is the lightweight default.
- **Huge lists**: streaming parse + virtualized table + bounded channels; never hold rendered emails for all rows in memory.

## 13. Resolved scope decisions

- **Attachments**: deferred — revisit when we can attach *and* preview them in the mail body (all providers support them, so it's purely additive).
- **Per-recipient BCC/CC columns**: out of scope for v1.
- **Resume from outcome report**: in scope — M7.
- **Unsubscribe-link / List-Unsubscribe header**: skipped for v1 (uneven support across account types).
