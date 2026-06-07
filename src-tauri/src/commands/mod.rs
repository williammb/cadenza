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

async fn normalize_and_validate_blockers(
    repo: &dyn Repository,
    task_id: &str,
    blocked_by: Vec<String>,
) -> Result<Vec<String>, String> {
    let mut normalized = Vec::new();
    for raw in blocked_by {
        let id = raw.trim();
        if id.is_empty() || normalized.iter().any(|existing| existing == id) {
            continue;
        }
        crate::store::validate_id(id).map_err(to_str_err)?;
        if id == task_id {
            return Err(format!("task '{task_id}' cannot block itself"));
        }
        repo.read_task(id).await.map_err(to_str_err)?;
        normalized.push(id.to_string());
    }
    Ok(normalized)
}

async fn ensure_task_unblocked(state: &AppState, task_id: &str) -> Result<(), String> {
    let blockers = state.task_blockers.get(task_id);
    if blockers.is_empty() {
        return Ok(());
    }

    let mut unfinished = Vec::new();
    for blocker_id in blockers {
        match state.repo.read_task(&blocker_id).await {
            Ok(task) if task.estado.satisfies_blocker() => {}
            Ok(task) => unfinished.push(format!(
                "{} '{}' is {}",
                task.id,
                task.titulo,
                task.estado.as_str()
            )),
            Err(StoreError::NotFound(_)) => unfinished.push(format!("{blocker_id} was not found")),
            Err(e) => return Err(to_str_err(e)),
        }
    }

    if unfinished.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "task '{task_id}' is blocked by unfinished task(s): {}",
            unfinished.join("; ")
        ))
    }
}

// ───────────────────────── tasks ─────────────────────────

#[tauri::command]
pub async fn list_tasks(
    state: State<'_, Arc<AppState>>,
    estado: Option<String>,
) -> Result<Vec<Task>, String> {
    let filter = estado.as_deref().and_then(Estado::parse);
    let tasks = state.repo.list_tasks(filter).await.map_err(to_str_err)?;
    let mut tasks: Vec<Task> = tasks.into_iter().map(|t| enrich_task(&state, t)).collect();
    sort_tasks_by_order(&mut tasks, &state.task_order.snapshot());
    Ok(tasks)
}

/// Sort tasks by the per-column priority order from `task-order.json`,
/// in place. Tasks are kept grouped by estado (deterministic across
/// backends); within a column, ids present in that column's list come
/// first in list order, and any task not listed (a freshly created card,
/// or one moved in out-of-band) sorts after them by ascending `T-<n>`
/// number — so the newest task lands last. Stale ids in the list (a
/// deleted task, or one whose estado changed) simply never match a real
/// task and are ignored.
pub(crate) fn sort_tasks_by_order(tasks: &mut [Task], order: &HashMap<String, Vec<String>>) {
    tasks.sort_by(|a, b| {
        let (ea, eb) = (a.estado.as_str(), b.estado.as_str());
        if ea != eb {
            return ea.cmp(eb);
        }
        let list = order.get(ea);
        let rank = |id: &str| list.and_then(|l| l.iter().position(|x| x == id));
        match (rank(&a.id), rank(&b.id)) {
            (Some(i), Some(j)) => i.cmp(&j),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => task_num(&a.id)
                .cmp(&task_num(&b.id))
                .then_with(|| a.id.cmp(&b.id)),
        }
    });
}

/// Numeric component of a `T-<n>` id, or `u64::MAX` for any other shape
/// so non-`T-` ids sort to the end. Used to keep unlisted tasks ordered
/// newest-last.
fn task_num(id: &str) -> u64 {
    id.strip_prefix("T-")
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(u64::MAX)
}

#[tauri::command]
pub async fn read_task(state: State<'_, Arc<AppState>>, id: String) -> Result<Task, String> {
    let task = state.repo.read_task(&id).await.map_err(to_str_err)?;
    Ok(enrich_task(&state, task))
}

