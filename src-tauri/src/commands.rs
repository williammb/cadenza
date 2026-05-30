//! Tauri `#[command]` handlers — the in-process IPC surface used by the
//! React frontend. Per DESIGN-desktop-v2.md § "commands.rs". The CLI
//! talks to the app over a separate NDJSON socket (Phase 4), not these
//! handlers.

use cadenza_i18n::{locale, FluentArgs, I18n, LocaleSources};
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

use crate::agent::{self, CodexCapture, LaunchPlan, PromptDelivery};
use crate::config::{AgenteKind, Config, PgConfig, PgSslMode, StorageBackend};
use crate::ordering::TaskOrder;
use crate::projects::TaskProjects;
use crate::runs::{TaskRun, TaskRuns};
use crate::secrets;
use crate::spawn::{PtyHandle, SpawnConfig};
use crate::store::{
    migrate, Decisao, DecisaoRegistro, Estado, FileRepository, Ideia, IdeiaStatus, NewProposta,
    PgConnectionParams, PgRepository, PgSslModeChoice, Proposta, Repository, SqliteRepository,
    Task,
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

        let env_lang = locale::read_env();
        let active_locale = locale::resolve(LocaleSources {
            flag: None,
            env: env_lang.as_deref(),
            config: config.locale.as_deref(),
        });
        tracing::info!(locale = %active_locale, "i18n initialized");
        let i18n = I18n::new(&active_locale);

        let task_projects = Arc::new(TaskProjects::load(&home)?);
        let task_runs = Arc::new(TaskRuns::load(&home)?);
        let task_worktrees = Arc::new(TaskWorktrees::load(&home)?);
        let task_order = Arc::new(TaskOrder::load(&home)?);

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
            task_order,
            app_handle: Mutex::new(None),
            agent_models: Mutex::new(seeded_models),
            token_epoch: AtomicU64::new(0),
            decision_lock: tokio::sync::Mutex::new(()),
        })
    }
}

/// Amarra tasks órfãs (sem entrada em `task-projects.json`) ao primeiro
/// projeto do config. Chamado em `AppState::init` antes de qualquer
/// comando rodar — preserva a constraint "toda task tem projeto" para
/// bases migradas da versão Node.js legacy. Se não há projetos, retorna
/// sem fazer nada; a UI detecta esse estado e guia o usuário a criar o
/// primeiro projeto.
fn ensure_default_project_and_bind_orphans(
    config: &Config,
    task_projects: &TaskProjects,
    repo: &dyn Repository,
) -> anyhow::Result<()> {
    if config.projects.is_empty() {
        return Ok(());
    }

    let default_project_id = config.projects[0].id.clone();
    let mapping = task_projects.snapshot();

    // Bloqueante / síncrono: `repo.list_tasks` é async mas estamos
    // num init síncrono. Mesmo padrão usado por `build_repo` para
    // migrações de backend.
    let tasks = tauri::async_runtime::block_on(async { repo.list_tasks(None).await })
        .map_err(|e| anyhow::anyhow!("list_tasks during orphan migration: {e}"))?;

    let mut bound = 0usize;
    for task in tasks {
        if !mapping.contains_key(&task.id) {
            task_projects.set(&task.id, Some(&default_project_id))?;
            bound += 1;
        }
    }
    if bound > 0 {
        tracing::info!(bound, project = %default_project_id, "bound orphan tasks");
    }
    Ok(())
}

/// Build the `Repository` impl matching `config.storage_backend`,
/// running a file→backend migration on first activation. The Files
/// backend is always opened (it's the source of historical data) so
/// the migration has something to read.
fn build_repo(home: &std::path::Path, config: &Config) -> anyhow::Result<Arc<dyn Repository>> {
    let files = Arc::new(FileRepository::new(home)?);
    let marker = home.join("migrated.json");
    match config.storage_backend {
        StorageBackend::Files => Ok(files),
        StorageBackend::Sqlite => {
            let db_path = home.join("cadenza.db");
            let sqlite: SqliteRepository =
                tauri::async_runtime::block_on(async { SqliteRepository::open(&db_path).await })?;
            let sqlite = Arc::new(sqlite);
            let files_dyn: Arc<dyn Repository> = files.clone();
            let sqlite_dyn: Arc<dyn Repository> = sqlite.clone();
            tauri::async_runtime::block_on(async {
                migrate::maybe_migrate(
                    &*files_dyn,
                    &*sqlite_dyn,
                    migrate::Backend::Files,
                    migrate::Backend::Sqlite,
                    &marker,
                )
                .await
            })?;
            Ok(sqlite)
        }
        StorageBackend::Postgres => {
            let Some(pg_cfg) = config.postgres.as_ref() else {
                tracing::warn!("postgres selected but no config; falling back to files");
                return Ok(files);
            };
            let params = match load_pg_params(pg_cfg) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "postgres password unavailable; falling back to files");
                    return Ok(files);
                }
            };
            let pg = match tauri::async_runtime::block_on(PgRepository::open(&params)) {
                Ok(p) => Arc::new(p),
                Err(e) => {
                    tracing::warn!(error = %e, "postgres open failed; falling back to files");
                    return Ok(files);
                }
            };
            let files_dyn: Arc<dyn Repository> = files.clone();
            let pg_dyn: Arc<dyn Repository> = pg.clone();
            tauri::async_runtime::block_on(async {
                migrate::maybe_migrate(
                    &*files_dyn,
                    &*pg_dyn,
                    migrate::Backend::Files,
                    migrate::Backend::Postgres,
                    &marker,
                )
                .await
            })?;
            Ok(pg)
        }
    }
}

