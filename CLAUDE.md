# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository state

**Phases 1–4 complete; Phase 5 partial.** The full IPC stack (named pipe/Unix socket, NDJSON), triage engine with idempotency/recovery, PTY/terminal, and the full task store (SQLite and PostgreSQL backends, ideias inbox, projects, agent runs) are implemented. Phase 5 (cross-platform packaging) has a working NSIS installer; AppImage and DMG are not yet produced. The Cargo workspace has five crates (`src-tauri`, `cadenza-cli`, `proto`, `i18n`, `skills-core`), the vanilla HTML/CSS/JS UI lives under `ui/` with vendored libs in `ui/vendor/`, and locale bundles are in `locales/`. `cargo build`, `cargo run -p cadenza-cli -- …`, and `cargo tauri dev` (after `cargo install tauri-cli --version "^2" --locked`) are real commands.

## Added scope

Features added after the original Phases 1–5 plan — all fully implemented:

- **SQLite and PostgreSQL backends.** The store is a `Repository` trait with three impls: `FileRepository` (legacy Node.js compat), `SqliteRepository`, and `PgRepository`. Backend is switchable at runtime via `set_storage_backend`; migrations handled by `store/migrate.rs`.
- **OS keyring for PG password.** `secrets.rs` uses the `keyring` crate to store/retrieve the PostgreSQL password from the OS credential store — never on disk in plaintext.
- **Ideias inbox.** File-backed inbox under `~/.cadenza/inbox/` (`store/ideias_inner.rs`). Tauri commands: `list_ideias`, `read_ideia`, `create_ideia`, `delete_ideia`, `set_ideia_status`, `destrinchar_ideia`.
- **Projects.** `projects.rs` + commands (`set_task_project`, `list_task_projects`, `set_active_project`) attach tasks to named projects and track the active project.
- **Agent runs.** `runs.rs` + commands (`start_task_agent`, `read_task_run`, `list_task_runs`, `clear_task_run`) spawn and track PTY-based agent sessions per task.

## What this project is

**Cadenza** is a planned **Tauri 2.x desktop rewrite** of an existing Node.js task-management tool (internal codename `task-ai` / `taskloop`). The UI is **vanilla HTML/CSS/JS** — no React, no Vite, no `node_modules`. It will ship two binaries from one installer:

- `cadenza` — Tauri app (tray icon, webview rendering hand-written HTML/CSS, Rust backend)
- `cadenza-cli` — CLI used by AI agents (Claude Code, Codex) to drive tasks

They communicate over a local **named pipe (Windows) / Unix socket** using **NDJSON** framing, authenticated by a token in `~/.cadenza/auth` (mode `0600`). The terminal stream uses **Tauri channels** (`ipc::Channel`) — no TCP ports are opened.

The project briefly explored Tauri+React and Slint before landing on the current Tauri+vanilla-JS shape on 2026-05-27. Do not re-propose either reversal (see the "Hard constraints" section below).

## Hard constraints (do not break)

These are decisions already made — re-deriving or "improving" them silently is a mistake:

- **On-disk data format is frozen** for compatibility with the existing Node.js version sharing the same `~/.cadenza/` directory. That means:
  - Task state names stay in Portuguese canonical: `a_fazer`, `fazendo`, `aguardando_revisao`, `feito`.
  - YAML frontmatter field names stay in PT: `id`, `titulo`, `estado`, `responsavel`.
  - Folder paths stay in PT: `~/.cadenza/triage/`, `~/.cadenza/logs/`, `~/.cadenza/auth`.
  - IPC event names stay in PT: `proposta_pendente`, `proposta_decidida`.
  - `--json` CLI output always emits PT canonical values for parsing stability.
- **i18n is unified under `fluent-rs`.** Backend, CLI, and UI all consume the same `.ftl` files. UI loads strings via a Tauri command (`load_translations(locale)` at boot returns a dict; `t(key, args)` for plural-y cases that need backend resolution). English is the fallback; pt-BR is primary. Logs are always English. Don't introduce a second i18n system (no react-i18next, no gettext .po) — one toolchain only.
- **CLI accepts EN aliases for enum values** (`--estado todo|doing|review|done`) mapped to PT canonical in `cadenza-cli/src/aliases.rs`. Subcommands themselves are English (`list`, `current`, `propose`, `done`).
- **No MCP.** Agent integration is CLI-based by design — debuggable, agent-agnostic. Don't propose adding MCP.
- **No JSON-RPC.** Wire format is NDJSON with `{v, id, op, args}` requests and `{v, id, ok, result|error}` responses — don't switch to jsonrpsee.
- **No JS framework, no `node_modules`, no build step for the UI.** UI is hand-written HTML/CSS/JS in `ui/`, served as static files by Tauri (`withGlobalTauri: true` in `tauri.conf.json`, accessed via `window.__TAURI__`). Don't propose React, Vue, Svelte, Vite, npm, Webpack — that decision was made and reversed in v2. Third-party JS libs (xterm.js, marked, DOMPurify) are vendored under `ui/vendor/` with pinned versions.
- **No Slint, no native Rust GUI framework.** v3 explored Slint and reversed it because GPLv3 blocks enterprise adoption. UI is Tauri webview. Don't re-propose iced/egui/Dioxus.
- **License: MIT OR Apache-2.0.** Every `Cargo.toml` declares it; repo has both `LICENSE-MIT` and `LICENSE-APACHE`. **All dependencies must be MIT, Apache-2.0, BSD, or MPL-2.0** — no GPL/LGPL/AGPL ever. `cargo-deny` enforces this in CI.
- **XSS hygiene in the UI.** `innerHTML` is forbidden everywhere except the markdown renderer in `modal.js` (which sanitizes with DOMPurify). Use `textContent` + `createElement`/`append` for everything else. A lint rule blocks `.innerHTML =` in PRs with an explicit allowlist for the markdown path.
- **`protocol_version` is separate from `app_version`** and negotiated in the `hello` handshake. The app keeps a `MIN_PROTOCOL`/`MAX_PROTOCOL` window of one deprecation release.
- **`propose` must be idempotent and resumable** — every proposal carries a client-generated `idempotency_key` (uuid v4) and is persisted under `~/.cadenza/triage/<id>.proposta.json` before any response. Either side can crash and reconnect.
- **CLI ships inside the app installer.** Never distributed separately — this guarantees CLI and app share the same protocol version on a given host.