/// Compute the next sequential task id (`T-<n>`) by scanning existing
/// tasks. The frontend calls this just before submitting a new task so
/// IDs read like a notebook (T-1, T-2, ...) instead of opaque UUIDs.
///
/// Source of truth is `repo.list_tasks(None)` — that survives external
/// writes from the Node.js task-ai version sharing `~/.cadenza/tasks/`.
/// Two near-simultaneous creates can theoretically race to the same
/// number, but the cost is a benign rename; the file backend overwrites
/// safely, and the UI does this in one user-initiated submit.
#[tauri::command]
pub async fn next_task_id(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    mint_next_task_id(state.repo.as_ref()).await
}

/// Compute the next sequential `T-<n>` id from the repo's current tasks.
/// Shared by `next_task_id` (UI pre-fill) and `create_task_from_proposta`
/// (derived-task materialization) so the id scheme lives in one place.
async fn mint_next_task_id(repo: &dyn Repository) -> Result<String, String> {
    let tasks = repo.list_tasks(None).await.map_err(to_str_err)?;
    let next = highest_task_number(tasks.iter().map(|t| t.id.as_str())) + 1;
    Ok(format!("T-{next}"))
}

/// Inspect `T-<n>` ids, ignore any other shape, and return the highest
/// `n` seen (0 if none). Pure — call from anywhere that has an
/// iterator of task ids.
pub fn highest_task_number<'a, I: Iterator<Item = &'a str>>(ids: I) -> u64 {
    let mut max = 0u64;
    for id in ids {
        let Some(rest) = id.strip_prefix("T-") else {
            continue;
        };
        if let Ok(n) = rest.parse::<u64>() {
            if n > max {
                max = n;
            }
        }
    }
    max
}

#[tauri::command]
pub async fn create_task(
    state: State<'_, Arc<AppState>>,
    task: Task,
    project_id: String,
) -> Result<(), String> {
    // Toda task precisa de projeto. O ID precisa existir em
    // `config.projects` — caso contrário a UI/CLI tentou usar um
    // projeto inválido (digitação, projeto removido entre passos).
    let pid = project_id.trim();
    if pid.is_empty() {
        return Err("project_id is required".to_string());
    }
    {
        let cfg = state.config.lock().map_err(to_str_err)?;
        if !cfg.projects.iter().any(|p| p.id == pid) {
            return Err(format!("unknown project_id: {pid}"));
        }
    }
    let blocked_by =
        normalize_and_validate_blockers(state.repo.as_ref(), &task.id, task.blocked_by.clone())
            .await?;
    state.repo.create_task(&task).await.map_err(to_str_err)?;
    if !blocked_by.is_empty() {
        state
            .task_blockers
            .set(&task.id, blocked_by)
            .map_err(to_str_err)?;
    }
    state
        .task_projects
        .set(&task.id, Some(pid))
        .map_err(to_str_err)?;
    Ok(())
}

#[tauri::command]
pub async fn set_estado(
    state: State<'_, Arc<AppState>>,
    id: String,
    estado: String,
) -> Result<(), String> {
    let parsed = Estado::parse(&estado).ok_or_else(|| format!("invalid estado: {estado}"))?;
    if parsed == Estado::Fazendo {
        ensure_task_unblocked(&state, &id).await?;
    }
    state.repo.set_estado(&id, parsed).await.map_err(to_str_err)
}

/// Persist the priority order of one column. The UI sends the full
/// ordered id list for the affected estado after a drag-to-reorder (or
/// cross-column drop), so the call is idempotent and self-correcting —
/// it overwrites whatever was stored. Ordering is a GUI-only concern, so
/// there is no matching NDJSON op: the CLI never reorders.
#[tauri::command]
pub async fn set_task_order(
    state: State<'_, Arc<AppState>>,
    estado: String,
    ids: Vec<String>,
) -> Result<(), String> {
    Estado::parse(&estado).ok_or_else(|| format!("invalid estado: {estado}"))?;
    state.task_order.set(&estado, ids).map_err(to_str_err)
}

#[tauri::command]
pub async fn append_log(
    state: State<'_, Arc<AppState>>,
    id: String,
    text: String,
) -> Result<(), String> {
    state.repo.append_log(&id, &text).await.map_err(to_str_err)
}

#[tauri::command]
pub async fn update_task_body(
    state: State<'_, Arc<AppState>>,
    id: String,
    body: String,
) -> Result<(), String> {
    state
        .repo
        .update_task_body(&id, &body)
        .await
        .map_err(to_str_err)
}

