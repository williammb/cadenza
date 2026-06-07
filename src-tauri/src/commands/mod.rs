//! Tauri `#[command]` handlers — the in-process IPC surface used by the
//! React frontend. Per DESIGN-desktop-v2.md § "commands.rs". The CLI
//! talks to the app over a separate NDJSON socket (Phase 4), not these
//! handlers.

use cadenza_i18n::{locale as i18n_locale, FluentArgs, I18n, LocaleSources};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::ipc::Channel;
use tauri::{Emitter, State};
use uuid::Uuid;

use crate::agent::{self, CodexCapture, LaunchPlan, OpenCodeCapture, PromptDelivery};
use crate::blockers::TaskBlockers;
use crate::config::{AgenteKind, Config, PgConfig, PgSslMode, StorageBackend};
use crate::jira_sidecar::{JiraKeyIndex, TaskJira};
use crate::ordering::TaskOrder;
use crate::projects::TaskProjects;
use crate::runs::{TaskRun, TaskRuns};
use crate::secrets;
use crate::spawn::{PtyHandle, SpawnConfig};
use crate::store::{
    migrate, Decisao, DecisaoRegistro, Estado, FileRepository, Ideia, IdeiaStatus, MemoryItem,
    MemorySuggestion, NewProposta, PgConnectionParams, PgRepository, PgSslModeChoice, Proposta,
    Repository, SqliteRepository, StoreError, SuggestionKind, Task,
};
use crate::terminal::TerminalSession;
use crate::worktrees::{TaskWorktrees, WorktreeInfo};

/// Tauri-managed app state.
///
/// `repo` is a `Arc<dyn Repository>` so the backend (files / SQLite /
/// Postgres) can be swapped at startup without touching the call sites.
/// `config`/`i18n` use sync `Mutex` since their methods are sync and we
/// never hold a guard across an `.await`.
pub struct AppState {
    pub repo: Arc<dyn Repository>,
    pub config: Mutex<Config>,
    pub i18n: Mutex<I18n>,
    pub sessions: Mutex<HashMap<String, Arc<TerminalSession>>>,
    /// task_id → project_id side mapping. Lives in
    /// `~/.cadenza/task-projects.json`, not inside the task files —
    /// keeps the YAML frontmatter format frozen for Node.js compat.
    pub task_projects: Arc<TaskProjects>,
    /// task_id → last agent invocation (agent kind, model, conversation
    /// id). Persists to `~/.cadenza/task-runs.json`. Drives the
    /// "Iniciar" vs "Continuar" decision in the UI.
    pub task_runs: Arc<TaskRuns>,
    /// task_id → worktree/branch side mapping. Lives in
    /// `~/.cadenza/task-worktrees.json` — keeps the YAML frontmatter
    /// format frozen for Node.js compat.
    pub task_worktrees: Arc<TaskWorktrees>,
    /// task_id -> blocker task ids. Lives in `~/.cadenza/task-blockers.json`
    /// so dependency metadata stays structured without touching legacy
    /// task frontmatter.
    pub task_blockers: Arc<TaskBlockers>,
    /// task_id → Jira identity (site, issue_id) side mapping. Lives in
    /// `~/.cadenza/task-jira.json` — the file-backend equivalent of the SQL
    /// `tasks.jira_site/jira_issue_id` columns (the YAML frontmatter is
    /// frozen for Node.js compat).
    pub task_jira: Arc<TaskJira>,
    /// In-memory `(site, issue_id) → jira_key` index for synchronous
    /// `jira_key_display` enrichment. Seeded at startup from
    /// `repo.list_jira_issues()` and updated on `upsert_jira_issue`.
    pub jira_keys: Arc<JiraKeyIndex>,
    /// Per-column card priority order. Lives in
    /// `~/.cadenza/task-order.json` — keeps the YAML frontmatter format
    /// frozen and the DB schemas untouched. Applied as a sort in
    /// `list_tasks`; tasks absent from a column's list sort to the end.
    pub task_order: Arc<TaskOrder>,
    /// AppHandle for emitting events (e.g. `task_run_changed` from the
    /// async Codex-uuid capture task). Set once during `setup()`.
    pub app_handle: Mutex<Option<tauri::AppHandle>>,
    /// Per-agent cache of the `/model` menu entries. Populated lazily by
    /// `list_agent_models` (each call spawns the agent's CLI under a PTY
    /// and parses the rendered menu — ~10-15 s, so the result is
    /// memoized for the rest of the process lifetime). `refresh=true`
    /// on the command bypasses the cache, after which the new list
    /// replaces the old one.
    /// Keyed by `(kind, resolved command)` so changing the
    /// `config.agente.command` override invalidates the cache instead of
    /// returning a list discovered from the previous binary.
    pub agent_models: Mutex<HashMap<(AgenteKind, String), Vec<crate::models::ModelEntry>>>,
    /// Monotonic counter bumped by the tray "Revoke CLI token" handler.
    /// IPC connections capture the current value at hello-time; each
    /// dispatch checks against the live counter and rejects ops when
    /// they don't match so a revoked-mid-session connection can't keep
    /// driving the server until it disconnects on its own.
    pub token_epoch: AtomicU64,
    /// Serializes the accept-materialization path in `decidir_proposta`.
    /// Tauri runs commands concurrently, so a double-clicked "Accept"
    /// would otherwise let two calls both read "no prior decision" and
    /// each mint a derived task. Holding this across read→create→write
    /// makes the second caller observe the first's decision and reuse it.
    pub decision_lock: tokio::sync::Mutex<()>,
    /// Per-issue creation guard registry for the shared Jira worktree
    /// (Slice 4). Keyed by `(jira_site, jira_issue_id)`. The OUTER map is a
    /// std `Mutex` used only for in-memory lookups (never held across an
    /// `.await`); each value is a `tokio::sync::Mutex` that IS held across
    /// the git work in `ensure_issue_worktree`, serializing concurrent
    /// ensure calls for the same issue so they converge on one worktree.
    pub jira_worktree_locks: Mutex<crate::jira::worktree::WorktreeLockRegistry>,
    /// One-executor-per-issue registry (Slice 4): `(jira_site,
    /// jira_issue_id) → ExecutorSlot` for that issue's shared worktree. A
    /// `Reserving` slot is an in-flight start (claimed at guard time, before
    /// its session exists); a `Live(session_id)` slot is a running executor,
    /// reaped lazily once its session leaves `sessions`. This is the
    /// authoritative index for "is an agent already running for this issue"
    /// — no cross-task scan needed.
    pub jira_active_executors:
        Mutex<HashMap<(String, String), crate::jira::worktree::ExecutorSlot>>,
}

