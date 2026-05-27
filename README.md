# Cadenza

> A system-tray desktop app for managing AI-agent tasks — built for Claude Code, Codex, and similar tools.

**Status:** under development (Phase 1 — foundation). No public release yet.
**Stack:** Tauri 2 · Rust · vanilla HTML/CSS/JS (no build step)
**Target platforms:** Windows · Linux · macOS
**License:** MIT OR Apache-2.0

---

## What it is

Cadenza is a rewrite of the internal `task-ai` / `taskloop` system (Node.js)
as a **standalone desktop app**. Instead of two loose processes accessed
through a browser via `start.bat`, Cadenza ships a single binary with:

- A system-tray icon — no leftover terminal window.
- The OS's native webview (no Electron).
- A separate CLI (`cadenza-cli`) that AI agents use to read the active
  task, report progress, propose new scope, and mark work as done.
- Native OS notifications when the agent needs a human decision.
- Signed auto-updates (ed25519) with automatic rollback on failure.

The human ↔ agent loop is mediated by **proposals**: the agent never
creates or completes tasks on its own — it proposes, and the human decides
through the modal or the notification's action buttons.

## Why it exists

The previous Node.js flow depended on keeping two background servers
running, depended on the browser, and ended up orphaned whenever a
terminal got closed. Cadenza solves that with a single native process
that boots with the OS, listens for the agent on an authenticated local
socket, and keeps on-disk state in the same format as the old system —
so they can coexist during the migration.

---

## Architecture at a glance

```
┌─────────────────────────────────────────────────────┐
│              cadenza (Tauri 2.x)                    │
│                                                     │
│  ┌────────────┐    ┌──────────────────────────────┐ │
│  │ Tray Icon  │    │   WebView (vanilla HTML/JS)  │ │
│  │ (Rust)     │    │   Board · Modal · Terminal   │ │
│  └────────────┘    └──────┬───────────────────────┘ │
│        │                  │ invoke / emit / channel │
│  ┌─────▼──────────────────▼─────────────────────┐   │
│  │           Rust backend                       │   │
│  │  config · store · triage · spawn · terminal  │   │
│  │  commands · ipc · notify · updater · i18n    │   │
│  └────────────────────┬─────────────────────────┘   │
│                       │ NDJSON over                  │
│                       │ named pipe / unix socket     │
└───────────────────────┼─────────────────────────────┘
                        │
              ┌─────────▼─────────┐
              │   cadenza-cli     │
              │   used by the     │
              │   AI agent        │
              └───────────────────┘
```

| Channel | Direction | Purpose |
|---|---|---|
| Tauri `invoke` | Frontend → Backend | Task CRUD, projects, decisions |
| Tauri `emit` | Backend → Frontend | Real-time updates |
| Tauri `Channel` | Backend ⇄ Frontend | Binary PTY streaming (no WebSocket) |
| Named pipe / Unix socket | CLI ↔ App | Authenticated NDJSON request/response |

---

## Repository layout

```
cadenza/
├── src-tauri/        # Tauri app: tray, IPC server, store, triage, PTY, notify
├── cadenza-cli/      # clap CLI that talks to the app over the local socket
├── proto/            # Shared NDJSON wire types (path dep)
├── i18n/             # Shared Fluent bundle loader + locale resolution
├── locales/          # .ftl files for app, CLI and UI (pt-BR, en)
├── ui/               # Hand-written HTML/CSS/JS; vendored libs in ui/vendor/
├── skills/           # Per-locale skill snippet handed to the agent
├── skills-core/      # Shared skill metadata + loader
├── Cargo.toml        # Workspace manifest
├── deny.toml         # cargo-deny policy (MIT/Apache/BSD/MPL only)
└── CLAUDE.md         # Non-negotiable constraints + 12-rule operating manual
```

The non-negotiable constraints (frozen on-disk format, no MCP/JSON-RPC,
exit codes, license policy, XSS hygiene) live in `CLAUDE.md`. When the
README and `CLAUDE.md` disagree, `CLAUDE.md` wins. Portuguese is the
project's primary working language for architecture discussions.

---

## Getting started (development)

> No public release yet. These instructions are for building from source.

### Prerequisites