#[tauri::command]
pub async fn delete_task(state: State<'_, Arc<AppState>>, id: String) -> Result<(), String> {
    state.repo.delete_task(&id).await.map_err(to_str_err)?;
    // Drop the task's review packages (+ any dangling done journal on the
    // file backend). Best-effort: the task row/file is already gone, so
    // orphaned packages only cost storage (PLAN §F.17).
    if let Err(e) = state.repo.delete_review_packages(&id).await {
        tracing::warn!(error = ?e, task = %id, "delete_review_packages failed");
    }
    // Drop the side-mapping entry so it doesn't dangle forever after
    // the task file is gone. Failure here is non-fatal — the task is
    // already deleted; a stale mapping entry just costs disk bytes.
    if let Err(e) = state.task_projects.forget(&id) {
        tracing::warn!(error = ?e, task = %id, "task_projects.forget failed");
    }
    if let Err(e) = state.task_runs.forget(&id) {
        tracing::warn!(error = ?e, task = %id, "task_runs.forget failed");
    }
    if let Err(e) = state.task_worktrees.forget(&id) {
        tracing::warn!(error = ?e, task = %id, "task_worktrees.forget failed");
    }
    if let Err(e) = state.task_blockers.forget(&id) {
        tracing::warn!(error = ?e, task = %id, "task_blockers.forget failed");
    }
    if let Err(e) = state.task_order.forget(&id) {
        tracing::warn!(error = ?e, task = %id, "task_order.forget failed");
    }
    // Drop any images the task body referenced. Best-effort: the task is
    // already gone, orphaned files only cost disk bytes.
    crate::attachments::delete_owner("tasks", &id);
    Ok(())
}

// ───────────────────────── attachments ─────────────────────────

mod attachments;
pub use attachments::*;

// ───────────────────────── task ↔ project mapping ─────────────────────────

/// Return the full task_id → project_id mapping. The board calls this
/// once on render and joins with `list_tasks` client-side to filter
/// by `active_project_id`. Cheaper than per-task get_task_project
/// calls since most boards have <100 entries.
#[tauri::command]
pub fn list_task_projects(
    state: State<'_, Arc<AppState>>,
) -> Result<HashMap<String, String>, String> {
    Ok(state.task_projects.snapshot())
}

/// Bind (or unbind, when `project_id` is `None`) a task to a project.
/// Called by the "Nova task" modal after a successful create and by
/// the per-card "Mover de projeto" action.
#[tauri::command]
pub fn set_task_project(
    state: State<'_, Arc<AppState>>,
    task_id: String,
    project_id: Option<String>,
) -> Result<(), String> {
    state
        .task_projects
        .set(&task_id, project_id.as_deref())
        .map_err(to_str_err)
}

/// Snapshot of every task→worktree/branch mapping. Currently unused by
/// the board — `list_tasks`/`read_task`/`current_task` already enrich
/// each task with `worktree_path`/`branch` inline (see
/// `TaskWorktrees::enrich`), so there is no client-side join. Kept as a
/// command for a future board view that needs the mapping standalone;
/// do not remove the inline enrichment on the assumption the UI joins here.
#[tauri::command]
pub fn list_task_worktrees(
    state: State<'_, Arc<AppState>>,
) -> Result<HashMap<String, WorktreeInfo>, String> {
    Ok(state.task_worktrees.snapshot())
}

/// Persist the blockers for a task. Blocker ids must point to existing
/// tasks and cannot include the task itself.
#[tauri::command]
pub async fn set_task_blockers(
    state: State<'_, Arc<AppState>>,
    task_id: String,
    blocked_by: Vec<String>,
) -> Result<(), String> {
    crate::store::validate_id(&task_id).map_err(to_str_err)?;
    state.repo.read_task(&task_id).await.map_err(to_str_err)?;
    let blocked_by =
        normalize_and_validate_blockers(state.repo.as_ref(), &task_id, blocked_by).await?;
    state
        .task_blockers
        .set(&task_id, blocked_by)
        .map_err(to_str_err)?;
    emit_tasks_changed(&state, &task_id);
    Ok(())
}