/// Map `PgConfig` + keyring password into the sqlx-shaped params the
/// store layer wants. Kept private to commands.rs so the keyring
/// account-format stays in one place (`secrets::account_for`).
fn load_pg_params(cfg: &PgConfig) -> anyhow::Result<PgConnectionParams> {
    let account = secrets::account_for(&cfg.user, &cfg.host, cfg.port, &cfg.database);
    let password = secrets::get_password(&account)
        .map_err(|e| anyhow::anyhow!("postgres password from keyring: {e}"))?;
    Ok(PgConnectionParams {
        host: cfg.host.clone(),
        port: cfg.port,
        database: cfg.database.clone(),
        user: cfg.user.clone(),
        password,
        ssl_mode: pg_ssl_choice(cfg.ssl_mode),
    })
}

fn pg_ssl_choice(mode: PgSslMode) -> PgSslModeChoice {
    match mode {
        PgSslMode::Disable => PgSslModeChoice::Disable,
        PgSslMode::Prefer => PgSslModeChoice::Prefer,
        PgSslMode::Require => PgSslModeChoice::Require,
    }
}

fn to_str_err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

// ───────────────────────── tasks ─────────────────────────

#[tauri::command]
pub async fn list_tasks(
    state: State<'_, Arc<AppState>>,
    estado: Option<String>,
) -> Result<Vec<Task>, String> {
    let filter = estado.as_deref().and_then(Estado::parse);
    let tasks = state.repo.list_tasks(filter).await.map_err(to_str_err)?;
    let mut tasks: Vec<Task> = tasks
        .into_iter()
        .map(|t| state.task_worktrees.enrich(t))
        .collect();
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
    Ok(state.task_worktrees.enrich(task))
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
    state.repo.create_task(&task).await.map_err(to_str_err)?;
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
    if let Err(e) = state.task_order.forget(&id) {
        tracing::warn!(error = ?e, task = %id, "task_order.forget failed");
    }
    // Drop any images the task body referenced. Best-effort: the task is
    // already gone, orphaned files only cost disk bytes.
    crate::attachments::delete_owner("tasks", &id);
    Ok(())
}

// ───────────────────────── attachments ─────────────────────────

/// Image bytes + MIME for the preview, base64-encoded so the JS side can
/// build a `data:` URL without a second round-trip.
#[derive(Serialize)]
pub struct AttachmentData {
    pub mime: String,
    pub base64: String,
}

/// Map a typed attachment error to the stable i18n key the UI translates.
/// Keeping the mapping here (not in `attachments.rs`) keeps that module
/// free of any i18n / UI coupling.
fn attachment_error_key(e: &crate::attachments::AttachmentError) -> String {
    use crate::attachments::AttachmentError as E;
    match e {
        E::UnsupportedFormat => "attachment-error-unsupported-format",
        E::TooLarge => "attachment-error-too-large",
        _ => "attachment-error-save-failed",
    }
    .to_string()
}

/// Persist an image for a task/ideia body and return its relative path
/// (`attachments/<kind>/<owner_id>/<hash>.<ext>`) for the JS to embed as
/// `![](rel)`. Validation (format + size) lives in `attachments`; on
/// failure we log the English detail and return a translatable key.
#[tauri::command]
pub fn save_attachment(kind: String, owner_id: String, bytes: Vec<u8>) -> Result<String, String> {
    crate::attachments::save(&kind, &owner_id, &bytes).map_err(|e| {
        tracing::warn!(error = ?e, kind = %kind, owner = %owner_id, "save_attachment failed");
        attachment_error_key(&e)
    })
}

