# MassFckinMailer

Cross-platform desktop mass mailer in Rust — gpui + gpui-component. Non-blocking,
smooth, lightweight. See [PLAN.md](PLAN.md) for the full design.

## Features

- **Providers**: generic SMTP, Mailgun, AWS SES, Gmail (OAuth), Outlook/Graph
  (OAuth). Credentials live in the OS keychain, never in project files.
- **Templates**: HTML editor with live preview and `{{field}}` / `##field##`
  placeholders (rendered with a strict minijinja environment — undefined fields
  are caught before sending). Auto-generated plain-text alternative.
- **Recipients**: streaming CSV + Excel/ODS import, automatic email-column
  detection, fuzzy field mapping, per-row validation, and a virtualized preview
  table that handles very large lists.
- **Send engine**: work queue + workers, per-account throttling, retry with
  backoff, a consecutive-failure circuit breaker, cooperative cancel, and live
  coalesced progress — all on a dedicated tokio runtime bridged to the UI over
  flume channels.
- **Outcome reports**: per-row CSV (status, error, provider message id) that can
  be re-loaded to **resume** only the failed/cancelled rows.
- **Projects**: human-readable TOML save/load, recent projects, dirty-state
  tracking with a discard prompt.

## Build & run

Requires stable Rust ≥ 1.85 (`rustup update`).

```
cargo run -p mmm-app          # launch the app
cargo test --workspace        # pure logic + engine bridge + providers
cargo clippy --workspace
```

## Packaging

The app icon lives at [`assets/icon.svg`](assets/icon.svg); the raster/ICO
versions are regenerated from it with:

```
cargo run -p mmm-app --example gen_icon   # writes assets/icon.{png,ico}
```

On Windows the icon is embedded in the `.exe` automatically at build time
(`crates/app/build.rs` via `winresource`). Installer builds use the
`[package.metadata.bundle]` metadata in `crates/app/Cargo.toml`:

| Target | Tool | Command |
|---|---|---|
| Windows `.msi` | [`cargo-wix`](https://github.com/volks73/cargo-wix) (needs the WiX Toolset) | `cargo wix -p mmm-app` |
| macOS `.app`/`.dmg` | [`cargo-bundle`](https://github.com/burtonageo/cargo-bundle) | `cargo bundle -p mmm-app --release` |
| Linux `.deb` | `cargo-bundle` | `cargo bundle -p mmm-app --release` |
| Linux AppImage | [`cargo-appimage`](https://github.com/StratusFearMe21/cargo-appimage) | `cargo appimage` |

Each installer must be produced on (or cross-compiled for) its own platform.

## OAuth setup (Gmail / Outlook)

These providers require you to register your own OAuth app (no secret is shipped):

- **Gmail** — Google Cloud: enable the Gmail API, create an OAuth client of type
  *Desktop app*, and paste its client ID + secret when adding the account.
- **Outlook** — Azure: register an app, add the delegated **Mail.Send**
  permission, and under *Mobile and desktop applications* add redirect URI
  `http://localhost` (no client secret needed — public client + PKCE).

"Connect & authorize" opens your browser; tokens are stored in the keychain and
refreshed automatically.

## Version pinning

`gpui` is pre-1.0 and breaks between versions. `gpui`, `gpui-component`, and
`gpui-component-assets` are pinned exactly in the workspace `Cargo.toml`
(gpui-component 0.5.1 ↔ gpui 0.2.2) — upgrade all together, deliberately.