/// Persist the task's declarative branch/worktree config from the modal:
/// origin → destination, the use-worktree intent, and the worktree path.
/// No git runs here — the actual pull/branch/worktree happens at agent
/// start (`prepare_task_workspace`). An all-empty config clears the entry.
#[tauri::command]
pub fn set_task_worktree(
    state: State<'_, Arc<AppState>>,
    task_id: String,
    worktree_path: Option<String>,
    branch: Option<String>,
    origin_branch: Option<String>,
    use_worktree: Option<bool>,
) -> Result<(), String> {
    // Normalize empty strings to None so a cleared field doesn't persist
    // as `Some("")` and later defeat the `is_empty`/fallback checks.
    let norm = |s: Option<String>| s.filter(|v| !v.trim().is_empty());
    state
        .task_worktrees
        .set(
            &task_id,
            WorktreeInfo {
                worktree_path: norm(worktree_path),
                branch: norm(branch),
                origin_branch: norm(origin_branch),
                use_worktree: use_worktree.unwrap_or(false),
            },
        )
        .map_err(to_str_err)
}

/// What the task modal needs to pre-fill its worktree/branch section in
/// one round-trip: the project repo path, its *current* branch (the
/// default shown to the user), a suggested sibling worktree path, and any
/// association already stored for this task.
#[derive(Serialize)]
pub struct TaskWorktreeDefaults {
    pub project_path: String,
    pub current_branch: String,
    pub suggested_worktree_path: String,
    pub stored: WorktreeInfo,
    /// Local branches in the repo, to populate the origin/destination
    /// pickers. Empty when the repo has no commits yet or git fails.
    pub branches: Vec<String>,
    /// The project's configured default branch (`None`/empty when unset);
    /// the UI pre-fills origin with it before falling back to current.
    pub default_branch: Option<String>,
}

/// Resolve the on-disk repo path for a task via its project mapping.
/// Mirrors the project-resolution step in `start_task_agent`.
fn project_path_for_task(state: &AppState, task_id: &str) -> Result<PathBuf, String> {
    let project_id = state
        .task_projects
        .snapshot()
        .get(task_id)
        .cloned()
        .ok_or_else(|| {
            format!(
                "task '{task_id}' has no project assigned — assign one so the worktree has a repo"
            )
        })?;
    let cfg = state.config.lock().map_err(to_str_err)?;
    let project = cfg
        .projects
        .iter()
        .find(|p| p.id == project_id)
        .ok_or_else(|| format!("project '{project_id}' not found in config"))?;
    Ok(project.path.clone())
}

/// The configured default branch for a task's project, or `None` when the
/// task has no project, the project is gone, or its `default_branch` is
/// unset/blank. Mirrors `project_path_for_task`'s task→project resolution.
fn default_branch_for_task(state: &AppState, task_id: &str) -> Result<Option<String>, String> {
    let cfg = state.config.lock().map_err(to_str_err)?;
    Ok(state
        .task_projects
        .snapshot()
        .get(task_id)
        .and_then(|pid| cfg.projects.iter().find(|p| &p.id == pid))
        .and_then(|p| p.default_branch.clone())
        .filter(|b| !b.trim().is_empty()))
}

/// Default sibling worktree path: `<repo-parent>/<repo-name>-<branch>`,
/// with path separators in the branch flattened to `-` so it stays a
/// single directory name.
pub(crate) fn suggested_worktree_path(repo: &Path, branch: &str) -> PathBuf {
    let sanitized: String = branch
        .chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .collect();
    let name = repo.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
    let parent = repo.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{name}-{sanitized}"))
}

/// Notify open views (board / cards) that a task's worktree/branch
/// changed. Best-effort: the modal also refreshes itself on close.
fn emit_tasks_changed(state: &AppState, task_id: &str) {
    if let Some(app) = state.app_handle.lock().ok().and_then(|h| h.clone()) {
        let _ = app.emit(cadenza_proto::ops::EV_TASKS_CHANGED, task_id);
    }
}