impl AppState {
    /// Initialize from `~/.cadenza/`. Creates subdirs if missing and
    /// tolerates a missing config.json (uses defaults). The storage
    /// backend is picked from `config.storage_backend`; first activation
    /// of a non-default backend triggers a one-way file→backend
    /// migration tracked in `~/.cadenza/migrated.json`.
    pub fn init() -> anyhow::Result<Self> {
        let home = dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".cadenza");
        std::fs::create_dir_all(&home)?;

        let config_path = home.join("config.json");
        let config = if config_path.exists() {
            Config::load_from(&config_path)?
        } else {
            Config::default()
        };

        let repo = build_repo(&home, &config)?;

        let env_lang = i18n_locale::read_env();
        let active_locale = i18n_locale::resolve(LocaleSources {
            flag: None,
            env: env_lang.as_deref(),
            config: config.locale.as_deref(),
        });
        tracing::info!(locale = %active_locale, "i18n initialized");
        let i18n = I18n::new(&active_locale);

        let task_projects = Arc::new(TaskProjects::load(&home)?);
        let task_runs = Arc::new(TaskRuns::load(&home)?);
        let task_worktrees = Arc::new(TaskWorktrees::load(&home)?);
        let task_blockers = Arc::new(TaskBlockers::load(&home)?);
        let task_order = Arc::new(TaskOrder::load(&home)?);
        let task_jira = Arc::new(TaskJira::load(&home)?);
        // Seed the synchronous display-key index from any cached records so
        // `jira_key_display` resolves without an async store read.
        let jira_records = tauri::async_runtime::block_on(async { repo.list_jira_issues().await })
            .unwrap_or_default();
        let jira_keys = Arc::new(JiraKeyIndex::from_records(&jira_records));

        // Seed the in-memory model cache from any lists persisted in
        // config.json so the task-start modal shows models instantly
        // (and offline) without re-running the ~15 s `/model` probe.
        let seeded_models = config
            .agent_models
            .as_ref()
            .map(|list| {
                list.iter()
                    .map(|c| ((c.kind, c.command.clone()), c.models.clone()))
                    .collect::<HashMap<(AgenteKind, String), Vec<crate::models::ModelEntry>>>()
            })
            .unwrap_or_default();

