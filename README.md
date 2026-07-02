# MassFckinMailer

Cross-platform desktop mass mailer in Rust — gpui + gpui-component. See [PLAN.md](PLAN.md) for the full design.

## Status: M0 (skeleton)

- Cargo workspace: `core` (domain, pure), `providers` (trait contract), `engine` (tokio bridge), `app` (gpui UI)
- Main window with sidebar navigation (Accounts / Template / Recipients / Send)
- gpui↔tokio bridge proven: the Send panel runs a simulated 200-email campaign with live progress + cancel, driven by a dedicated tokio runtime thread over flume channels

## Build & run

Requires stable Rust ≥ 1.85 (`rustup update`).

```
cargo run -p mmm-app
```

Tests (pure logic + bridge):

```
cargo test --workspace --exclude mmm-app
```

## Version pinning

`gpui` is pre-1.0 and breaks between versions. `gpui`, `gpui-component`, and
`gpui-component-assets` are pinned exactly in the workspace `Cargo.toml`
(gpui-component 0.5.1 ↔ gpui 0.2.2) — upgrade all together, deliberately.