/// Pre-fill data for the task modal's worktree section. Reads the
/// project's current git branch; surfaces git errors to the UI (e.g. the
/// project path is not a git repo) so the modal can show a hint.
#[tauri::command]
pub async fn task_worktree_defaults(
    state: State<'_, Arc<AppState>>,
    task_id: String,
) -> Result<TaskWorktreeDefaults, String> {
    let repo = project_path_for_task(&state, &task_id)?;
    let current_branch = crate::git::current_branch(&repo)
        .await
        .map_err(to_str_err)?;
    let suggested = suggested_worktree_path(&repo, &current_branch);
    let stored = state.task_worktrees.get(&task_id).unwrap_or_default();
    let branches = crate::git::list_branches(&repo).await.unwrap_or_default();
    let default_branch = default_branch_for_task(&state, &task_id)?;
    Ok(TaskWorktreeDefaults {
        project_path: repo.to_string_lossy().into_owned(),
        current_branch,
        suggested_worktree_path: suggested.to_string_lossy().into_owned(),
        stored,
        branches,
        default_branch,
    })
}

/// Prepare the git workspace for a task right before an agent starts,
/// driven by the declarative config the modal stored (`set_task_worktree`).
///
/// Resolves the origin and destination branches, pulls origin (blocking on
/// a real failure; a no-op without an upstream), creates/switches the
/// destination branch, and creates the worktree when requested. Returns the
/// cwd the agent runs in — the worktree when used, otherwise the project
/// repo — and persists the resolved config back to the sidecar.
async fn prepare_task_workspace(state: &AppState, task_id: &str) -> Result<PathBuf, String> {
    let repo = project_path_for_task(state, task_id)?;
    let default_branch = default_branch_for_task(state, task_id)?;
    let stored = state.task_worktrees.get(task_id).unwrap_or_default();
    let current = crate::git::current_branch(&repo)
        .await
        .map_err(to_str_err)?;

    // 1. Resolve origin (stored → project default → current) and
    //    destination (stored → origin).
    let origin = stored
        .origin_branch
        .clone()
        .filter(|b| !b.trim().is_empty())
        .or(default_branch)
        .unwrap_or_else(|| current.clone())
        .trim()
        .to_string();
    let destination = stored
        .branch
        .clone()
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| origin.clone())
        .trim()
        .to_string();

    // 2. Pull origin. Blocks on a real failure; no-op without an upstream.
    crate::git::pull_branch(&repo, &origin)
        .await
        .map_err(to_str_err)?;

    let dest_exists = crate::git::branch_exists(&repo, &destination)
        .await
        .map_err(to_str_err)?;
    // New destination branches are based on origin; for an existing branch
    // git ignores the start point, so passing it is harmless either way.
    let start_point = if dest_exists {
        None
    } else {
        Some(origin.as_str())
    };

    // 3 + 4. Land on the destination branch, in a worktree when asked.
    let cwd = if stored.use_worktree {
        let wt_path = stored
            .worktree_path
            .clone()
            .filter(|p| !p.trim().is_empty())
            .ok_or_else(|| {
                format!("task '{task_id}' is set to use a worktree but has no worktree path")
            })?;
        let wt = PathBuf::from(&wt_path);
        if wt.exists() {
            // Reuse the existing worktree: switch it to the destination only
            // when it isn't already there.
            let on = crate::git::current_branch(&wt).await.map_err(to_str_err)?;
            if on != destination {
                crate::git::switch_branch(&wt, &destination, !dest_exists, start_point)
                    .await
                    .map_err(to_str_err)?;
            }
        } else {
            crate::git::add_worktree(&repo, &wt, &destination, !dest_exists, start_point)
                .await
                .map_err(to_str_err)?;
        }
        wt
    } else {
        // No worktree: operate on the project repo. Switch only when not
        // already on the destination ("se for igual só vai para o ramo se
        // já não estiver").
        if current != destination {
            crate::git::switch_branch(&repo, &destination, !dest_exists, start_point)
                .await
                .map_err(to_str_err)?;
        }
        repo.clone()
    };

    // 5. Persist the resolved config so the read-only displays and the next
    //    open reflect what actually happened.
    let resolved = WorktreeInfo {
        worktree_path: if stored.use_worktree {
            Some(cwd.to_string_lossy().into_owned())
        } else {
            None
        },
        branch: Some(destination),
        origin_branch: Some(origin),
        use_worktree: stored.use_worktree,
    };
    state
        .task_worktrees
        .set(task_id, resolved)
        .map_err(to_str_err)?;
    emit_tasks_changed(state, task_id);
    Ok(cwd)
}