## Exit codes (stable CLI contract)

Agents inspect these directly without parsing stdout. Don't renumber:

`0` ok · `1` generic · `2` bad usage · `10` app not running · `11` bad/missing token · `12` protocol mismatch · `20` proposal rejected · `21` decision timeout · `30` task not found.

## Architectural shape

Cargo workspace with five Rust crates plus a static-files UI directory — no frontend toolchain:

```
src-tauri/   - Tauri app: tray, IPC server, store, triage, PTY, notifications, Tauri commands
ui/          - hand-written HTML/CSS/JS, served as static files by Tauri
  vendor/    - pinned JS libs: xterm.js, marked, DOMPurify (vendored, no npm)
cadenza-cli/ - clap-based CLI client over the local socket
proto/       - shared NDJSON types (path dep from both Rust crates)
i18n/        - shared Fluent bundle loader + locale resolution chain
skills-core/ - shared skill snippet metadata + loader
locales/     - .ftl files for UI, app, and CLI (pt-BR, en)
skills/      - per-locale skill snippet handed to the agent
installers/  - tauri build outputs: NSIS / AppImage / DMG (Phase 5)
LICENSE-MIT, LICENSE-APACHE, deny.toml at repo root
```

The boundary between `store`/`triage`/`ipc`/`commands` in `src-tauri/src/` is intentional — read the existing modules before adding a new one. `commands.rs` holds `#[tauri::command]` handlers (UI ↔ backend); it does not own business logic. `triage` owns proposal idempotency and recovery; `store` owns persistence; `ipc` owns the NDJSON server and framing.

## Locale resolution chain

`flag → env (CADENZA_LANG) → config.json → OS locale (sys-locale) → en`. App and CLI share this logic (in `locale.rs`). `pt_PT` falls through to `pt-BR` (only PT variant packaged).

## Document conventions

Portuguese is the project's working language for architecture discussions; English is fine for code, comments, commits and PRs. When adding constraints to this file of comparable weight to the existing "Hard constraints", land them as a new bullet there — not as scattered footnotes elsewhere.

---

# 12-Rule Operating Manual

These rules apply to every task in this project unless explicitly overridden.
Bias: caution over speed on non-trivial work. Use judgment on trivial tasks.

## Rule 1 — Think Before Coding
State assumptions explicitly. If uncertain, ask rather than guess.
Present multiple interpretations when ambiguity exists.
Push back when a simpler approach exists.
Stop when confused. Name what's unclear.

## Rule 2 — Simplicity First
Minimum code that solves the problem. Nothing speculative.
No features beyond what was asked. No abstractions for single-use code.
Test: would a senior engineer say this is overcomplicated? If yes, simplify.

## Rule 3 — Surgical Changes
Touch only what you must. Clean up only your own mess.
Don't "improve" adjacent code, comments, or formatting.
Don't refactor what isn't broken. Match existing style.

## Rule 4 — Goal-Driven Execution
Define success criteria. Loop until verified.
Don't follow steps. Define success and iterate.
Strong success criteria let you loop independently.

## Rule 5 — Use the model only for judgment calls
Use me for: classification, drafting, summarization, extraction.
Do NOT use me for: routing, retries, deterministic transforms.
If code can answer, code answers.

## Rule 6 — Token budgets are not advisory
Per-task: 4,000 tokens. Per-session: 30,000 tokens.
If approaching budget, summarize and start fresh.
Surface the breach. Do not silently overrun.

## Rule 7 — Surface conflicts, don't average them
If two patterns contradict, pick one (more recent / more tested).
Explain why. Flag the other for cleanup.
Don't blend conflicting patterns.

## Rule 8 — Read before you write
Before adding code, read exports, immediate callers, shared utilities.
"Looks orthogonal" is dangerous. If unsure why code is structured a way, ask.

## Rule 9 — Tests verify intent, not just behavior
Tests must encode WHY behavior matters, not just WHAT it does.
A test that can't fail when business logic changes is wrong.

## Rule 10 — Checkpoint after every significant step
Summarize what was done, what's verified, what's left.
Don't continue from a state you can't describe back.
If you lose track, stop and restate.

## Rule 11 — Match the codebase's conventions, even if you disagree
Conformance > taste inside the codebase.
If you genuinely think a convention is harmful, surface it. Don't fork silently.

## Rule 12 — Fail loud
"Completed" is wrong if anything was skipped silently.
"Tests pass" is wrong if any were skipped.
Default to surfacing uncertainty, not hiding it.