- Rust stable ≥ 1.77 (`rustup toolchain install stable`)
- Tauri CLI 2 (`cargo install tauri-cli --version "^2" --locked`)
- The OS toolchain Tauri 2 needs — see
  [tauri.app/start/prerequisites](https://tauri.app/start/prerequisites/)
  (WebView2 on Windows, `webkit2gtk` on Linux, Xcode CLI on macOS).

No Node.js, no `pnpm`, no `node_modules` — the UI is plain HTML/CSS/JS
served by Tauri as static files, and third-party JS libs are vendored
under `ui/vendor/` with pinned versions.

### Run in dev

```bash
cargo tauri dev   # launches the app; reload UI changes with Ctrl+R
```

### Production build

```bash
cargo tauri build # builds the installer for the current platform
```

### CLI only

```bash
cargo run -p cadenza-cli -- current --json
```

---

## CLI usage (example)

`cadenza-cli` ships inside the app installer and is added to the user's
PATH. It requires the Cadenza app to be running.

```bash
cadenza-cli current --json                          # active task (or null)
cadenza-cli list --estado doing                     # filter by state
cadenza-cli log T-42 "implemented the handshake"    # record progress
cadenza-cli propose --parent T-42 \
        --title "extract auth module" \
        --repro "..." --file "src/auth/mod.rs" \
        --what-failed "..." --action "..."          # blocks until decided
cadenza-cli done T-42 "merged, tests green"         # mark as done
```

`--estado` accepts English aliases (`todo | doing | review | done`); the
`--json` output always returns the canonical Portuguese values
(`a_fazer`, `fazendo`, `aguardando_revisao`, `feito`) for stable parsing.

### Exit codes (stable contract for agents)

`0` ok · `1` generic error · `2` bad usage · `10` app not running ·
`11` bad/missing token · `12` protocol mismatch · `20` proposal rejected ·
`21` decision timeout · `30` task not found.

---

## IPC protocol

- **Transport:** named pipe `\\.\pipe\cadenza-{sid}` on Windows; Unix
  socket at `$XDG_RUNTIME_DIR/cadenza.sock` on Linux/macOS (mode `0600`).
- **Framing:** NDJSON — one JSON message per line.
- **Authentication:** a 32-byte token at `~/.cadenza/auth` (`0600`),
  shared between the app and the CLI on the same host. Rotatable from
  the tray menu.
- **Versioning:** `protocol_version` is separate from `app_version` and
  negotiated in the `hello` handshake. The app keeps a one-release
  deprecation window.
- **`propose` is idempotent and resumable:** every proposal carries an
  `idempotency_key` (UUID v4) and is persisted at
  `~/.cadenza/triage/<id>.proposta.json` before any response — either
  side can crash and reconnect without duplicating tasks.

The full NDJSON protocol — handshake, ops, error envelopes — is defined
in the `proto/` crate; the canonical wire values are PT-canonical.

---

## Internationalization

- **Display layer only.** On-disk data and wire values stay in canonical
  Portuguese.
- **One toolchain everywhere.** Backend, CLI and UI all consume the same
  `.ftl` files via `fluent-rs` (embedded with `include_dir!`). The UI
  loads its strings at boot through a Tauri command (`load_translations`)
  and renders them with a small `t(key, args)` helper — no
  `react-i18next`, no second i18n system.
- **Locales at launch:** `pt-BR` (primary), `en` (fallback). `pt_PT`
  falls through to `pt-BR`.
- **Resolution chain:** `--lang` > `CADENZA_LANG` > `config.json` >
  OS locale (`sys-locale`) > `en`.
- **Logs are always in English**, regardless of the active locale.

---

## Design decisions already settled

A few decisions are locked in, and silently re-deriving them is treated
as a mistake. Summary of what's in `CLAUDE.md`:

- **On-disk format is frozen** so Cadenza can coexist with the Node.js
  version (state names and frontmatter fields stay in canonical
  Portuguese).
- **No MCP.** Agent integration is CLI-based — debuggable and
  agent-agnostic.
- **No JSON-RPC.** Wire format is plain NDJSON with `{v, id, op, args}`.
- **The CLI ships inside the app installer** — never distributed
  separately. That guarantees the CLI and app on a given host speak the
  same `protocol_version`.
- **No open TCP port.** PTY streaming uses Tauri channels instead of a
  local WebSocket.

The rationale for each one is in `CLAUDE.md`.

---

## Roadmap

- [ ] **Phase 1 — Foundation:** Cargo workspace, Tauri boilerplate,
  `config`/`store`/`observ`/`i18n` wired from the first boot.
- [ ] **Phase 2 — Core backend:** `triage` (idempotency + recovery),
  `spawn` (PTY via `portable-pty`), `terminal` (channel + ring buffer).
- [ ] **Phase 3 — Frontend integration:** `commands.rs`, `ui/api.js`
  built on `window.__TAURI__`, terminal on a Tauri channel, language
  switcher.
- [ ] **Phase 4 — CLI + IPC + Notifications:** `proto`/`auth`/`ipc`,
  `cadenza-cli` with clap, `notify` with actions, per-locale skill,
  end-to-end test with a real agent.
- [ ] **Phase 5 — Packaging and updates:** icons,
  NSIS/AppImage/DMG, cross-platform CI with ed25519 signing,
  `tauri-plugin-updater` pointed at a public endpoint, smoke tests on
  all three OSes.

---

## Contributing

Read `CLAUDE.md` before opening a PR — it spells out the non-negotiable
constraints (frozen on-disk format, no MCP/JSON-RPC, stable exit codes,
license policy, XSS hygiene) and the 12-rule operating manual that
applies to both humans and agents.

Portuguese is the project's working language for architecture
discussions; English is fine for code, comments, commits and PRs.

---

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE),
at your option.