/// Persist `active_project_id` to config.json. The board re-renders
/// after each call so the user sees the filter immediately.
#[tauri::command]
pub fn set_active_project(
    state: State<'_, Arc<AppState>>,
    project_id: Option<String>,
) -> Result<Config, String> {
    let path = dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".cadenza")
        .join("config.json");
    let mut slot = state.config.lock().map_err(to_str_err)?;
    slot.active_project_id = project_id;
    slot.save_to(&path).map_err(to_str_err)?;
    Ok(slot.clone())
}

#[tauri::command]
pub async fn set_titulo(
    state: State<'_, Arc<AppState>>,
    id: String,
    titulo: String,
) -> Result<(), String> {
    state
        .repo
        .set_titulo(&id, &titulo)
        .await
        .map_err(to_str_err)
}

/// First task in `fazendo`, or null if none. Tooling convenience — the
/// CLI's `cadenza current` maps here.
#[tauri::command]
pub async fn current_task(state: State<'_, Arc<AppState>>) -> Result<Option<Task>, String> {
    let task = state.repo.current_task().await.map_err(to_str_err)?;
    Ok(task.map(|t| enrich_task(&state, t)))
}

// ───────────────────────── triage ─────────────────────────

#[tauri::command]
pub async fn list_pending_propostas(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<Proposta>, String> {
    let propostas = state
        .repo
        .list_pending_propostas()
        .await
        .map_err(to_str_err)?;
    Ok(propostas
        .into_iter()
        .map(|p| enrich_proposta(&state, p))
        .collect())
}

#[tauri::command]
pub async fn read_proposta(
    state: State<'_, Arc<AppState>>,
    proposta_id: String,
) -> Result<Option<Proposta>, String> {
    let proposta = state
        .repo
        .read_proposta(&proposta_id)
        .await
        .map_err(to_str_err)?;
    Ok(proposta.map(|p| enrich_proposta(&state, p)))
}

#[tauri::command]
pub async fn read_decisao(
    state: State<'_, Arc<AppState>>,
    proposta_id: String,
) -> Result<Option<DecisaoRegistro>, String> {
    state
        .repo
        .read_decisao(&proposta_id)
        .await
        .map_err(to_str_err)
}

/// Persist a decision — frontend calls this from the modal or the
/// notification action handler.
///
/// When the decision is `Aceita` and no `task_id` was supplied, we
/// materialize the derived task here and stamp its id into the registro
/// before persisting. Doing it backend-side keeps create+decision atomic:
/// the UI can't crash between the two steps and leave a proposal accepted
/// without a task. (`Mesclada` carries an existing `task_id`; `Rejeitada`
/// keeps `task_id = None` — neither creates anything.)
#[tauri::command]
pub async fn decidir_proposta(
    state: State<'_, Arc<AppState>>,
    mut registro: DecisaoRegistro,
) -> Result<(), String> {
    if registro.decisao == Decisao::Aceita && registro.task_id.is_none() {
        // Serializa read→create→write para que um duplo-clique não deixe
        // duas chamadas concorrentes lerem "sem decisão" e materializarem
        // duas tasks. O segundo a entrar enxerga a decisão do primeiro e
        // reaproveita a task. (Re-tentativa após crash entre create e
        // write ainda pode duplicar — fechar essa janela exige persistir
        // create+decisão numa transação, o que depende do backend.)
        let _guard = state.decision_lock.lock().await;
        let existing = state
            .repo
            .read_decisao(&registro.proposta_id)
            .await
            .map_err(to_str_err)?
            .and_then(|d| d.task_id);
        let task_id = match existing {
            Some(id) => id,
            None => create_task_from_proposta(&state, &registro.proposta_id).await?,
        };
        registro.task_id = Some(task_id);
        return state.repo.write_decisao(registro).await.map_err(to_str_err);
    }
    state.repo.write_decisao(registro).await.map_err(to_str_err)
}

/// Materialize the derived task for an accepted proposal and return its
/// new `T-<n>` id. The project is inherited from the proposal's `parent`
/// task (via the task→project mapping), falling back to the active
/// project; errors when neither is known, since `create_task` requires a
/// valid project.
async fn create_task_from_proposta(state: &AppState, proposta_id: &str) -> Result<String, String> {
    let proposta = state
        .repo
        .read_proposta(proposta_id)
        .await
        .map_err(to_str_err)?
        .ok_or_else(|| format!("proposta not found: {proposta_id}"))?;

    // Projeto: herda do parent, senão usa o projeto ativo do config.
    let project_id = proposta
        .parent
        .as_deref()
        .and_then(|p| state.task_projects.get(p))
        .or_else(|| {
            state
                .config
                .lock()
                .ok()
                .and_then(|cfg| cfg.active_project_id.clone())
        })
        .ok_or_else(|| {
            "cannot create derived task: proposta has no parent project and no active project is set"
                .to_string()
        })?;

    // Mesmo guard de `create_task`: o projeto precisa existir no config
    // (pode ter sido removido entre a proposta e a aceitação).
    {
        let cfg = state.config.lock().map_err(to_str_err)?;
        if !cfg.projects.iter().any(|p| p.id == project_id) {
            return Err(format!("unknown project_id: {project_id}"));
        }
    }

    // Mint a sequential T-<n>, matching the in-app and CLI create paths.
    let task_id = mint_next_task_id(state.repo.as_ref()).await?;

    let task = Task {
        id: task_id.clone(),
        titulo: proposta.title.clone(),
        estado: Estado::AFazer,
        responsavel: "humano".to_string(),
        body: proposta_to_body(&proposta),
        worktree_path: None,
        branch: None,
        blocked_by: Vec::new(),
        jira_site: proposta.jira_site.clone(),
        jira_issue_id: proposta.jira_issue_id.clone(),
        jira_key_display: None,
    };
    state.repo.create_task(&task).await.map_err(to_str_err)?;
    // File backend has no Jira columns on the task row, so persist identity
    // to the `task-jira.json` sidecar. On SQL backends the row already
    // carries it (create_task above); the sidecar is harmless redundancy
    // and keeps `enrich_task` backend-agnostic.
    if let (Some(site), Some(issue)) = (&task.jira_site, &task.jira_issue_id) {
        state
            .task_jira
            .set(&task_id, site, issue)
            .map_err(to_str_err)?;
    }
    state
        .task_projects
        .set(&task_id, Some(&project_id))
        .map_err(to_str_err)?;
    // Slice 4: ensure the issue's shared worktree exists and associate this
    // task with it. Runs under `decision_lock` (held by `decidir_proposta`),
    // but that lock is global, not per-issue, so `ensure_issue_worktree` also
    // takes a per-issue guard + persisted reservation to converge re-accepts
    // of different propostas for the same issue onto one worktree. This is
    // best-effort for the accept path: a worktree failure must not fail task
    // creation (the task exists; the worktree can be retried at agent start),
    // so a failure is logged, not propagated.
    if let (Some(site), Some(issue)) = (&task.jira_site, &task.jira_issue_id) {
        // The record (seeded by prior slices) carries the canonical
        // display key; use it as the branch-name source. Fall back to the
        // task title when the record can't be read.
        let summary = match state.repo.read_jira_issue(site, issue).await {
            Ok(Some(rec)) => rec.jira_key,
            _ => task.titulo.clone(),
        };
        if let Err(e) = crate::jira::worktree::ensure_issue_worktree(
            state,
            site,
            issue,
            &project_id,
            &task_id,
            &summary,
        )
        .await
        {
            tracing::warn!(
                error = %e, jira_site = %site, jira_issue_id = %issue, task = %task_id,
                "ensure_issue_worktree failed during accept; task created without worktree"
            );
        }
    }
    emit_tasks_changed(state, &task_id);
    Ok(task_id)
}

/// Render an accepted proposal into the derived task's markdown body so
/// the task keeps the full context the agent reported. Mirrors the fields
/// shown in the triage modal (pt-BR primary locale).
fn proposta_to_body(p: &Proposta) -> String {
    let mut body = String::new();
    let file = p.file.trim();
    if !file.is_empty() {
        body.push_str(&format!("**Arquivo:** {file}\n\n"));
    }
    body.push_str(&format!("## Como reproduzir\n{}\n\n", p.repro.trim()));
    body.push_str(&format!("## O que falhou\n{}\n\n", p.what_failed.trim()));
    body.push_str(&format!("## Ação proposta\n{}\n", p.action.trim()));
    body.push_str(&format!("\n---\nDerivada da proposta {}.\n", p.proposta_id));
    body
}

/// Used by the CLI's `propose` path (will go through the NDJSON socket
/// in Phase 4 — this Tauri-side variant is for in-app testing / tooling).
#[tauri::command]
pub async fn propose(
    state: State<'_, Arc<AppState>>,
    args: NewProposta,
) -> Result<Proposta, String> {
    // Hardening (Slice 2 §C): the public propose surface must not let a
    // caller forge a Jira identity. Only `jira_materialize` (which stamps
    // identity server-side from a verified capability secret) may set
    // these. Mirrors the IPC `OP_PROPOSE` guard.
    if args.jira_site.is_some() || args.jira_issue_id.is_some() {
        return Err("jira_site/jira_issue_id may only be set via jira_materialize".to_string());
    }
    state.repo.propose(args).await.map_err(to_str_err)
}

#[tauri::command]
pub async fn await_proposta_decisao(
    state: State<'_, Arc<AppState>>,
    proposta_id: String,
    timeout_ms: u64,
) -> Result<Option<DecisaoRegistro>, String> {
    state
        .repo
        .await_decisao(&proposta_id, Duration::from_millis(timeout_ms))
        .await
        .map_err(to_str_err)
}

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

/// Outcome of an `export_diagnostics` call. `Cancelled` is a normal,
/// non-error result (the user dismissed the save dialog); the UI shows a
/// neutral status rather than an error in that case.
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DiagnosticsExport {
    Saved { path: String, logs: usize },
    Cancelled,
}

/// Build a redacted diagnostics zip (rolling logs + env info) and let the
/// user choose where to save it. Secrets are scrubbed per the manifest in
/// `diagnostics.rs`; the bundle never includes the auth file or any keyring
/// secret. Returns the chosen path on success, or `Cancelled` if the user
/// dismissed the save dialog.
#[tauri::command]
pub async fn export_diagnostics(app: tauri::AppHandle) -> Result<DiagnosticsExport, String> {
    use tauri_plugin_dialog::DialogExt;

    let data_dir = dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".cadenza");
    let log_dir = crate::observ::log_dir();

    let default_name = format!(
        "cadenza-diagnostics-{}.zip",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );

    // The blocking dialog API would deadlock the async runtime if awaited
    // directly; the channel bridges the dialog's callback back here.
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_file_name(&default_name)
        .add_filter("Zip", &["zip"])
        .save_file(move |path| {
            let _ = tx.send(path);
        });
    let chosen = rx.await.map_err(to_str_err)?;
    let Some(dest) = chosen else {
        tracing::info!("diagnostics export cancelled by user");
        return Ok(DiagnosticsExport::Cancelled);
    };
    let dest = dest
        .into_path()
        .map_err(|e| format!("invalid destination path: {e}"))?;

    let inputs = crate::diagnostics::BundleInputs {
        app_version: env!("CARGO_PKG_VERSION"),
        protocol_version: cadenza_proto::MAX_PROTOCOL,
        data_dir,
        log_dir,
    };
    let logs = crate::diagnostics::write_bundle(&dest, &inputs).map_err(to_str_err)?;

    Ok(DiagnosticsExport::Saved {
        path: dest.display().to_string(),
        logs,
    })
}

/// Reveal the rolling-log directory in the OS file manager so the user can
/// inspect logs directly without exporting a bundle. Creates the directory
/// first if it doesn't exist yet (e.g. logging failed to init at boot), so
/// the open never fails with "path not found". Uses the platform's native
/// file-manager launcher directly to avoid adding another Tauri plugin +
/// capability for a single reveal action.
#[tauri::command]
pub fn open_logs_folder() -> Result<(), String> {
    let dir = crate::observ::log_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = ?e, "open_logs_folder: create_dir_all failed");
    }

    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer").arg(&dir).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(&dir).spawn();

    match result {
        // `explorer.exe` returns exit code 1 even on success, so we only
        // gate on the spawn itself succeeding, not on the child's status.
        Ok(_) => {
            tracing::info!(dir = %dir.display(), "opened logs folder");
            Ok(())
        }
        Err(e) => {
            tracing::warn!(error = ?e, "failed to open logs folder");
            Err(to_str_err(e))
        }
    }
}

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