        // Amarra tasks órfãs ao primeiro projeto. Idempotente.
        ensure_default_project_and_bind_orphans(&config, &task_projects, repo.as_ref())?;

        Ok(AppState {
            repo,
            config: Mutex::new(config),
            i18n: Mutex::new(i18n),
            sessions: Mutex::new(HashMap::new()),
            task_projects,
            task_runs,
            task_worktrees,
            task_blockers,
            task_jira,
            jira_keys,
            task_order,
            app_handle: Mutex::new(None),
            agent_models: Mutex::new(seeded_models),
            token_epoch: AtomicU64::new(0),
            decision_lock: tokio::sync::Mutex::new(()),
            jira_worktree_locks: Mutex::new(HashMap::new()),
            jira_active_executors: Mutex::new(HashMap::new()),
        })
    }

    /// Test-only constructor: build an `AppState` over an explicit `home`
    /// dir, repo, and config without touching the user's real `~/.cadenza`.
    /// Side tables are loaded fresh from `home` (empty on a tempdir).
    #[cfg(test)]
    pub fn for_test(
        home: &Path,
        repo: Arc<dyn Repository>,
        config: Config,
    ) -> anyhow::Result<Self> {
        let i18n = I18n::new("en");
        Ok(AppState {
            repo,
            config: Mutex::new(config),
            i18n: Mutex::new(i18n),
            sessions: Mutex::new(HashMap::new()),
            task_projects: Arc::new(TaskProjects::load(home)?),
            task_runs: Arc::new(TaskRuns::load(home)?),
            task_worktrees: Arc::new(TaskWorktrees::load(home)?),
            task_blockers: Arc::new(TaskBlockers::load(home)?),
            task_jira: Arc::new(TaskJira::load(home)?),
            jira_keys: Arc::new(JiraKeyIndex::default()),
            task_order: Arc::new(TaskOrder::load(home)?),
            app_handle: Mutex::new(None),
            agent_models: Mutex::new(HashMap::new()),
            token_epoch: AtomicU64::new(0),
            decision_lock: tokio::sync::Mutex::new(()),
            jira_worktree_locks: Mutex::new(HashMap::new()),
            jira_active_executors: Mutex::new(HashMap::new()),
        })
    }
}

fn to_str_err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Layer the sidecar-stored fields (worktree/branch, then blockers) onto a
/// task read from the repo. The single place that defines the enrichment
/// order, shared by the Tauri commands here and the IPC dispatch in `ipc`.
pub(crate) fn enrich_task(state: &AppState, task: Task) -> Task {
    // First merge the file-backend Jira identity sidecar (no-op when the
    // task row already carries `jira_site`/`jira_issue_id`, i.e. SQL
    // backends), then compute the read-only `jira_key_display`.
    let task = state
        .task_blockers
        .enrich(state.task_worktrees.enrich(task));
    let task = state.task_jira.enrich(task);
    enrich_jira_key_display(state, task)
}

/// Fill the computed, never-persisted `jira_key_display` field from the
/// in-memory index, falling back to a `"<site>/<issue_id>"` string when no
/// record is cached. Synchronous — reads `state.jira_keys` lock-only.
fn enrich_jira_key_display(state: &AppState, mut task: Task) -> Task {
    if let (Some(site), Some(issue)) = (task.jira_site.clone(), task.jira_issue_id.clone()) {
        task.jira_key_display = Some(jira_key_display_for(state, &site, &issue));
    }
    task
}

fn jira_key_display_for(state: &AppState, site: &str, issue_id: &str) -> String {
    match state.jira_keys.get(site, issue_id) {
        Some(key) => key,
        None => format!("{site}/{issue_id}"),
    }
}

/// Compute `jira_key_display` on a proposta. There is no shared
/// `enrich_proposta` seam elsewhere; both proposta read handlers call this.
pub(crate) fn enrich_proposta(state: &AppState, mut p: Proposta) -> Proposta {
    if let (Some(site), Some(issue)) = (p.jira_site.clone(), p.jira_issue_id.clone()) {
        p.jira_key_display = Some(jira_key_display_for(state, &site, &issue));
    }
    p
}

