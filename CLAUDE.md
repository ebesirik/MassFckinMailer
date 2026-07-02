# MassFckinMailer — working guide

Cross-platform desktop mass mailer. Rust + gpui + gpui-component. Non-blocking,
smooth, lightweight. Design rationale and decisions live in [PLAN.md](PLAN.md);
this file is the day-to-day guide for working in the repo.

## Workspace (4 crates under `crates/`)

- **core** — pure domain logic; **no UI, no tokio**. Project file (TOML),
  templating (minijinja), CSV/XLSX import (csv/calamine), field mapping +
  row validation, app settings. Fully unit-tested.
- **providers** — the `EmailProvider` trait + impls: SMTP (lettre), Mailgun /
  Gmail / Outlook (reqwest), SES (aws-sdk-sesv2). Account model + `accounts.toml`
  store, OS keychain (`secrets`, keyring), OAuth (`oauth` — hand-rolled PKCE +
  loopback), and the `build_provider` factory.
- **engine** — the gpui↔tokio bridge (`MailRuntime` + flume channels) and the
  send engine (`run_campaign`: work queue, N workers, governor throttle, retry
  w/ backoff, cancel, circuit breaker), plus CSV outcome-report export/parse for
  resume. tokio-only.
- **app** — the gpui UI (`main_window.rs`) + i18n. Binary target `massfckinmailer`.

Dependencies: app → {core, engine, providers}; engine → {core, providers};
providers → core. Keep `core` pure.

## Commands

```
cargo run -p mmm-app                        # launch the app
cargo test --workspace                      # core/engine/providers unit tests
cargo clippy --workspace                    # keep this clean
cargo run -p mmm-app --example gen_icon     # regenerate assets/icon.{png,ico}
```

## Conventions & gotchas

- **gpui is pre-1.0.** `gpui`, `gpui-component`, `gpui-component-assets` are
  pinned **exactly** in the workspace `Cargo.toml` (gpui-component 0.5.1 ↔ gpui
  0.2.2). Upgrade all three together, deliberately.
- **gpui-component builder methods live on extension traits you must import** —
  a missing `use` gives a confusing "method not found":
  `.primary()/.danger()/.ghost()/.link()` → `button::ButtonVariants`;
  `.selected()` → `Selectable`; `.disabled()` → `Disableable`;
  `.with_size()` → `Sizable`. (`.outline()` is inherent.)
- **Async model.** One dedicated tokio-runtime thread (engine `MailRuntime`).
  UI → engine via `Command`, engine → UI via `Event`, both over flume; the UI
  drains events with `cx.spawn` on the foreground executor. File parsing runs on
  gpui's `background_executor`. Never block the UI thread.
- **Secrets** live only in the OS keychain (`providers::secrets`), keyed by
  account id. `accounts.toml` and project files hold non-secret references only.
- **Preferences** (UI language + theme) persist via `core::settings::AppSettings`
  to `{config_dir}/massfckinmailer/settings.toml`. Theme is Light / Dark / Auto;
  Auto follows the OS via `window.appearance()` + `observe_window_appearance`, and
  is applied with gpui-component's `Theme::change`.
- **i18n.** rust-i18n; all user-facing text goes through `t!` / the `tr(key)`
  helper. Strings live in `crates/app/locales/app.yml` (`_version: 2`,
  12 languages, English is the fallback). Helper fns like `field_label`,
  `labeled`, `kind_button`, `stat`, `summary_row` take an **i18n key**, not
  literal text. `crates/app/build.rs` has `rerun-if-changed=locales`; if
  translations look stale after editing the YAML, `touch crates/app/src/main.rs`.
  Do **not** add `#[test]` in the app **bin** crate that uses `t!` (hits a
  `#[test]` macro-expansion recursion) — verify translations at runtime instead.

## Verifying changes

Prefer real behavior over just tests. `cargo run -p mmm-app`; to smoke-test a
specific tab, temporarily set the initial `active:` section in
`MainWindow::new`, launch, confirm, then revert. The send engine's logic is
covered by mock-`EmailProvider` tests in `engine`.

Some things **cannot** be verified in this environment and need real
credentials/tooling: actual SMTP/Mailgun/SES delivery, Gmail/Outlook OAuth
(needs the user's own Google Cloud / Azure app + interactive browser consent),
and producing platform installers. See README for setup + packaging commands.

## CI & releases (`.github/workflows/`)

- `ci.yml` — fmt/clippy/test/build on Linux+macOS+Windows for `master`/`beta`/`dev`
  pushes and PRs.
- `channels.yml` — pushing `beta` or `dev` updates a rolling pre-release
  (`beta` → "Beta", `dev` → "nightly"): it force-moves the channel tag to the new
  commit and overwrites the per-platform assets. `master` is intentionally excluded.
- `release.yml` — a `v*` tag builds all platforms into a draft GitHub Release
  (stable). No secrets required; signing is stubbed/commented.
- **Version**: single source is `[workspace.package] version` in `Cargo.toml`.
  Pipelines compute `MFM_VERSION` (base + channel + Actions run number) and pass
  it to the build; `crates/app/build.rs` embeds it via `env!("MFM_VERSION")`
  (falling back to the crate version locally), shown as `APP_VERSION` in the
  sidebar. Bump the version in one place — `Cargo.toml`.

## Status

All milestones M0–M8 complete; the app is feature-complete per PLAN.md, and the
UI is fully localized. Remaining work is user-side (real sends, OAuth app
registration, installers).
