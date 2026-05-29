# Cadenza

[![CI](https://github.com/williammb/cadenza/actions/workflows/ci.yml/badge.svg)](https://github.com/williammb/cadenza/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)

> A local-first desktop task board for AI coding agents.

**Status:** public pre-release. The repository is open for development, but stable public binaries are not published yet.
**Version:** 0.1.5
**Stack:** Tauri 2, Rust, vanilla HTML/CSS/JS
**Release artifacts:** Windows NSIS first. Linux AppImage and macOS DMG are planned but not published yet.
**License:** MIT OR Apache-2.0

---

## What Cadenza Does

Cadenza is a desktop app that helps a human collaborate with AI coding
agents through a local task board. It ships two binaries from the same
installer:

- `cadenza`: the Tauri desktop app with tray, webview UI, notifications,
  task storage, terminal sessions, and update checks.
- `cadenza-cli`: the command-line client that agents use to read tasks,
  report progress, propose work, and mark work as done.

The app and CLI communicate on the same machine over an authenticated
named pipe on Windows or Unix socket on Linux/macOS. The protocol uses
NDJSON request/response messages. Cadenza does not open a TCP server for
agent communication.

Agents do not directly create or complete task scope. They submit
proposals, and the human accepts or rejects them in the app.

## Current Features

- Local task board with the states `a_fazer`, `fazendo`,
  `aguardando_revisao`, and `feito`.
- CLI for AI agents, with stable exit codes for automation.
- Idempotent proposal flow with persisted recovery data.
- PTY-backed agent runs and terminal streaming through Tauri channels.
- Projects and active-project tracking.
- Ideias inbox.
- File, SQLite, and PostgreSQL storage backends.
- PostgreSQL password storage through the OS keyring.
- Shared i18n through Fluent files in `locales/`.
- Vanilla HTML/CSS/JS UI with vendored browser libraries.
- Windows NSIS packaging path and signed updater plumbing.

---

## Architecture

```text
cadenza desktop app
  ui/                 static HTML/CSS/JS loaded by Tauri
  src-tauri/          Rust backend, tray, commands, store, IPC, PTY
        |
        | Tauri invoke / emit / Channel
        |
  webview UI

cadenza-cli
        |
        | NDJSON over named pipe / Unix socket
        |
cadenza IPC server
```

| Channel | Direction | Purpose |
|---|---|---|
| Tauri `invoke` | Frontend to backend | Task CRUD, projects, settings, decisions |
| Tauri `emit` | Backend to frontend | Real-time UI updates |
| Tauri `Channel` | Backend to frontend | PTY terminal streaming |
| Named pipe / Unix socket | CLI to app | Authenticated NDJSON protocol |

---

## Repository Layout

```text
cadenza/
  src-tauri/        Tauri app: tray, IPC server, store, triage, PTY, notify
  cadenza-cli/      clap CLI that talks to the app over the local socket
  proto/            shared NDJSON wire types
  i18n/             shared Fluent bundle loader and locale resolution
  locales/          .ftl files for app, CLI, and UI
  ui/               static HTML/CSS/JS
  ui/vendor/        vendored browser libraries
  skills/           per-locale agent skill snippets
  skills-core/      shared skill metadata and loader
  docs/             release and maintenance docs
  installers/       packaging support files
  Cargo.toml        workspace manifest
  deny.toml         cargo-deny policy
  AGENTS.md         Codex guidance for this repository
  CLAUDE.md         Claude Code guidance for this repository
```

---

## Requirements

- Rust stable >= 1.77
- Tauri CLI 2:

```bash
cargo install tauri-cli --version "^2" --locked
```

- Platform dependencies required by Tauri 2:
  - Windows: WebView2
  - Linux: WebKitGTK and AppIndicator/Ayatana development packages
  - macOS: Xcode command line tools

See the official Tauri prerequisites:
[tauri.app/start/prerequisites](https://tauri.app/start/prerequisites/)

The UI has no package-manager workflow and no frontend build step. Browser
libraries are vendored under `ui/vendor/` and documented in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

Major Rust dependencies include Tauri 2, Tokio, SQLx, Fluent, Interprocess,
portable-pty, keyring, clap, tracing, serde, and cargo-deny policy checks.
The authoritative dependency list is in [Cargo.toml](Cargo.toml) and
[Cargo.lock](Cargo.lock).

---

## Development

Run the desktop app:

```bash
cargo tauri dev
```

Build the current platform bundle:

```bash
cargo tauri build
```

Run the CLI from source:

```bash
cargo run -p cadenza-cli -- current --json
```

Run tests:

```bash
cargo test
```

Run lint and policy checks used by CI:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo deny check
```

---

## CLI Example

`cadenza-cli` requires the Cadenza app to be running.

```bash
cadenza-cli current --json
cadenza-cli list --estado doing
cadenza-cli log T-42 "implemented the handshake"
cadenza-cli propose --parent T-42 \
  --title "extract auth module" \
  --repro "..." \
  --file "src/auth/mod.rs" \
  --what-failed "..." \
  --action "..."
cadenza-cli done T-42 "merged, tests green"
```

`--estado` accepts English aliases (`todo`, `doing`, `review`, `done`).
JSON output uses the canonical Portuguese state values:
`a_fazer`, `fazendo`, `aguardando_revisao`, and `feito`.

### Exit Codes

| Code | Meaning |
|---:|---|
| 0 | ok |
| 1 | generic error |
| 2 | bad usage |
| 10 | app not running |
| 11 | bad or missing token |
| 12 | protocol mismatch |
| 20 | proposal rejected |
| 21 | decision timeout |
| 30 | task not found |

---

## Project Rules

- The UI is plain HTML/CSS/JS served by Tauri. Keep it framework-free and
  build-step-free.
- Backend, CLI, and UI share Fluent locale files. Do not add a second i18n
  system.
- The IPC protocol is NDJSON over local OS IPC, with `protocol_version`
  negotiated separately from `app_version`.
- `propose` must stay idempotent and resumable through client-generated
  idempotency keys.
- UI code must avoid `innerHTML` except in `ui/markdown.js`, where markdown
  output is sanitized before assignment.
- Dependencies must remain compatible with the repository license policy:
  MIT, Apache-2.0, BSD, or MPL-2.0. `cargo-deny` enforces this in CI.

Agent-specific implementation guidance lives in [AGENTS.md](AGENTS.md) and
[CLAUDE.md](CLAUDE.md).

---

## Privacy and Local Data

- Cadenza stores local app data under the user's Cadenza data directory,
  including task data, inbox items, proposals, logs, and the local auth token.
- The CLI token is used only for same-host communication between the app and
  CLI.
- PostgreSQL passwords are stored in the OS keyring when the PostgreSQL
  backend is used.
- Public release builds may contact the configured GitHub release endpoint
  to check signed updater metadata.

---

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a PR.

Portuguese is welcome for architecture discussions. English is fine for code,
comments, commits, issues, and PRs.

Security issues should be reported privately; see [SECURITY.md](SECURITY.md).

---

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.