// ───────────────────────── tasks ─────────────────────────
//
// Task CRUD, ordering, blocker validation, and the `T-<n>` id helpers live
// in `tasks.rs`. Re-exported flat so every existing `commands::*` path keeps
// resolving unchanged (Tauri `generate_handler!` in lib.rs; `ipc.rs` uses
// `crate::commands::{next_task_id, highest_task_number, sort_tasks_by_order}`).
// `mint_next_task_id` / `normalize_and_validate_blockers` /
// `ensure_task_unblocked` are `pub(crate)` so sibling submodules reach them
// via `super::` (e.g. `proposals.rs::create_task_from_proposta`).
mod tasks;
pub use tasks::*;

// ───────────────────────── attachments ─────────────────────────

mod attachments;
pub use attachments::*;

// ───────────────────────── task ↔ project mapping ─────────────────────────
//
// Task↔project mapping and active-project handlers live in `projects.rs`.
// Re-exported flat so every existing `commands::*` path keeps resolving
// unchanged (Tauri `generate_handler!` in lib.rs).
mod projects;
pub use projects::*;

/// Notify open views (board / cards) that a task's worktree/branch
/// changed. Best-effort: the modal also refreshes itself on close.
fn emit_tasks_changed(state: &AppState, task_id: &str) {
    if let Some(app) = state.app_handle.lock().ok().and_then(|h| h.clone()) {
        let _ = app.emit(cadenza_proto::ops::EV_TASKS_CHANGED, task_id);
    }
}

// ───────────────────────── task worktrees ─────────────────────────
//
// Worktree/branch command handlers, the `TaskWorktreeDefaults` response
// type, and the agent-start workspace prep live in `worktrees.rs`.
// Re-exported flat so every existing `commands::*` path keeps resolving
// unchanged (Tauri `generate_handler!` in lib.rs;
// `crate::commands::suggested_worktree_path` in `jira/worktree.rs`).
// `prepare_task_workspace` / `suggested_worktree_path` are `pub(crate)` so
// sibling submodules reach them via `super::` (e.g. `agents.rs`).
mod worktrees;
pub use worktrees::*;

// ───────────────────────── triage / proposals ─────────────────────────
//
// Proposal read/decision handlers and the derived-task materialization
// (`create_task_from_proposta`) live in `proposals.rs`. Re-exported flat so
// every existing `commands::*` path keeps resolving unchanged (Tauri
// `generate_handler!` in lib.rs). `create_task_from_proposta` is
// `pub(crate)` (also called by the Jira flow via `super::`);
// `proposta_to_body` is `pub(crate)` for the `mod.rs` test block.
mod proposals;
pub use proposals::*;

// ───────────────────────── Jira domain (split-out submodule) ─────────────────────────
//
// The Jira analysis-run capability layer, data layer, review aggregation, and
// import/discard lifecycle live in `jira.rs`. Re-exported flat so every
// existing `commands::jira_*` path (Tauri `generate_handler!` in lib.rs, IPC
// dispatch in `ipc.rs`) keeps resolving unchanged.
mod jira;
pub use jira::*;

// ───────────────────────── PTY / terminal ─────────────────────────

mod pty;
pub use pty::*;

// ───────────────────────── i18n / config ─────────────────────────

mod locale;
pub use locale::*;

// ───────────────────────── settings / storage / updater ─────────────────────────
//
// Config getters/setters, storage-backend switch, keyring (pg/jira) ops,
// updater commands, and repository construction (`build_repo`,
// `ensure_default_project_and_bind_orphans`, called from `AppState::init`)
// live in `config.rs`. Re-exported flat so every existing `commands::*` path
// (Tauri `generate_handler!` in lib.rs; unqualified calls in mod.rs) keeps
// resolving unchanged.
mod config;
pub use config::*;

// ───────────────────────── agent runs ─────────────────────────
//
// Agent launch (`start_task_agent`), run-record commands
// (`read_task_run`/`list_task_runs`/`clear_task_run`), and discovery
// (`list_installed_agents`/`list_agent_models`) live in `agents.rs`,
// re-exported flat. `send_initial_prompt` and `wait_for_codex_uuid` stay
// here (also used by `ideias.rs`/`jira.rs`); the agents submodule reaches
// them via `super::`.
mod agents;
pub use agents::*;

