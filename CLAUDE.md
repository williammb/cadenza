# CLAUDE.md

This file gives Claude Code repository-specific guidance for Cadenza.

## Current Project

Cadenza is a Tauri 2 desktop app plus a companion CLI for local AI-agent task
management. The committed project contains:

- `src-tauri/`: Tauri app backend, tray integration, commands, IPC server,
  storage, triage, PTY handling, notifications, updater integration, and app
  settings.
- `cadenza-cli/`: clap-based CLI used by agents to communicate with the app.
- `proto/`: shared NDJSON protocol types.
- `i18n/`: shared Fluent bundle loading and locale resolution.
- `skills-core/`: shared agent-skill metadata and loader.
- `ui/`: static vanilla HTML/CSS/JS frontend.
- `ui/vendor/`: vendored browser libraries.
- `locales/`: Fluent locale files.
- `skills/`: per-locale agent skill snippets.

The workspace version is `0.2.0`. Windows NSIS packaging is implemented.
Linux AppImage and macOS DMG packaging are planned but not published yet.

## Main Commands

```bash
cargo build
cargo test
cargo run -p cadenza-cli -- current --json
cargo tauri dev
cargo tauri build
```

Install the Tauri CLI when needed:

```bash
cargo install tauri-cli --version "^2" --locked
```

CI also runs:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo deny check
```

## Architecture Boundaries

- `commands.rs` owns Tauri command handlers. Keep business logic in the
  dedicated modules it delegates to.
- `triage` owns proposal idempotency, decisions, and recovery.
- `store` owns persistence. Current backends are file, SQLite, and
  PostgreSQL.
- `ipc` owns the local NDJSON server and framing.
- `terminal`, `spawn`, and `runs` own PTY sessions and agent-run tracking.
- `projects` owns project assignment and active-project state.
- `secrets` owns OS-keyring interaction for PostgreSQL passwords.

Read the immediate module and caller context before changing shared behavior.

## Protocol and Data Contracts

- CLI and app communicate over a local named pipe on Windows or Unix socket on
  Linux/macOS.
- Wire format is NDJSON with `{v, id, op, args}` requests and
  `{v, id, ok, result|error}` responses.
- `protocol_version` is negotiated separately from `app_version`.
- `propose` must remain idempotent and resumable. Each proposal carries a
  client-generated idempotency key.
- CLI exit codes are stable: `0` ok, `1` generic, `2` bad usage,
  `10` app not running, `11` bad or missing token, `12` protocol mismatch,
  `20` proposal rejected, `21` decision timeout, `30` task not found.
- JSON CLI output uses canonical Portuguese task states:
  `a_fazer`, `fazendo`, `aguardando_revisao`, `feito`.
- `--estado` accepts English aliases through `cadenza-cli/src/aliases.rs`.

## Frontend Rules

- The UI is static HTML/CSS/JS in `ui/`; keep it framework-free and
  build-step-free.
- Use Tauri's global API through `window.__TAURI__`.
- Third-party browser libraries are vendored in `ui/vendor/`.
- `innerHTML` is allowed only in `ui/markdown.js`, where markdown output is
  sanitized through DOMPurify before assignment. Everywhere else, use
  `textContent`, `createElement`, and `append`.

## Internationalization

- Backend, CLI, and UI share Fluent resources from `locales/`.
- The locale resolution chain is:
  `--lang` / flag, `CADENZA_LANG`, config, OS locale, then `en`.
- `pt-BR` is the primary locale; `en` is the fallback.
- Logs stay in English.
- Do not add a second i18n system.

## Dependencies and License Policy

- Rust dependencies are declared through the Cargo workspace.
- Major dependencies include Tauri 2, Tokio, SQLx, Fluent, Interprocess,
  portable-pty, keyring, clap, tracing, serde, and uuid.
- Browser dependencies are vendored under `ui/vendor/` and documented in
  `THIRD_PARTY_NOTICES.md`.
- Project license is `MIT OR Apache-2.0`.
- Allowed dependency licenses are MIT, Apache-2.0, BSD, and MPL-2.0.
  `cargo-deny` enforces the policy in CI.

## Documentation Style

Keep public documentation focused on what exists in this repository: current
architecture, commands, dependencies, release status, and user-visible
contracts. Do not add references to implementation history, non-public plans,
or external systems that are not part of the committed project.