/// Read an attachment back as base64 for the markdown preview. Errors are
/// non-fatal to the caller — the preview just falls back to showing the
/// image `alt` text for an orphaned reference.
#[tauri::command]
pub fn read_attachment(rel_path: String) -> Result<AttachmentData, String> {
    use base64::Engine;
    let (mime, bytes) = crate::attachments::read(&rel_path).map_err(|e| {
        tracing::warn!(error = ?e, rel = %rel_path, "read_attachment failed");
        e.to_string()
    })?;
    Ok(AttachmentData {
        mime,
        base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

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
fn suggested_worktree_path(repo: &Path, branch: &str) -> PathBuf {
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
    Ok(task.map(|t| state.task_worktrees.enrich(t)))
}

// ───────────────────────── triage ─────────────────────────

#[tauri::command]
pub async fn list_pending_propostas(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<Proposta>, String> {
    state
        .repo
        .list_pending_propostas()
        .await
        .map_err(to_str_err)
}

#[tauri::command]
pub async fn read_proposta(
    state: State<'_, Arc<AppState>>,
    proposta_id: String,
) -> Result<Option<Proposta>, String> {
    state
        .repo
        .read_proposta(&proposta_id)
        .await
        .map_err(to_str_err)
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
    };
    state.repo.create_task(&task).await.map_err(to_str_err)?;
    state
        .task_projects
        .set(&task_id, Some(&project_id))
        .map_err(to_str_err)?;
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

// ───────────────────────── PTY / terminal ─────────────────────────

#[derive(Debug, Deserialize)]
pub struct PtySpawnArgs {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub session_id_hint: Option<String>,
}

fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

#[derive(Debug, Serialize)]
pub struct PtySpawnResult {
    pub session_id: String,
}

#[tauri::command]
pub fn pty_spawn(
    state: State<'_, Arc<AppState>>,
    args: PtySpawnArgs,
) -> Result<PtySpawnResult, String> {
    let claude_session_id = args
        .session_id_hint
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());

    let mut cfg = SpawnConfig::new(args.command)
        .args(args.args)
        .size(args.cols, args.rows);
    if let Some(d) = args.cwd {
        cfg = cfg.cwd(d);
    }
    for (k, v) in args.env {
        cfg = cfg.env(k, v);
    }
    if let (Some(pid), Some(tid)) = (args.project_id.as_ref(), args.task_id.as_ref()) {
        cfg = cfg.cadenza_env(pid, tid, &claude_session_id);
    }

    let pty = PtyHandle::spawn(cfg).map_err(to_str_err)?;
    let session_id = format!("S-{}", Uuid::new_v4().simple());
    let session = TerminalSession::start(session_id.clone(), pty).map_err(to_str_err)?;
    state
        .sessions
        .lock()
        .map_err(to_str_err)?
        .insert(session_id.clone(), session);
    tracing::info!(session = %session_id, "pty session started");
    Ok(PtySpawnResult { session_id })
}

#[tauri::command]
pub fn pty_write(
    state: State<'_, Arc<AppState>>,
    session_id: String,
    data: Vec<u8>,
) -> Result<(), String> {
    let session = state
        .sessions
        .lock()
        .map_err(to_str_err)?
        .get(&session_id)
        .cloned()
        .ok_or_else(|| format!("session {session_id} not found"))?;
    session.write(&data).map_err(to_str_err)
}

#[tauri::command]
pub fn pty_resize(
    state: State<'_, Arc<AppState>>,
    session_id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let session = state
        .sessions
        .lock()
        .map_err(to_str_err)?
        .get(&session_id)
        .cloned()
        .ok_or_else(|| format!("session {session_id} not found"))?;
    session.resize(cols, rows).map_err(to_str_err)
}

#[tauri::command]
pub fn pty_kill(state: State<'_, Arc<AppState>>, session_id: String) -> Result<(), String> {
    let session = state
        .sessions
        .lock()
        .map_err(to_str_err)?
        .remove(&session_id)
        .ok_or_else(|| format!("session {session_id} not found"))?;
    session.kill().map_err(to_str_err)
}

#[tauri::command]
pub fn pty_snapshot(
    state: State<'_, Arc<AppState>>,
    session_id: String,
) -> Result<Vec<u8>, String> {
    let session = state
        .sessions
        .lock()
        .map_err(to_str_err)?
        .get(&session_id)
        .cloned()
        .ok_or_else(|| format!("session {session_id} not found"))?;
    Ok(session.snapshot())
}

/// Stream PTY bytes to the frontend over a Tauri channel. The frontend
/// constructs `new Channel<number[]>()` and passes it as the `channel`
/// arg — the first message is the current scrollback, subsequent
/// messages are live chunks.
#[tauri::command]
pub async fn pty_attach(
    state: State<'_, Arc<AppState>>,
    session_id: String,
    channel: Channel<Vec<u8>>,
) -> Result<(), String> {
    let session = {
        let sessions = state.sessions.lock().map_err(to_str_err)?;
        sessions
            .get(&session_id)
            .cloned()
            .ok_or_else(|| format!("session {session_id} not found"))?
    };

    // Replay scrollback first so reattaches don't lose context. Snapshot
    // and subscription are paired atomically so chunks produced during
    // attach are not lost between the two operations.
    let (snap, mut rx) = session.subscribe_with_snapshot();
    if !snap.is_empty() {
        let _ = channel.send(snap);
    }

    let handle = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(bytes) => {
                    if channel.send(bytes).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(lagged = n, "pty broadcast lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    // Keep at most one stream loop alive per session: a re-attach (e.g.
    // after a webview reload) aborts the previous loop so the old
    // subscriber can't keep draining the broadcast into a dead channel.
    session.set_attach_task(handle.abort_handle());

    Ok(())
}

// ───────────────────────── i18n / config ─────────────────────────

#[tauri::command]
pub fn get_locale(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    Ok(state.i18n.lock().map_err(to_str_err)?.active().to_string())
}

/// Return every UI translation string for `locale` as a flat
/// `{ key: value }` map. The UI calls this once at boot and resolves
/// `data-i18n` lookups locally — see DESIGN-desktop-v4.md
/// § "Internacionalização (i18n) — sistema único".
#[tauri::command]
pub fn load_translations(locale: String) -> HashMap<String, String> {
    let normalized = locale::normalize(&locale);
    let i18n = I18n::new(&normalized);
    i18n.dump_namespace_strings("ui")
}

#[tauri::command]
pub fn set_locale(state: State<'_, Arc<AppState>>, locale: String) -> Result<String, String> {
    let normalized = locale::normalize(&locale);
    *state.i18n.lock().map_err(to_str_err)? = I18n::new(&normalized);
    tracing::info!(locale = %normalized, "i18n locale changed");
    Ok(normalized)
}

#[tauri::command]
pub fn get_config(state: State<'_, Arc<AppState>>) -> Result<Config, String> {
    Ok(state.config.lock().map_err(to_str_err)?.clone())
}

/// Relaunch the app. Used by Settings → Storage to apply a backend
/// change without making the user close + reopen by hand.
#[tauri::command]
pub fn restart_app(app: tauri::AppHandle) {
    tracing::info!("restart requested from UI");
    app.restart();
}

/// Outcome of a manual update check, returned to the UI so the Settings
/// button can show inline feedback for *both* the "you're up to date"
/// and "update available" cases. The hourly ticker stays fire-and-forget
/// via `check_for_updates`, which only surfaces something when there's a
/// newer build.
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UpdateCheckResult {
    Available { version: String },
    UpToDate,
}

/// Manually poll the updater on demand from the Settings button. Unlike
/// the silent hourly ticker, this returns the outcome so the UI can render
/// "up to date" vs "vX available". On the available case it still emits
/// `update_available` so the existing banner appears, but skips the OS
/// notification the ticker uses — the user is already looking at the app
/// and gets the inline result instead.
#[tauri::command]
pub async fn check_update(app: tauri::AppHandle) -> Result<UpdateCheckResult, String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(to_str_err)?;
    match updater.check().await.map_err(to_str_err)? {
        Some(update) => {
            let version = update.version.clone();
            tracing::info!(version = %version, "update available (manual check)");
            if let Err(e) = app.emit("update_available", &version) {
                tracing::warn!(error = ?e, "emit update_available");
            }
            Ok(UpdateCheckResult::Available { version })
        }
        None => {
            tracing::debug!("no update available (manual check)");
            Ok(UpdateCheckResult::UpToDate)
        }
    }
}

/// Download the pending release and relaunch the app into it. Backs
/// the "Reiniciar agora" button on the `update_available` banner; on
/// success the call site never observes the `Ok(())` because
/// `app.restart()` exits the process. Surfaces an `Err` when the
/// updater handle is missing, the check itself failed, or there's
/// nothing to install (i.e. the banner was stale because the user
/// caught up via the tray "Reiniciar" item already).
#[tauri::command]
pub async fn install_update_and_restart(app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(to_str_err)?;
    let update = updater
        .check()
        .await
        .map_err(to_str_err)?
        .ok_or_else(|| "no update available".to_string())?;
    let version = update.version.clone();
    tracing::info!(version = %version, "downloading update");
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(to_str_err)?;
    tracing::info!(version = %version, "update installed; restarting");
    app.restart();
}

/// Change the active storage backend. Writes the new value to
/// `config.json`; the actual switch (open SQLite/Postgres, run the
/// file→backend migration) happens at the next `AppState::init` so
/// the UI MUST follow this with a restart to take effect. Returning
/// here without restarting leaves the in-memory `repo` pointing at
/// the previous backend, which is intentional — see Fase B notes.
#[tauri::command]
pub fn set_storage_backend(
    state: State<'_, Arc<AppState>>,
    backend: StorageBackend,
) -> Result<Config, String> {
    let path = dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".cadenza")
        .join("config.json");
    let mut slot = state.config.lock().map_err(to_str_err)?;
    slot.storage_backend = backend;
    slot.save_to(&path).map_err(to_str_err)?;
    tracing::info!(?backend, "storage backend changed; restart to apply");
    Ok(slot.clone())
}

/// Test a Postgres connection without committing anything. The
/// password is passed inline (the UI hasn't necessarily stored it in
/// the keyring yet — the flow is "test, then save"). Returns Ok on
/// success or an error string the UI surfaces.
#[tauri::command]
pub async fn test_db_connection(
    host: String,
    port: u16,
    database: String,
    user: String,
    password: String,
    ssl_mode: PgSslMode,
) -> Result<(), String> {
    let params = PgConnectionParams {
        host,
        port,
        database,
        user,
        password,
        ssl_mode: pg_ssl_choice(ssl_mode),
    };
    PgRepository::ping(&params).await.map_err(to_str_err)
}

/// Persist the Postgres password to the OS keyring under the account
/// key derived from `(user, host, port, database)`. The password never
/// touches `config.json`. Idempotent — overwrites an existing entry.
#[tauri::command]
pub fn set_pg_password(
    host: String,
    port: u16,
    database: String,
    user: String,
    password: String,
) -> Result<(), String> {
    let account = secrets::account_for(&user, &host, port, &database);
    secrets::set_password(&account, &password).map_err(to_str_err)
}

/// Remove the Postgres password from the keyring. Used by the Settings
/// UI when the user clears or rotates credentials. Returns Ok even if
/// the entry didn't exist (idempotent — matches `delete_password`).
#[tauri::command]
pub fn clear_pg_password(
    host: String,
    port: u16,
    database: String,
    user: String,
) -> Result<(), String> {
    let account = secrets::account_for(&user, &host, port, &database);
    secrets::delete_password(&account).map_err(to_str_err)
}

/// Persist a full Config replacement to `~/.cadenza/config.json` and
/// hot-swap the in-memory copy. The UI's Settings modal sends the whole
/// document — there's no patch surface — so this is a simple overwrite.
#[tauri::command]
pub fn save_config(state: State<'_, Arc<AppState>>, config: Config) -> Result<Config, String> {
    let path = dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".cadenza")
        .join("config.json");
    config.save_to(&path).map_err(to_str_err)?;
    // Rebind orphans now that a project exists. Handles the legacy
    // migration case: first install with pre-existing Node.js tasks and
    // zero projects — AppState::init skipped binding; this is the next
    // hook that can repair the invariant.
    ensure_default_project_and_bind_orphans(
        &config,
        state.task_projects.as_ref(),
        state.repo.as_ref(),
    )
    .map_err(to_str_err)?;
    let mut slot = state.config.lock().map_err(to_str_err)?;
    *slot = config.clone();
    tracing::info!(path = %path.display(), "config saved");
    Ok(config)
}

// ───────────────────────── agent runs ─────────────────────────

#[derive(Debug, Serialize)]
pub struct StartTaskAgentResult {
    pub session_id: String,
    pub conversation_id: Option<String>,
    pub resumed: bool,
}

/// Whether `start_task_agent` runs the task or plans it. In `Plan` mode
/// the agent is told to interview the human and persist a refined plan
/// (via `cadenza-cli plan`) instead of implementing; the task stays in
/// `a_fazer` and no run record is kept, so a later `Execute` run is a
/// clean start that reads the saved plan from the task body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskAgentMode {
    #[default]
    Execute,
    Plan,
}

/// Launch the configured agent CLI (Claude Code / Codex) in a PTY
/// running inside the task's project directory. The frontend then
/// calls `pty_attach` with the returned `session_id` to stream output.
///
/// Persists the run in `~/.cadenza/task-runs.json` so a subsequent call
/// for the same `task_id` becomes a resume.
///
/// Returns errors as user-facing strings (the UI surfaces them in a
/// toast). Failure modes the user is expected to fix:
///   - task not found / not in `fazendo`
///   - task has no project mapping (can't decide a cwd)
///   - configured project path doesn't exist
///   - CLI binary not on PATH (and no override in Settings)
#[tauri::command]
pub async fn start_task_agent(
    state: State<'_, Arc<AppState>>,
    task_id: String,
    agent_kind: AgenteKind,
    model: String,
    // Absent/null from older callers → `Execute`.
    mode: Option<TaskAgentMode>,
) -> Result<StartTaskAgentResult, String> {
    let mode = mode.unwrap_or_default();
    // 1. Task must exist and not be `feito`. The transition to `fazendo`
    //    (if not already there) happens AFTER a successful spawn — see
    //    step 5b — so a failed start doesn't leave the kanban moved.
    let task = state.repo.read_task(&task_id).await.map_err(to_str_err)?;
    if task.estado == Estado::Feito {
        return Err(format!(
            "task '{}' is in state '{}', can't start an agent on a completed task",
            task_id,
            task.estado.as_str()
        ));
    }
    if mode == TaskAgentMode::Plan && task.estado != Estado::AFazer {
        return Err(format!(
            "task '{}' is in state '{}'; plan mode requires the task to be in a_fazer",
            task_id,
            task.estado.as_str()
        ));
    }
    let original_estado = task.estado;
    let task_titulo = task.titulo.clone();

    // 2. Resolve project + cwd.
    let project_id = state
        .task_projects
        .snapshot()
        .get(&task_id)
        .cloned()
        .ok_or_else(|| {
            format!(
                "task '{}' has no project assigned — assign one in the card menu so the agent has a working directory",
                task_id
            )
        })?;

    let (project_path, command_override) = {
        let cfg = state.config.lock().map_err(to_str_err)?;
        let project = cfg
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .ok_or_else(|| format!("project '{}' not found in config", project_id))?;
        let project_path = project.path.clone();
        // Per-project agente override wins over the global one.
        let cmd = project
            .agente
            .as_ref()
            .and_then(|a| a.command.clone())
            .or_else(|| cfg.agente.as_ref().and_then(|a| a.command.clone()));
        (project_path, cmd)
    };

    if !project_path.exists() {
        return Err(format!(
            "project path does not exist: {} — fix it in Settings → Projetos",
            project_path.display()
        ));
    }

    // Prepare the git workspace from the task's declarative config: pull the
    // origin branch, create/switch the destination branch, and create the
    // worktree when requested. A pull or git failure blocks the start with
    // the error surfaced to the caller. `cwd` is the worktree when used,
    // otherwise the project repo.
    let cwd = prepare_task_workspace(&state, &task_id).await?;

    // 3. Decide new vs resume from `task-runs.json`. Plan mode always
    //    starts fresh: planning runs are never recorded, and matching an
    //    earlier *execution* conversation would resume the wrong posture.
    let existing = match mode {
        TaskAgentMode::Plan => None,
        TaskAgentMode::Execute => state.task_runs.get(&task_id),
    };
    // Resume only when the saved entry agrees with the user's current
    // choice. Switching agent/model means a new conversation. (Claude
    // can change --model on resume but the agent kind has to match;
    // simpler to start fresh on any change.)
    let existing_conv_id = existing.as_ref().and_then(|r| {
        if r.agent == agent_kind && r.conversation_id.is_some() {
            r.conversation_id.clone()
        } else {
            None
        }
    });
    let resumed = existing_conv_id.is_some();

    // 4. Render the initial prompt (fresh start only) and build the
    //    SpawnConfig via the per-agent planner. Preferred delivery is argv:
    //    the planner bakes the prompt into the command line so the backend
    //    never types into the live PTY (no race with the agent's UI boot).
    let initial_prompt: Option<String> = if resumed {
        None
    } else {
        Some(render_initial_task_prompt(
            &state.i18n,
            &task_id,
            &task_titulo,
            mode,
        ))
    };
    let plan: LaunchPlan = agent::plan_launch(
        agent_kind,
        &model,
        command_override.as_deref(),
        &cwd,
        &task_id,
        &project_id,
        existing_conv_id.as_deref(),
        initial_prompt.as_deref(),
    );
    let LaunchPlan {
        spawn,
        conversation_id_known,
        pending_codex_capture,
        prompt_delivery,
    } = plan;

    // 5. Spawn PTY + register session in AppState.
    let pty = PtyHandle::spawn(spawn).map_err(|e| {
        // ENOENT-style failures are the most common; surface a clear hint.
        format!("failed to start agent: {e}. Is the CLI installed and on PATH? You can override the binary path in Settings.")
    })?;
    let session_id = format!("S-{}", Uuid::new_v4().simple());
    let session = TerminalSession::start(session_id.clone(), pty).map_err(to_str_err)?;
    state
        .sessions
        .lock()
        .map_err(to_str_err)?
        .insert(session_id.clone(), session.clone());
    tracing::info!(
        task = %task_id, agent = ?agent_kind, model = %model, resumed,
        session = %session_id, "task agent started"
    );

    // 5a. Deliver the initial prompt on a fresh start. The planner has
    //     already baked it into the spawn argv for agents that support it
    //     (PromptDelivery::Argv) — those need nothing here. Only agents
    //     without a verified initial-prompt flag fall back to typing it
    //     into the PTY after a delay, which races the agent's UI boot.
    if let (Some(prompt), PromptDelivery::TypeIn) = (initial_prompt, prompt_delivery) {
        let session_for_prompt = session.clone();
        tauri::async_runtime::spawn(async move {
            send_initial_prompt(&session_for_prompt, &prompt).await;
        });
    }

    // 5b. With the spawn confirmed, move the task to `fazendo` if it
    //     wasn't already. Logged-only on failure: the agent is already
    //     running, the user can move the card manually if needed.
    //     Plan mode leaves the task in `a_fazer` — planning happens
    //     *before* execution, so the card must not move yet.
    if mode == TaskAgentMode::Execute && original_estado != Estado::Fazendo {
        if let Err(e) = state.repo.set_estado(&task_id, Estado::Fazendo).await {
            tracing::warn!(error = ?e, task = %task_id, "set_estado(fazendo) after spawn failed");
        }
    }

    // 6./7. Persist the run record and (Codex/Antigravity first-run only)
    //        kick off async session-UUID capture — but ONLY for execution
    //        runs. A planning run is intentionally not recorded so it can't
    //        be resumed into a later execution; with no record there is also
    //        nothing for the capture task to patch.
    if mode == TaskAgentMode::Execute {
        let run = TaskRun {
            agent: agent_kind,
            model: model.clone(),
            conversation_id: conversation_id_known.clone(),
            last_started_at: chrono::Utc::now(),
            last_session_id: Some(session_id.clone()),
        };
        if let Err(e) = state.task_runs.upsert(&task_id, run) {
            tracing::warn!(error = ?e, task = %task_id, "task_runs.upsert failed");
        }

        if let Some(capture) = pending_codex_capture {
            let task_runs = state.task_runs.clone();
            let app_handle = state.app_handle.lock().ok().and_then(|h| h.clone());
            let task_id_clone = task_id.clone();
            tauri::async_runtime::spawn(async move {
                let found = wait_for_codex_uuid(capture).await;
                match found {
                    Some(uuid) => {
                        if let Err(e) = task_runs.set_conversation_id(&task_id_clone, &uuid) {
                            tracing::warn!(error = ?e, task = %task_id_clone, "set_conversation_id failed");
                        } else {
                            tracing::info!(task = %task_id_clone, uuid = %uuid, "captured codex session uuid");
                            if let Some(app) = app_handle {
                                let _ = app.emit("task_run_changed", &task_id_clone);
                            }
                        }
                    }
                    None => {
                        tracing::warn!(task = %task_id_clone, "codex uuid capture timed out");
                    }
                }
            });
        }
    }

    Ok(StartTaskAgentResult {
        session_id,
        conversation_id: conversation_id_known,
        resumed,
    })
}

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

/// Resolve the localized initial prompt sent to the agent when a task
/// is started fresh. The key depends on `mode`: execution uses
/// `agent-initial-prompt`, planning uses `agent-planning-prompt`. Falls
/// back to a plain English message if the key isn't in either bundle.
fn render_initial_task_prompt(
    i18n_slot: &Mutex<I18n>,
    task_id: &str,
    titulo: &str,
    mode: TaskAgentMode,
) -> String {
    let key = match mode {
        TaskAgentMode::Execute => "agent-initial-prompt",
        TaskAgentMode::Plan => "agent-planning-prompt",
    };
    let mut args = FluentArgs::new();
    args.set("task_id", task_id.to_string());
    args.set("titulo", titulo.to_string());
    match i18n_slot.lock() {
        Ok(i18n) => i18n.t_with(key, Some(&args)),
        Err(_) => match mode {
            TaskAgentMode::Execute => format!(
                "Use the `cadenza` skill to coordinate with Cadenza through cadenza-cli. Your task is {task_id} ({titulo}). Start by running `cadenza-cli current --json`."
            ),
            TaskAgentMode::Plan => format!(
                "Use the `cadenza` skill to coordinate with Cadenza. You are in PLANNING mode for task {task_id} ({titulo}) — do NOT write or run any code yet. Read the task with `cadenza-cli list --json` and find {task_id}. Ask clarifying questions, in batches, until scope and acceptance criteria are clear. When we agree, save the plan by piping markdown into stdin: `cadenza-cli plan {task_id}` (omit --body so the plan is read from stdin). Do not mark anything done and do not start implementing — a separate execution run comes later."
            ),
        },
    }
}

fn render_initial_ideia_prompt(i18n_slot: &Mutex<I18n>, ideia_id: &str) -> String {
    let mut args = FluentArgs::new();
    args.set("ideia_id", ideia_id.to_string());
    match i18n_slot.lock() {
        Ok(i18n) => i18n.t_with("agent-initial-prompt-ideia", Some(&args)),
        Err(_) => format!(
            "Use the `cadenza` skill to coordinate with Cadenza through cadenza-cli. Break the ideia {ideia_id} down into actionable tasks."
        ),
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

#[tauri::command]
pub fn read_task_run(
    state: State<'_, Arc<AppState>>,
    task_id: String,
) -> Result<Option<TaskRun>, String> {
    Ok(state.task_runs.get(&task_id))
}

#[tauri::command]
pub fn list_task_runs(state: State<'_, Arc<AppState>>) -> Result<HashMap<String, TaskRun>, String> {
    Ok(state.task_runs.snapshot())
}

#[tauri::command]
pub fn clear_task_run(state: State<'_, Arc<AppState>>, task_id: String) -> Result<(), String> {
    state.task_runs.forget(&task_id).map_err(to_str_err)
}

// ───────────────────────── ideias (Inbox) ─────────────────────────
//
// Surface paralela à de tasks. Diferentemente das tasks, ideias têm o
// `project_id` no próprio registro — não dependem do side-mapping.
// O servidor mintava `id` e `created_at_ms` quando ausentes para que
// a UI possa só preencher `titulo` + `body` + `project_id`.

#[tauri::command]
pub async fn list_ideias(state: State<'_, Arc<AppState>>) -> Result<Vec<Ideia>, String> {
    state.repo.list_ideias().await.map_err(to_str_err)
}

#[tauri::command]
pub async fn read_ideia(
    state: State<'_, Arc<AppState>>,
    id: String,
) -> Result<Option<Ideia>, String> {
    state.repo.read_ideia(&id).await.map_err(to_str_err)
}

#[derive(Debug, Deserialize)]
pub struct NewIdeiaArgs {
    #[serde(default)]
    pub id: Option<String>,
    pub titulo: String,
    #[serde(default)]
    pub body: String,
    pub project_id: String,
}

#[tauri::command]
pub async fn create_ideia(
    state: State<'_, Arc<AppState>>,
    args: NewIdeiaArgs,
) -> Result<Ideia, String> {
    let pid = args.project_id.trim();
    if pid.is_empty() {
        return Err("project_id is required".to_string());
    }
    {
        let cfg = state.config.lock().map_err(to_str_err)?;
        if !cfg.projects.iter().any(|p| p.id == pid) {
            return Err(format!("unknown project_id: {pid}"));
        }
    }
    let id = args
        .id
        .unwrap_or_else(|| format!("I-{}", Uuid::new_v4().simple()));
    let created_at_ms = chrono::Utc::now().timestamp_millis();
    let ideia = Ideia {
        id,
        titulo: args.titulo,
        body: args.body,
        project_id: pid.to_string(),
        status: IdeiaStatus::Pendente,
        created_at_ms,
    };
    state.repo.create_ideia(&ideia).await.map_err(to_str_err)?;
    Ok(ideia)
}

#[tauri::command]
pub async fn delete_ideia(state: State<'_, Arc<AppState>>, id: String) -> Result<(), String> {
    state.repo.delete_ideia(&id).await.map_err(to_str_err)?;
    // Best-effort cleanup of any images embedded in the ideia body.
    crate::attachments::delete_owner("ideias", &id);
    Ok(())
}

#[tauri::command]
pub async fn set_ideia_status(
    state: State<'_, Arc<AppState>>,
    id: String,
    status: IdeiaStatus,
) -> Result<(), String> {
    state
        .repo
        .set_ideia_status(&id, status)
        .await
        .map_err(to_str_err)
}

/// Spawna um agente em PTY na pasta do projeto da ideia, seedando env
/// vars (`CADENZA_IDEIA_ID`, `CADENZA_IDEIA_BODY`) para o agente saber
/// qual ideia destrinchar. O agente roda o skill `cadenza-cli new-task`
/// para criar as tasks resultantes — a UI vê tudo via `tasks_changed`.
///
/// Modelado em `start_task_agent`: mesma sequência de checagens (projeto
/// existe, cwd existe, planejar comando do agente, registrar PTY).
#[tauri::command]
pub async fn destrinchar_ideia(
    state: State<'_, Arc<AppState>>,
    ideia_id: String,
    agent_kind: AgenteKind,
    model: String,
) -> Result<StartTaskAgentResult, String> {
    // 1. Ideia precisa existir.
    let ideia = state
        .repo
        .read_ideia(&ideia_id)
        .await
        .map_err(to_str_err)?
        .ok_or_else(|| format!("ideia '{}' not found", ideia_id))?;

    // 2. Resolver projeto + cwd a partir do `ideia.project_id`.
    let (cwd, command_override) = {
        let cfg = state.config.lock().map_err(to_str_err)?;
        let project = cfg
            .projects
            .iter()
            .find(|p| p.id == ideia.project_id)
            .ok_or_else(|| {
                format!(
                    "project '{}' from ideia not found in config",
                    ideia.project_id
                )
            })?;
        let project_path = project.path.clone();
        let cmd = project
            .agente
            .as_ref()
            .and_then(|a| a.command.clone())
            .or_else(|| cfg.agente.as_ref().and_then(|a| a.command.clone()));
        (project_path, cmd)
    };

    if !cwd.exists() {
        return Err(format!(
            "project path does not exist: {} — fix it in Settings → Projetos",
            cwd.display()
        ));
    }

    // 3. Decomposição é sempre uma nova conversa. Usamos um id sintético
    //    `IDEIA-<id>` no lugar de task_id para que logs e env continuem
    //    fazendo sentido sem precisar entrar em `task-runs.json`.
    let synthetic_task_id = format!("IDEIA-{}", ideia.id);

    // 4. Plan + adiciona env vars específicas da ideia. A decomposição é
    //    sempre fresh, então sempre há um prompt inicial — entregue via
    //    argv quando o agente suporta (igual a `start_task_agent`).
    let prompt = render_initial_ideia_prompt(&state.i18n, &ideia.id);
    let plan: LaunchPlan = agent::plan_launch(
        agent_kind,
        &model,
        command_override.as_deref(),
        &cwd,
        &synthetic_task_id,
        &ideia.project_id,
        None,
        Some(&prompt),
    );
    let LaunchPlan {
        spawn,
        conversation_id_known,
        pending_codex_capture,
        prompt_delivery,
    } = plan;
    let spawn = spawn.ideia_env(&ideia.id, &ideia.body);

    // 5. Spawn PTY + registrar sessão.
    let pty = PtyHandle::spawn(spawn).map_err(|e| {
        format!("failed to start agent: {e}. Is the CLI installed and on PATH? You can override the binary path in Settings.")
    })?;
    let session_id = format!("S-{}", Uuid::new_v4().simple());
    let session = TerminalSession::start(session_id.clone(), pty).map_err(to_str_err)?;
    state
        .sessions
        .lock()
        .map_err(to_str_err)?
        .insert(session_id.clone(), session.clone());
    tracing::info!(
        ideia = %ideia.id, agent = ?agent_kind, model = %model,
        session = %session_id, "destrinchar agent started"
    );

    // 5a. Deliver the initial prompt — argv when the agent supports it,
    //     otherwise type it in (same split as start_task_agent).
    if prompt_delivery == PromptDelivery::TypeIn {
        let session_for_prompt = session.clone();
        tauri::async_runtime::spawn(async move {
            send_initial_prompt(&session_for_prompt, &prompt).await;
        });
    }

    // 6. Capturar UUID do Codex se for o caso (mesmo padrão de
    //    `start_task_agent`). Não armazenamos em `task_runs` porque
    //    decomposição é one-shot — não há "continuar" depois.
    if let Some(capture) = pending_codex_capture {
        tauri::async_runtime::spawn(async move {
            let _ = wait_for_codex_uuid(capture).await;
        });
    }

    Ok(StartTaskAgentResult {
        session_id,
        conversation_id: conversation_id_known,
        resumed: false,
    })
}

// ─────────────────────────── skills (CLI snippet) ───────────────────────────
//
// Wrappers around `skills-core`. The actual filesystem work (writing
// SKILL.md, editing AGENTS.md, deleting on remove) lives in the shared
// crate so the cadenza-cli command and these handlers stay in lockstep.
//
// `skill_install` uses the app's active locale as the body language —
// the Settings UI doesn't expose a locale picker here because switching
// the app language already covers it.

#[tauri::command]
pub fn skill_install(
    state: State<'_, Arc<AppState>>,
    agents: Vec<skills_core::Agent>,
    scope: skills_core::Scope,
    force: bool,
    project_path: Option<String>,
) -> Result<Vec<skills_core::Outcome>, String> {
    let locale = state.i18n.lock().map_err(to_str_err)?.active().to_string();
    let root = project_path.as_deref().map(std::path::Path::new);
    skills_core::install(&agents, scope, &locale, force, root).map_err(to_str_err)
}

#[tauri::command]
pub fn skill_remove(
    agents: Vec<skills_core::Agent>,
    scope: skills_core::Scope,
    project_path: Option<String>,
) -> Result<Vec<skills_core::Outcome>, String> {
    let root = project_path.as_deref().map(std::path::Path::new);
    skills_core::remove(&agents, scope, root).map_err(to_str_err)
}

#[tauri::command]
pub fn skill_status(project_path: Option<String>) -> Result<Vec<skills_core::StatusRow>, String> {
    let root = project_path.as_deref().map(std::path::Path::new);
    Ok(skills_core::status(root))
}

#[tauri::command]
pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[tauri::command]
pub fn list_installed_agents() -> Vec<agent::AgentPresence> {
    agent::list_installed_agents()
}

/// Discover the models the agent's CLI exposes via its interactive
/// `/model` menu. The first call per `agent_kind` per process spawns
/// the binary under a PTY, drives it to the menu, parses the rendered
/// frame, and caches the result. Subsequent calls return the cached
/// list. `refresh=true` skips the cache and re-runs discovery.
///
/// We honor the *global* `Config.agente.command` override (project-level
/// overrides aren't applied here — model availability is per agent
/// install, not per project — and threading a task_id through this
/// surface would be a larger change for marginal correctness).
#[tauri::command]
pub async fn list_agent_models(
    state: State<'_, Arc<AppState>>,
    agent_kind: AgenteKind,
    refresh: Option<bool>,
    cached_only: Option<bool>,
) -> Result<Vec<crate::models::ModelEntry>, String> {
    let command = {
        let cfg = state.config.lock().map_err(to_str_err)?;
        cfg.agente
            .as_ref()
            .and_then(|a| a.command.clone())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| agent::default_command(agent_kind).to_string())
    };
    let cache_key = (agent_kind, command.clone());
    if !refresh.unwrap_or(false) {
        if let Some(cached) = state
            .agent_models
            .lock()
            .map_err(to_str_err)?
            .get(&cache_key)
        {
            return Ok(cached.clone());
        }
    }
    // Task-start path: never spawn the slow probe. A cache miss above means
    // nothing is loaded yet, so return empty and let the UI fall back to a
    // free-text model entry. Discovery lives in Settings → Modelos.
    if cached_only.unwrap_or(false) {
        return Ok(Vec::new());
    }
    // `discover_models` blocks ~10-15 s on PTY warmup + tail. Move it
    // off the tauri runtime so command dispatch (and the UI) stay
    // responsive.
    let cmd_for_spawn = command.clone();
    let entries = tauri::async_runtime::spawn_blocking(move || {
        // predismiss_enters=1: claude shows a trust dialog on first
        // launch in an unknown cwd; codex shows an onboarding step.
        // One Enter handles both with no false negative on already-
        // trusted setups (the extra Enter becomes a no-op at the prompt).
        crate::models::discover_models(&cmd_for_spawn, agent_kind, 8, 6, 1)
    })
    .await
    .map_err(to_str_err)?
    .map_err(|e| {
        let msg = e.to_string();
        // Spawn couldn't find the binary anywhere (PATH + standard install
        // locations). Give an actionable hint instead of the raw os error.
        // Covers the Windows ("os error 2") and Unix/portable-pty
        // ("No viable candidates found in PATH …") not-found phrasings.
        if msg.contains("os error 2")
            || msg.contains("cannot find the file")
            || msg.contains("No viable candidates")
        {
            format!(
                "`{command}` not found on PATH or in its standard install location. \
                 Set its full path in Settings → agent command, or install it on your PATH."
            )
        } else {
            format!("discover_models({command}): {msg}")
        }
    })?;
    if entries.is_empty() {
        return Err(format!(
            "no models parsed from `{command}` — the agent's `/model` menu likely changed shape; please report this"
        ));
    }
    state
        .agent_models
        .lock()
        .map_err(to_str_err)?
        .insert(cache_key, entries.clone());
    // Persist to config.json so the list survives restarts (seeded back
    // into the in-memory cache by AppState::init). Upsert by
    // `(kind, command)` to match the cache keying. Logged-only on failure:
    // the in-memory cache already holds the fresh list this session.
    if let Some(path) = dirs::home_dir().map(|h| h.join(".cadenza").join("config.json")) {
        let mut cfg = state.config.lock().map_err(to_str_err)?;
        let record = crate::models::CachedModels {
            kind: agent_kind,
            command: command.clone(),
            models: entries.clone(),
        };
        let list = cfg.agent_models.get_or_insert_with(Vec::new);
        if let Some(slot) = list
            .iter_mut()
            .find(|c| c.kind == agent_kind && c.command == command)
        {
            *slot = record;
        } else {
            list.push(record);
        }
        if let Err(e) = cfg.save_to(&path) {
            tracing::warn!(error = %e, "failed to persist discovered models to config");
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::{highest_task_number, proposta_to_body, Proposta};

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
            created_at_ms: 0,
        }
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