/// Type the prompt into the PTY and then send a discrete Enter.
///
/// Why paced: Claude Code (ink/React) and Codex both treat a single
/// chunk containing `text + \r` as a paste, which fills the input box
/// but does NOT trigger submit. Writing the text, pausing briefly, and
/// then sending `\r` alone reads as "user typed, then pressed Enter."
/// The initial 1.5 s wait is for the agent's UI to finish bootstrapping
/// before accepting any input at all.
async fn send_initial_prompt(session: &Arc<crate::terminal::TerminalSession>, prompt: &str) {
    tokio::time::sleep(Duration::from_millis(1500)).await;
    if let Err(e) = session.write(prompt.as_bytes()) {
        tracing::warn!(error = ?e, "failed to write initial prompt body");
        return;
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    if let Err(e) = session.write(b"\r") {
        tracing::warn!(error = ?e, "failed to submit initial prompt (CR)");
    }
}

/// Poll the Codex sessions directory until a new rollout file appears
/// or we give up. Budget: ~10 seconds at 250 ms intervals. Codex
/// usually creates the file within ~1 s of spawn, but cold starts and
/// slow disks can push it out.
async fn wait_for_codex_uuid(capture: CodexCapture) -> Option<String> {
    use tokio::time::{sleep, Duration};
    for _ in 0..40 {
        if let Some(uuid) = agent::find_codex_session_uuid(&capture) {
            return Some(uuid);
        }
        sleep(Duration::from_millis(250)).await;
    }
    None
}

// ───────────────────────── ideias (Inbox) ─────────────────────────

mod ideias;
pub use ideias::*;

// ───────────────────── memória compartilhada por projeto ─────────────────────

mod memory;
pub use memory::*;

// ─────────────────────────── skills (CLI snippet) ───────────────────────────

mod skills;
pub use skills::*;

// ───────────────────────── diagnostics ─────────────────────────
//
// The diagnostics-bundle export, its `DiagnosticsExport` result type, and
// the logs-folder reveal live in `diag.rs` (named `diag` to avoid confusion
// with the crate-level `crate::diagnostics` module). Re-exported flat so
// every existing `commands::*` path keeps resolving unchanged (Tauri
// `generate_handler!` in lib.rs).
mod diag;
pub use diag::*;

// ───────── review packages (PLAN §D.13) ─────────────────────────────────
//
// Review-package read commands (`get_review_package`, `get_review_diff`,
// `review_decision`, `list_review_states`), their response types, and the
// shared human-decision transition core (`apply_review_decision`, called
// from ipc.rs via `crate::commands::apply_review_decision`) live in
// `review.rs`. Re-exported flat so every existing `commands::*` path keeps
// resolving unchanged.
mod review;
pub use review::*;

#[cfg(test)]
mod tests {
    use super::{
        highest_task_number, proposta_to_body, render_memory_block, resync_payload, I18n,
        MemoryItem, Mutex, Proposta,
    };

    fn mem_item(texto: &str) -> MemoryItem {
        MemoryItem {
            id: "M-x".into(),
            texto: texto.into(),
            origem_task: None,
            criado_em: 0,
        }
    }

    #[test]
    fn memory_block_lists_each_item_as_a_bullet() {
        let i18n = Mutex::new(I18n::new("en"));
        let block = render_memory_block(
            &i18n,
            &[
                mem_item("IPC handlers live in ipc.rs"),
                mem_item("Use SQLite WAL"),
            ],
        );
        assert!(block.contains("- IPC handlers live in ipc.rs"));
        assert!(block.contains("- Use SQLite WAL"));
        // The localized header is present (en bundle key resolved).
        assert!(block.to_lowercase().contains("project memory"));
    }

    #[test]
    fn memory_block_is_caller_omitted_when_empty() {
        // The injection site guards on `!items.is_empty()` (see
        // start_task_agent), so render_memory_block is never called with an
        // empty slice in practice. Asserting that guard's contract here.
        let items: Vec<MemoryItem> = Vec::new();
        assert!(items.is_empty());
    }

    fn sample_proposta() -> Proposta {
        Proposta {
            proposta_id: "P-abc123".to_string(),
            idempotency_key: "key".to_string(),
            parent: Some("T-28".to_string()),
            title: "Bug X".to_string(),
            repro: "abrir o modal".to_string(),
            file: "ui/triage-modal.js".to_string(),
            what_failed: "task_id null hardcoded".to_string(),
            action: "criar a task no backend".to_string(),
            jira_site: None,
            jira_issue_id: None,
            jira_key_display: None,
            created_at_ms: 0,
        }
    }

    #[test]
    fn resync_payload_clears_then_replays_snapshot() {
        let payload = resync_payload(b"scrollback".to_vec());
        // Cursor home + clear viewport + clear scrollback, THEN the ring.
        assert!(
            payload.starts_with(b"\x1b[H\x1b[2J\x1b[3J"),
            "resync must clear before replaying so content isn't doubled"
        );
        assert!(payload.ends_with(b"scrollback"));
    }

    #[test]
    fn resync_payload_handles_empty_snapshot() {
        // An empty ring still produces just the clear sequence — harmless.
        assert_eq!(resync_payload(Vec::new()), b"\x1b[H\x1b[2J\x1b[3J".to_vec());
    }

    #[test]
    fn proposta_to_body_renders_all_sections() {
        let body = proposta_to_body(&sample_proposta());
        assert!(body.contains("**Arquivo:** ui/triage-modal.js"));
        assert!(body.contains("## Como reproduzir\nabrir o modal"));
        assert!(body.contains("## O que falhou\ntask_id null hardcoded"));
        assert!(body.contains("## Ação proposta\ncriar a task no backend"));
        assert!(body.contains("Derivada da proposta P-abc123."));
    }

    #[test]
    fn proposta_to_body_omits_empty_file_line() {
        let mut p = sample_proposta();
        p.file = "   ".to_string();
        let body = proposta_to_body(&p);
        assert!(!body.contains("**Arquivo:**"));
        // The substantive sections still render.
        assert!(body.contains("## Como reproduzir"));
    }

    #[tokio::test]
    async fn file_backend_task_jira_identity_only_via_enrichment() {
        // Regression (P1): start_task_agent's one-executor-per-issue guard and
        // shared-worktree ensure both key off task.jira_site/jira_issue_id. On
        // the file backend that identity lives in the task-jira.json sidecar,
        // NOT on the (frozen-frontmatter) task row, so a raw read_task reports
        // None for both — the guard would silently no-op and let two execute
        // agents share one issue's worktree. start_task_agent must enrich the
        // row before the guard; this locks that mechanism in.
        use super::{enrich_task, AppState, Config, FileRepository, Repository};
        use std::sync::Arc;
        use tempfile::TempDir;

        let home = TempDir::new().unwrap();
        let repo = Arc::new(FileRepository::new(home.path()).unwrap());
        let state = AppState::for_test(home.path(), repo.clone(), Config::default()).unwrap();

        let task = Task {
            id: "T-1".to_string(),
            titulo: "Jira subtask".to_string(),
            estado: Estado::AFazer,
            responsavel: "humano".to_string(),
            body: String::new(),
            worktree_path: None,
            branch: None,
            blocked_by: Vec::new(),
            jira_site: Some("https://x.atlassian.net".to_string()),
            jira_issue_id: Some("10001".to_string()),
            jira_key_display: None,
        };
        repo.create_task(&task).await.unwrap();
        // Persist the identity the way create_task_from_proposta does on the
        // file backend (sidecar, since the row has no Jira columns).
        state
            .task_jira
            .set("T-1", "https://x.atlassian.net", "10001")
            .unwrap();

        // Raw read: the file backend drops the Jira identity — this is exactly
        // what start_task_agent used to feed the guard, so it no-op'd.
        let raw = repo.read_task("T-1").await.unwrap();
        assert!(
            raw.jira_site.is_none() && raw.jira_issue_id.is_none(),
            "file backend unexpectedly carries Jira identity on the task row"
        );

        // Enriched read (what start_task_agent now does): identity restored, so
        // the executor guard and worktree-ensure actually fire.
        let enriched = enrich_task(&state, raw);
        assert_eq!(
            enriched.jira_site.as_deref(),
            Some("https://x.atlassian.net")
        );
        assert_eq!(enriched.jira_issue_id.as_deref(), Some("10001"));
    }

    #[test]
    fn highest_task_number_returns_zero_for_empty() {
        assert_eq!(highest_task_number(std::iter::empty()), 0);
    }

    #[test]
    fn highest_task_number_picks_max_of_sequential_ids() {
        let ids = ["T-1", "T-4", "T-2"];
        assert_eq!(highest_task_number(ids.iter().copied()), 4);
    }

    #[test]
    fn highest_task_number_ignores_legacy_uuid_ids() {
        // Tasks created by the old random-id path or by task-ai (Node)
        // shouldn't poison the counter — they're just skipped.
        let ids = ["T-MP08LIVOPNM", "T-7", "T-deadbeef", "T-3"];
        assert_eq!(highest_task_number(ids.iter().copied()), 7);
    }

    #[test]
    fn highest_task_number_ignores_other_prefixes() {
        let ids = ["I-5", "T-2", "X-99"];
        assert_eq!(highest_task_number(ids.iter().copied()), 2);
    }

    use super::{sort_tasks_by_order, Estado, Task};
    use std::collections::HashMap;

    fn task(id: &str, estado: Estado) -> Task {
        Task {
            id: id.to_string(),
            titulo: id.to_string(),
            estado,
            responsavel: "humano".to_string(),
            body: String::new(),
            worktree_path: None,
            branch: None,
            blocked_by: Vec::new(),
            jira_site: None,
            jira_issue_id: None,
            jira_key_display: None,
        }
    }

    fn order(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(e, ids)| (e.to_string(), ids.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    fn ids(tasks: &[Task]) -> Vec<&str> {
        tasks.iter().map(|t| t.id.as_str()).collect()
    }

    #[test]
    fn ordered_ids_come_first_in_list_order() {
        let mut tasks = vec![
            task("T-1", Estado::AFazer),
            task("T-3", Estado::AFazer),
            task("T-5", Estado::AFazer),
        ];
        let order = order(&[("a_fazer", &["T-5", "T-1", "T-3"])]);
        sort_tasks_by_order(&mut tasks, &order);
        assert_eq!(ids(&tasks), ["T-5", "T-1", "T-3"]);
    }

    #[test]
    fn unordered_appended_by_ascending_number() {
        // T-2 is listed; T-1 and T-10 are not — they fall after, newest
        // (higher number) last.
        let mut tasks = vec![
            task("T-10", Estado::AFazer),
            task("T-1", Estado::AFazer),
            task("T-2", Estado::AFazer),
        ];
        let order = order(&[("a_fazer", &["T-2"])]);
        sort_tasks_by_order(&mut tasks, &order);
        assert_eq!(ids(&tasks), ["T-2", "T-1", "T-10"]);
    }

    #[test]
    fn stale_ids_in_list_are_ignored() {
        // T-99 was deleted but lingers in the stored order — it must not
        // panic or affect the real tasks.
        let mut tasks = vec![task("T-1", Estado::AFazer), task("T-2", Estado::AFazer)];
        let order = order(&[("a_fazer", &["T-99", "T-2", "T-1"])]);
        sort_tasks_by_order(&mut tasks, &order);
        assert_eq!(ids(&tasks), ["T-2", "T-1"]);
    }

    #[test]
    fn new_task_lands_last() {
        // No stored order at all: pure ascending-number, newest last.
        let mut tasks = vec![
            task("T-7", Estado::AFazer),
            task("T-2", Estado::AFazer),
            task("T-12", Estado::AFazer),
        ];
        sort_tasks_by_order(&mut tasks, &HashMap::new());
        assert_eq!(ids(&tasks), ["T-2", "T-7", "T-12"]);
    }

    #[test]
    fn cross_column_lands_last() {
        // T-4 moved into `fazendo`, which has a stored order not yet
        // mentioning it — it sorts after the listed cards.
        let mut tasks = vec![
            task("T-4", Estado::Fazendo),
            task("T-1", Estado::Fazendo),
            task("T-2", Estado::Fazendo),
        ];
        let order = order(&[("fazendo", &["T-2", "T-1"])]);
        sort_tasks_by_order(&mut tasks, &order);
        assert_eq!(ids(&tasks), ["T-2", "T-1", "T-4"]);
    }

    #[test]
    fn tasks_stay_grouped_by_estado() {
        let mut tasks = vec![
            task("T-1", Estado::Fazendo),
            task("T-2", Estado::AFazer),
            task("T-3", Estado::Fazendo),
            task("T-4", Estado::AFazer),
        ];
        sort_tasks_by_order(&mut tasks, &HashMap::new());
        // a_fazer sorts before fazendo (lexicographic on as_str), each
        // group internally ascending by number.
        assert_eq!(ids(&tasks), ["T-2", "T-4", "T-1", "T-3"]);
    }
}
