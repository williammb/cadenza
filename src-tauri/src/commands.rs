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

/// Typed failure of [`apply_review_decision`]. Carries a stable machine
/// `code` so the IPC handler can map it to an `ErrorBody` and the Tauri
/// command can stringify it, without duplicating the transition guard.
pub(crate) struct ReviewDecisionError {
    /// Stable code: `bad_args`, `bad_state`, `task_not_found`, `internal`.
    pub code: &'static str,
    pub message: String,
}

impl ReviewDecisionError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Shared core of the human approve / request-changes transition
/// (PLAN §E.16). Both the NDJSON `review_decision_op` (CLI/agent surface)
/// and the `review_decision` Tauri command (webview surface) call this so
/// the transition guard lives in exactly one place.
///
/// Guard: `estado == aguardando_revisao` AND a latest non-terminal
/// (`Pending`) review package exists; otherwise `bad_state`. On success the
/// package decision mark, the `[revisão]` log line (both verdicts), and the
/// estado flip are applied in order decision → log → estado (each
/// idempotent, so a crash leaves a recoverable, never silently-finished
/// state) and the new [`Estado`] is returned.
pub(crate) async fn apply_review_decision(
    repo: &dyn Repository,
    task_id: &str,
    verdict: cadenza_proto::ops::review_decision::Verdict,
    note: &str,
) -> Result<Estado, ReviewDecisionError> {
    use crate::store::PackageStatus;
    use cadenza_proto::ops::review_decision::Verdict;

    crate::store::validate_id(task_id)
        .map_err(|e| ReviewDecisionError::new("bad_args", e.to_string()))?;

    // Guard: task must be awaiting review.
    let task = repo
        .read_task(task_id)
        .await
        .map_err(map_decision_store_err)?;
    if task.estado != Estado::AguardandoRevisao {
        return Err(ReviewDecisionError::new(
            "bad_state",
            "task is not awaiting review (estado != aguardando_revisao)",
        ));
    }

    // Guard: a latest, undecided (Pending) package must exist.
    let latest = repo
        .latest_review_package(task_id)
        .await
        .map_err(map_decision_store_err)?;
    let Some(latest) = latest.filter(|p| p.status == PackageStatus::Pending) else {
        return Err(ReviewDecisionError::new(
            "bad_state",
            "no pending review package to decide",
        ));
    };

    let (target_estado, decision, label) = match verdict {
        Verdict::Aprovado => (Estado::Feito, PackageStatus::Aprovado, "aprovado"),
        Verdict::PedirAlteracoes => (
            Estado::Fazendo,
            PackageStatus::AlteracoesSolicitadas,
            "pedir_alteracoes",
        ),
    };
    let log_line = if note.trim().is_empty() {
        format!("[revisão] {label}")
    } else {
        format!("[revisão] {label}: {note}")
    };

    // decision → log → estado: each idempotent; the order guarantees a
    // crash never leaves the task `feito`/`fazendo` while the package still
    // looks undecided.
    repo.set_package_decision(task_id, latest.attempt, decision)
        .await
        .map_err(map_decision_store_err)?;
    repo.append_log(task_id, &log_line)
        .await
        .map_err(map_decision_store_err)?;
    repo.set_estado(task_id, target_estado)
        .await
        .map_err(map_decision_store_err)?;

    Ok(target_estado)
}

fn map_decision_store_err(e: StoreError) -> ReviewDecisionError {
    match e {
        StoreError::NotFound(id) => ReviewDecisionError::new("task_not_found", id),
        StoreError::Busy => ReviewDecisionError::new("task_busy", e.to_string()),
        other => ReviewDecisionError::new("internal", other.to_string()),
    }
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

// ─────────────────── jira analysis runs (Slice 2) ───────────────────

use crate::jira_run::{self, RunSecret, RunSecretError, VerifiedRun};
use cadenza_proto::{ops as proto_ops, SecretStatus};

/// Epoch-ms now. Local helper (no shared `now_ms` in this module).
fn now_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Mint an analysis run: generate `analysis_run_id` + capability secret,
/// then upsert the issue record with `secret_hash` + expiry + status=Active.
/// Returns the plaintext secret EXACTLY ONCE (the caller surfaces it to the
/// operator and drops it). The plaintext is never persisted.
///
/// Requires an existing `JiraIssueRecord` for `(jira_site, jira_issue_id)`
/// (so `jira_key` and friends are preserved); absence is an error. Later
/// slices that own the import path will seed the record first.
// Minted from tests in Slice 2 and from the production import orchestration
// (`jira_import_persist`) in Slice 6a.
pub(crate) async fn create_analysis_run(
    state: &AppState,
    jira_site: &str,
    jira_issue_id: &str,
    project_id: Option<&str>,
) -> Result<(String, RunSecret), String> {
    let mut record = state
        .repo
        .read_jira_issue(jira_site, jira_issue_id)
        .await
        .map_err(to_str_err)?
        .ok_or_else(|| format!("no jira issue record for {jira_site}/{jira_issue_id}"))?;

    let analysis_run_id = format!("run-{}", Uuid::new_v4().simple());
    let secret = jira_run::generate_secret();
    let now = now_ms_i64();

    record.analysis_run_id = Some(analysis_run_id.clone());
    record.secret_hash = Some(jira_run::hash_secret(secret.expose()));
    record.secret_expiry_ms = Some(now + jira_run::RUN_SECRET_TTL_MS);
    record.secret_status = Some(SecretStatus::Active.as_str().to_string());
    if let Some(pid) = project_id {
        record.project_id = Some(pid.to_string());
    }
    record.updated_at_ms = now;

    state
        .repo
        .upsert_jira_issue(&record)
        .await
        .map_err(to_str_err)?;

    Ok((analysis_run_id, secret))
}

/// Resolve `analysis_run_id` → record by scanning `list_jira_issues`
/// (no secondary index in Slice 2; acceptable at desktop scale), then
/// verify status Active + not expired + hash match (constant-time).
pub(crate) async fn verify_run_secret(
    state: &AppState,
    analysis_run_id: &str,
    presented_secret: &str,
) -> Result<VerifiedRun, RunSecretError> {
    let records = state
        .repo
        .list_jira_issues()
        .await
        .map_err(|_| RunSecretError::NotFound)?;
    let record = records
        .into_iter()
        .find(|r| r.analysis_run_id.as_deref() == Some(analysis_run_id))
        .ok_or(RunSecretError::NotFound)?;

    let stored_hash = record
        .secret_hash
        .as_deref()
        .ok_or(RunSecretError::NotFound)?;

    // Status gate first (revoked is a definitive no), then expiry, then hash.
    match record
        .secret_status
        .as_deref()
        .and_then(SecretStatus::parse)
    {
        Some(SecretStatus::Revoked) => return Err(RunSecretError::Revoked),
        Some(SecretStatus::Expired) => return Err(RunSecretError::Expired),
        _ => {}
    }
    if let Some(expiry) = record.secret_expiry_ms {
        if now_ms_i64() > expiry {
            return Err(RunSecretError::Expired);
        }
    }
    let presented_hash = jira_run::hash_secret(presented_secret);
    if !jira_run::secret_hash_eq(stored_hash, &presented_hash) {
        return Err(RunSecretError::Invalid);
    }
    Ok(VerifiedRun {
        jira_site: record.jira_site,
        jira_issue_id: record.jira_issue_id,
        project_id: record.project_id,
    })
}

/// Set `secret_status=Revoked` via upsert. Idempotent: no-op if the record
/// is already revoked (or absent).
pub(crate) async fn revoke_run_secret(
    state: &AppState,
    analysis_run_id: &str,
) -> Result<(), String> {
    let records = state.repo.list_jira_issues().await.map_err(to_str_err)?;
    let Some(mut record) = records
        .into_iter()
        .find(|r| r.analysis_run_id.as_deref() == Some(analysis_run_id))
    else {
        return Ok(());
    };
    if record.secret_status.as_deref() == Some(SecretStatus::Revoked.as_str()) {
        return Ok(());
    }
    record.secret_status = Some(SecretStatus::Revoked.as_str().to_string());
    record.updated_at_ms = now_ms_i64();
    state
        .repo
        .upsert_jira_issue(&record)
        .await
        .map_err(to_str_err)?;
    Ok(())
}

/// Failure surface for `jira_materialize_core`. Carries enough to map to the
/// right wire `ErrorBody.code` (IPC) or `String` (Tauri command).
#[derive(Debug)]
pub(crate) enum MaterializeError {
    Secret(RunSecretError),
    Decomposition(jira_run::DecompError),
    Internal(String),
}

impl MaterializeError {
    /// `(code, message)` for an `ErrorBody`.
    pub(crate) fn code_message(&self) -> (&'static str, String) {
        match self {
            MaterializeError::Secret(RunSecretError::NotFound)
            | MaterializeError::Secret(RunSecretError::Invalid) => (
                "run_secret_invalid",
                "analysis run secret is unknown or invalid".to_string(),
            ),
            MaterializeError::Secret(RunSecretError::Expired) => (
                "run_secret_expired",
                "analysis run secret has expired".to_string(),
            ),
            MaterializeError::Secret(RunSecretError::Revoked) => (
                "run_secret_revoked",
                "analysis run secret has been revoked".to_string(),
            ),
            MaterializeError::Decomposition(e) => ("invalid_decomposition", e.reason()),
            MaterializeError::Internal(m) => ("internal", m.clone()),
        }
    }
}

impl std::fmt::Display for MaterializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (code, msg) = self.code_message();
        write!(f, "[{code}] {msg}")
    }
}

/// Shared materialize logic behind both the IPC op and the Tauri command.
///
/// Verifies the capability secret, validates the decomposition, then creates
/// one proposal per subtask with the Jira identity stamped SERVER-SIDE from
/// the verified run (never from the wire args) and a deterministic,
/// app-owned idempotency key `"jira:<site>:<issue>:<run_id>:<index>"`. Re-running while the
/// secret is still active is idempotent (same keys ⇒ `propose` dedup). On
/// success the secret is revoked (best-effort; revoke failure is logged in
/// English, not fatal — the tasks already exist).
pub(crate) async fn jira_materialize_core(
    state: &AppState,
    args: &proto_ops::jira_materialize::Args,
) -> Result<proto_ops::jira_materialize::Result, MaterializeError> {
    // 1. Authorize. Do NOT log `args` — it carries the secret.
    let verified = verify_run_secret(state, &args.analysis_run_id, &args.run_secret)
        .await
        .map_err(MaterializeError::Secret)?;

    // 2. Validate payload.
    jira_run::validate_decomposition(&args.subtasks).map_err(MaterializeError::Decomposition)?;

    // 3. Create one proposal per subtask, identity stamped from `verified`.
    let mut created = Vec::with_capacity(args.subtasks.len());
    for (index, subtask) in args.subtasks.iter().enumerate() {
        // Scope the key to the analysis run, not just (site, issue, index):
        // re-running the SAME run dedups (same run_id + index), but a NEW run
        // for the same issue (e.g. after discard + re-import + re-analysis)
        // mints fresh proposals instead of colliding with the prior run's.
        let idempotency_key = format!(
            "jira:{}:{}:{}:{}",
            verified.jira_site, verified.jira_issue_id, args.analysis_run_id, index
        );
        let np = NewProposta {
            idempotency_key: idempotency_key.clone(),
            parent: None,
            title: subtask.title.clone(),
            repro: subtask.body.clone(),
            file: String::new(),
            what_failed: String::new(),
            action: String::new(),
            jira_site: Some(verified.jira_site.clone()),
            jira_issue_id: Some(verified.jira_issue_id.clone()),
        };
        let proposta = state
            .repo
            .propose(np)
            .await
            .map_err(|e| MaterializeError::Internal(e.to_string()))?;
        created.push(proto_ops::jira_materialize::MaterializedTask {
            proposta_id: proposta.proposta_id,
            idempotency_key,
            subtask_index: index as u32,
        });
    }

    // 4. Revoke the now-spent secret (best-effort).
    if let Err(e) = revoke_run_secret(state, &args.analysis_run_id).await {
        tracing::warn!(error = %e, "failed to revoke analysis run secret after materialize");
    }

    Ok(proto_ops::jira_materialize::Result {
        jira_site: verified.jira_site,
        jira_issue_id: verified.jira_issue_id,
        created,
    })
}

/// Tauri-command surface for `jira_materialize` (in-app/test parity with the
/// IPC op). Delegates to [`jira_materialize_core`].
#[tauri::command]
pub async fn jira_materialize(
    state: State<'_, Arc<AppState>>,
    args: proto_ops::jira_materialize::Args,
) -> Result<proto_ops::jira_materialize::Result, String> {
    jira_materialize_core(&state, &args)
        .await
        .map_err(|e| e.to_string())
}

// ───────────────────────── Jira data layer (Slice 3) ─────────────────────────

/// Clone `config.jira` out of the lock, dropping the guard before any
/// `.await` (we never hold the sync Mutex across an await — commands.rs
/// state-doc rule). Returns a `JiraError::Config` if Jira is not configured.
fn jira_config_snapshot(
    state: &AppState,
) -> Result<crate::config::JiraConfig, crate::jira::JiraError> {
    let cfg = state
        .config
        .lock()
        .map_err(|e| crate::jira::JiraError::Config(format!("config lock poisoned: {e}")))?;
    cfg.jira
        .clone()
        .ok_or_else(|| crate::jira::JiraError::Config("Jira is not configured".to_string()))
    // guard drops here, before the caller awaits
}

/// Shared `jira_test_connection` logic behind the Tauri command and IPC op.
/// Fetches `/myself`; returns data only (no persistence).
pub(crate) async fn jira_test_connection_core(
    state: &AppState,
) -> Result<proto_ops::jira_test_connection::Result, crate::jira::JiraError> {
    let cfg = jira_config_snapshot(state)?;
    let client = crate::jira::JiraClient::from_config(&cfg)?;
    let cancel = crate::jira::CancelToken::new();
    let me = client.test_connection(&cancel).await?;
    Ok(proto_ops::jira_test_connection::Result {
        account_id: me.account_id,
        display_name: me.display_name,
    })
}

/// Shared `jira_fetch_issue` logic. Fetches+parses one issue; returns data
/// only (does NOT persist a `JiraIssueRecord`).
pub(crate) async fn jira_fetch_issue_core(
    state: &AppState,
    args: &proto_ops::jira_fetch_issue::Args,
) -> Result<proto_ops::jira_fetch_issue::Result, crate::jira::JiraError> {
    let key = args.key.trim();
    if key.is_empty() {
        return Err(crate::jira::JiraError::Config(
            "issue key is required".to_string(),
        ));
    }
    let cfg = jira_config_snapshot(state)?;
    let client = crate::jira::JiraClient::from_config(&cfg)?;
    let cancel = crate::jira::CancelToken::new();
    let issue = client.fetch_issue(key, &cancel).await?;
    Ok(proto_ops::jira_fetch_issue::Result {
        jira_issue_id: issue.jira_issue_id,
        jira_key: issue.jira_key,
        summary: issue.summary,
        description_markdown: issue.description_markdown,
        raw_adf: issue.raw_adf,
    })
}

/// Shared `jira_list_assigned` logic. Lists the caller's open issues with a
/// page cap; returns data only.
pub(crate) async fn jira_list_assigned_core(
    state: &AppState,
) -> Result<proto_ops::jira_list_assigned::Result, crate::jira::JiraError> {
    let cfg = jira_config_snapshot(state)?;
    let client = crate::jira::JiraClient::from_config(&cfg)?;
    let cancel = crate::jira::CancelToken::new();
    let res = client.list_assigned(&cancel).await?;
    Ok(proto_ops::jira_list_assigned::Result {
        issues: res
            .issues
            .into_iter()
            .map(|i| proto_ops::jira_list_assigned::Issue {
                key: i.key,
                id: i.id,
                summary: i.summary,
            })
            .collect(),
        partial: res.partial,
    })
}

#[tauri::command]
pub async fn jira_test_connection(
    state: State<'_, Arc<AppState>>,
) -> Result<proto_ops::jira_test_connection::Result, String> {
    jira_test_connection_core(&state)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn jira_fetch_issue(
    state: State<'_, Arc<AppState>>,
    args: proto_ops::jira_fetch_issue::Args,
) -> Result<proto_ops::jira_fetch_issue::Result, String> {
    jira_fetch_issue_core(&state, &args)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn jira_list_assigned(
    state: State<'_, Arc<AppState>>,
) -> Result<proto_ops::jira_list_assigned::Result, String> {
    jira_list_assigned_core(&state)
        .await
        .map_err(|e| e.to_string())
}

// ───────────────────────── Jira import (Slice 6a) ─────────────────────────

/// Failure surface for the import orchestration. Carries enough to map to the
/// right wire `ErrorBody.code` (IPC) or `String` (Tauri command). The
/// capability secret NEVER appears in any variant.
#[derive(Debug)]
pub(crate) enum ImportError {
    /// Bad usage / misconfiguration (empty issue_ref, bad analyst_kind, Jira
    /// not configured). Maps to `jira_config` (exit 2).
    Config(String),
    /// The target project id is not in config.projects. Maps to
    /// `unknown_project` (exit 30).
    UnknownProject(String),
    /// The fetch leg failed; passthrough of the Jira data-layer error so its
    /// own stable code (`jira_auth`/`jira_not_found`/`jira_http`/…) is kept.
    Fetch(crate::jira::JiraError),
    /// Minting the analysis run / persisting the seed record failed. Maps to
    /// `jira_import_failed` (exit 1).
    Mint(String),
    /// The analyst PTY spawn failed. Maps to `jira_import_failed` (exit 1).
    Spawn(String),
    /// Any other store/internal failure. Maps to `jira_import_failed` (exit 1).
    Internal(String),
}

impl ImportError {
    /// `(wire code, message)` for the IPC `ErrorBody`.
    pub(crate) fn code_message(&self) -> (&'static str, String) {
        match self {
            ImportError::Config(m) => ("jira_config", m.clone()),
            ImportError::UnknownProject(p) => {
                ("unknown_project", format!("unknown project_id: {p}"))
            }
            // Preserve the fetch error's own stable code/message.
            ImportError::Fetch(e) => {
                let (code, msg) = e.code_message();
                (code, msg)
            }
            ImportError::Mint(m) => ("jira_import_failed", m.clone()),
            ImportError::Spawn(m) => ("jira_import_failed", m.clone()),
            ImportError::Internal(m) => ("jira_import_failed", m.clone()),
        }
    }

    /// Build an `ErrorBody` for the IPC surface.
    pub(crate) fn to_error_body(&self) -> cadenza_proto::wire::ErrorBody {
        let (code, message) = self.code_message();
        cadenza_proto::wire::ErrorBody::new(code, message)
    }
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (code, msg) = self.code_message();
        write!(f, "[{code}] {msg}")
    }
}

/// Internal outcome of [`jira_import_persist`]. Holds the `RunSecret`
/// in-memory for the spawn tail ONLY; it is intentionally NOT `Serialize` and
/// NEVER logged so the capability secret cannot leak through the wire. The
/// derived `Debug` is safe — `RunSecret`'s own `Debug` redacts the plaintext.
#[derive(Debug)]
pub(crate) enum ImportPersistOutcome {
    New {
        record: cadenza_proto::JiraIssueRecord,
        analysis_run_id: String,
        secret: RunSecret,
        summary: String,
    },
    ExistingActive {
        record: cadenza_proto::JiraIssueRecord,
    },
}

/// "Active work" predicate for the reimport-idempotency check: an Active
/// analysis run, OR a live worktree (`Ready` with the dir on disk), OR a
/// worktree mid-creation (`creating`). An inactive record (revoked/expired
/// secret, no worktree) falls through to a fresh re-mint.
fn issue_has_active_work(rec: &cadenza_proto::JiraIssueRecord) -> bool {
    let active_secret = rec.secret_status.as_deref() == Some(SecretStatus::Active.as_str());
    let live_worktree = crate::jira::worktree::ready_if_valid(rec).is_some();
    let creating = rec.worktree_state.as_deref()
        == Some(cadenza_proto::jira::WorktreeState::Creating.as_str());
    active_secret || live_worktree || creating
}

/// Steps 1-5 of import, pure & unit-testable: validate project, idempotency
/// check, upsert seed record, mint run+secret. Takes an ALREADY-FETCHED issue
/// so the transport/keyring/network is out of the unit-test path. Returns the
/// new-vs-existing decision plus the minted secret (caller-only; NEVER goes
/// into the proto `Result`, NEVER logged).
pub(crate) async fn jira_import_persist(
    state: &AppState,
    jira_site: &str,
    fetched: &crate::jira::FetchedIssue,
    project_id: &str,
) -> Result<ImportPersistOutcome, ImportError> {
    // 1. Validate project.
    let pid = project_id.trim();
    if pid.is_empty() {
        return Err(ImportError::Config("project_id is required".to_string()));
    }
    {
        let cfg = state
            .config
            .lock()
            .map_err(|e| ImportError::Internal(format!("config lock poisoned: {e}")))?;
        if !cfg.projects.iter().any(|p| p.id == pid) {
            return Err(ImportError::UnknownProject(pid.to_string()));
        }
    }

    // 2. Derive identity.
    let issue_id = fetched.jira_issue_id.as_str();

    // 3. Reimport idempotency: an existing record with active work is reopened
    //    WITHOUT re-minting/spawning. (Note: in the production path the fetch
    //    already happened; the "no second fetch" guarantee for the active case
    //    is enforced by the test-only `jira_import_via` orchestrator, which is
    //    the seam the contract specifies.)
    let existing = state
        .repo
        .read_jira_issue(jira_site, issue_id)
        .await
        .map_err(|e| ImportError::Internal(e.to_string()))?;
    if let Some(rec) = &existing {
        if issue_has_active_work(rec) {
            return Ok(ImportPersistOutcome::ExistingActive {
                record: rec.clone(),
            });
        }
    }

    // 4. Upsert the seed record. Preserve `created_at_ms` when re-using an
    //    existing inactive record; refresh `raw_adf`/`jira_key`/`project_id`.
    let now = now_ms_i64();
    let raw_adf = if fetched.raw_adf.is_null() {
        None
    } else {
        Some(
            serde_json::to_string(&fetched.raw_adf)
                .map_err(|e| ImportError::Internal(format!("serialize raw_adf: {e}")))?,
        )
    };
    let created_at_ms = existing.as_ref().map(|r| r.created_at_ms).unwrap_or(now);
    let record = cadenza_proto::JiraIssueRecord {
        jira_site: jira_site.to_string(),
        jira_issue_id: fetched.jira_issue_id.clone(),
        jira_key: fetched.jira_key.clone(),
        project_id: Some(pid.to_string()),
        analysis_run_id: None,
        secret_hash: None,
        secret_expiry_ms: None,
        secret_status: None,
        raw_adf,
        branch_name: None,
        worktree_path: None,
        base_sha: None,
        worktree_state: None,
        created_at_ms,
        updated_at_ms: now,
    };
    state
        .repo
        .upsert_jira_issue(&record)
        .await
        .map_err(|e| ImportError::Mint(e.to_string()))?;

    // 5. Mint run + secret (stamps secret_hash/expiry/status/project on the
    //    record; re-read so the returned record reflects that).
    let (analysis_run_id, secret) = create_analysis_run(state, jira_site, issue_id, Some(pid))
        .await
        .map_err(ImportError::Mint)?;
    let record = state
        .repo
        .read_jira_issue(jira_site, issue_id)
        .await
        .map_err(|e| ImportError::Internal(e.to_string()))?
        .ok_or_else(|| ImportError::Internal("record vanished after mint".to_string()))?;

    Ok(ImportPersistOutcome::New {
        record,
        analysis_run_id,
        secret,
        summary: fetched.summary.clone(),
    })
}

/// Parse the wire analyst-kind string into an [`AgenteKind`]. Accepts the
/// canonical serde forms (`claude_code`, `codex`, `copilot`, `antigravity`,
/// `opencode`) and the hyphenated CLI alias `claude-code`.
fn parse_analyst_kind(s: &str) -> Result<AgenteKind, ImportError> {
    match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "claude_code" | "claudecode" | "claude" => Ok(AgenteKind::ClaudeCode),
        "codex" => Ok(AgenteKind::Codex),
        "copilot" => Ok(AgenteKind::Copilot),
        "antigravity" | "agy" => Ok(AgenteKind::Antigravity),
        "opencode" => Ok(AgenteKind::OpenCode),
        other => Err(ImportError::Config(format!(
            "unknown analyst_kind: {other}"
        ))),
    }
}

/// Derive the canonical `jira_site` for a record key from configured Jira
/// base_url (origin/host). Mirrors the host-rule guard used by the client.
fn jira_site_from_config(state: &AppState) -> Result<String, ImportError> {
    let cfg = jira_config_snapshot(state).map_err(ImportError::Fetch)?;
    let url = crate::jira::config::validate_base_url(&cfg.base_url)
        .map_err(|e| ImportError::Config(format!("base_url: {e}")))?;
    Ok(url.origin().ascii_serialization())
}

/// Localized initial prompt sent to the analyst when decomposing a Jira issue.
/// MUST NOT contain the capability secret — the agent reads it from
/// `$CADENZA_RUN_SECRET` (injected by `jira_analyst_env`).
fn render_initial_jira_prompt(
    i18n_slot: &Mutex<I18n>,
    jira_key: &str,
    summary: &str,
    issue_id: &str,
) -> String {
    let mut args = FluentArgs::new();
    args.set("jira_key", jira_key.to_string());
    args.set("summary", summary.to_string());
    args.set("issue_id", issue_id.to_string());
    match i18n_slot.lock() {
        Ok(i18n) => i18n.t_with("agent-initial-prompt-jira", Some(&args)),
        Err(_) => format!(
            "Use the `cadenza` skill to decompose Jira issue {jira_key} ({summary}) into subtasks. Read $CADENZA_RUN_SECRET and submit via jira-materialize."
        ),
    }
}

/// Full production import: fetch (real client) -> persist (steps 1-5) ->
/// spawn the analyst (step 6, thin tail). The capability secret reaches the
/// analyst via ENV only and is never logged.
pub(crate) async fn jira_import_core(
    state: &AppState,
    args: &proto_ops::jira_import::Args,
) -> Result<proto_ops::jira_import::Result, ImportError> {
    let issue_ref = args.issue_ref.trim();
    if issue_ref.is_empty() {
        return Err(ImportError::Config("issue_ref is required".to_string()));
    }
    // Parse the analyst kind up front so a bad kind fails before any fetch.
    let kind = parse_analyst_kind(&args.analyst_kind)?;

    let jira_site = jira_site_from_config(state)?;

    // Reimport short-circuit BEFORE any network fetch: if a record for this
    // site already has active work (matched by display key OR durable id),
    // reopen it without a second fetch. This makes "open existing" work
    // offline and survive the issue being renamed/deleted on the Jira side
    // (a post-fetch check would wrongly fail with jira_not_found/jira_http).
    {
        let existing = state
            .repo
            .list_jira_issues()
            .await
            .map_err(|e| ImportError::Internal(e.to_string()))?
            .into_iter()
            .find(|r| {
                r.jira_site == jira_site
                    && (r.jira_key == issue_ref || r.jira_issue_id == issue_ref)
                    && issue_has_active_work(r)
            });
        if let Some(record) = existing {
            return Ok(proto_ops::jira_import::Result::ExistingActive {
                jira_site: record.jira_site,
                jira_issue_id: record.jira_issue_id,
                jira_key: record.jira_key,
                project_id: record.project_id,
                analysis_run_id: record.analysis_run_id,
            });
        }
    }

    // Fetch (real client) — keeps the transport/keyring/network leg here, out
    // of the unit-test path (which drives `jira_import_persist` directly).
    let fetch_args = proto_ops::jira_fetch_issue::Args {
        key: issue_ref.to_string(),
    };
    let fetched = {
        let r = jira_fetch_issue_core(state, &fetch_args)
            .await
            .map_err(ImportError::Fetch)?;
        crate::jira::FetchedIssue {
            jira_issue_id: r.jira_issue_id,
            jira_key: r.jira_key,
            summary: r.summary,
            description_markdown: r.description_markdown,
            raw_adf: r.raw_adf,
        }
    };

    match jira_import_persist(state, &jira_site, &fetched, &args.project_id).await? {
        ImportPersistOutcome::ExistingActive { record } => {
            Ok(proto_ops::jira_import::Result::ExistingActive {
                jira_site: record.jira_site,
                jira_issue_id: record.jira_issue_id,
                jira_key: record.jira_key,
                project_id: record.project_id,
                analysis_run_id: record.analysis_run_id,
            })
        }
        ImportPersistOutcome::New {
            record,
            analysis_run_id,
            secret,
            summary,
        } => {
            // Step 6 — analyst spawn (thin tail, mirrors `destrinchar_ideia`).
            let pid = record
                .project_id
                .clone()
                .ok_or_else(|| ImportError::Internal("record missing project_id".to_string()))?;
            let (cwd, command_override) = {
                let cfg = state
                    .config
                    .lock()
                    .map_err(|e| ImportError::Internal(format!("config lock poisoned: {e}")))?;
                let project = cfg
                    .projects
                    .iter()
                    .find(|p| p.id == pid)
                    .ok_or_else(|| ImportError::UnknownProject(pid.clone()))?;
                let cmd = project
                    .agente
                    .as_ref()
                    .filter(|a| a.kind == kind)
                    .and_then(|a| a.command.clone())
                    .or_else(|| {
                        cfg.agente
                            .as_ref()
                            .filter(|a| a.kind == kind)
                            .and_then(|a| a.command.clone())
                    });
                (project.path.clone(), cmd)
            };
            if !cwd.exists() {
                return Err(ImportError::Spawn(format!(
                    "project path does not exist: {} — fix it in Settings → Projetos",
                    cwd.display()
                )));
            }

            // jira_site is a full origin ("https://acme.atlassian.net"); strip
            // the scheme and sanitize so the synthetic id (exported as
            // TASKAI_TASK_ID) is safe to use verbatim in paths/argv.
            let host = jira_site.rsplit("://").next().unwrap_or(jira_site.as_str());
            let site_token: String = host
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let synthetic_task_id = format!("JIRA-{}-{}", site_token, fetched.jira_issue_id);
            let prompt = render_initial_jira_prompt(
                &state.i18n,
                &fetched.jira_key,
                &summary,
                &fetched.jira_issue_id,
            );
            let model = String::new();
            let plan: LaunchPlan = agent::plan_launch(
                kind,
                &model,
                command_override.as_deref(),
                &cwd,
                &synthetic_task_id,
                &pid,
                None,
                Some(&prompt),
            );
            let LaunchPlan {
                spawn,
                conversation_id_known: _,
                pending_codex_capture,
                pending_opencode_capture: _,
                prompt_delivery,
            } = plan;
            // The capability secret reaches the analyst here via ENV ONLY.
            let spawn = spawn.jira_analyst_env(
                &analysis_run_id,
                secret.expose(),
                &jira_site,
                &fetched.jira_issue_id,
                &fetched.jira_key,
            );

            let pty = PtyHandle::spawn(spawn).map_err(|e| ImportError::Spawn(e.to_string()))?;
            let session_id = format!("S-{}", Uuid::new_v4().simple());
            let session = TerminalSession::start(session_id.clone(), pty)
                .map_err(|e| ImportError::Spawn(e.to_string()))?;
            state
                .sessions
                .lock()
                .map_err(|e| ImportError::Internal(e.to_string()))?
                .insert(session_id.clone(), session.clone());
            // Log identity only — NEVER the secret.
            tracing::info!(
                analysis_run_id = %analysis_run_id,
                jira_key = %fetched.jira_key,
                session = %session_id,
                "jira analyst started"
            );

            if prompt_delivery == PromptDelivery::TypeIn {
                let session_for_prompt = session.clone();
                tauri::async_runtime::spawn(async move {
                    send_initial_prompt(&session_for_prompt, &prompt).await;
                });
            }
            if let Some(capture) = pending_codex_capture {
                tauri::async_runtime::spawn(async move {
                    let _ = wait_for_codex_uuid(capture).await;
                });
            }

            Ok(proto_ops::jira_import::Result::Imported {
                jira_site,
                jira_issue_id: fetched.jira_issue_id,
                jira_key: fetched.jira_key,
                summary,
                project_id: pid,
                analysis_run_id,
                session_id,
            })
        }
    }
}

#[tauri::command]
pub async fn jira_import(
    state: State<'_, Arc<AppState>>,
    args: proto_ops::jira_import::Args,
) -> Result<proto_ops::jira_import::Result, String> {
    jira_import_core(&state, &args)
        .await
        .map_err(|e| e.to_string())
}

// ───────────────────────── Jira discard (Slice 6a) ─────────────────────────

/// Failure surface for the discard lifecycle.
#[derive(Debug)]
pub(crate) enum DiscardError {
    /// No record for `(jira_site, jira_issue_id)`. Maps to `jira_not_found`
    /// (exit 30).
    NotFound,
    /// A subtask agent is live for this issue. Maps to `jira_worktree_busy`
    /// (exit 1).
    Busy,
    /// The worktree has uncommitted/untracked changes and `force` was not
    /// set. Carries the COUNT only — never file names. Maps to
    /// `jira_worktree_dirty` (exit 1).
    WorktreeDirty { changed_files: u32 },
    /// `git worktree remove` failed. Maps to `jira_worktree_failed` (exit 1).
    RemoveFailed(String),
    /// Any other store/internal failure. Maps to `jira_worktree_failed`
    /// (exit 1).
    Internal(String),
}

impl DiscardError {
    pub(crate) fn code_message(&self) -> (&'static str, String) {
        match self {
            DiscardError::NotFound => (
                "jira_not_found",
                "no jira issue record to discard".to_string(),
            ),
            DiscardError::Busy => (
                "jira_worktree_busy",
                "a subtask agent is still running for this Jira issue".to_string(),
            ),
            DiscardError::WorktreeDirty { changed_files } => (
                "jira_worktree_dirty",
                format!(
                    "worktree has {changed_files} uncommitted/untracked change(s); pass --force to discard"
                ),
            ),
            DiscardError::RemoveFailed(m) => ("jira_worktree_failed", m.clone()),
            DiscardError::Internal(m) => ("jira_worktree_failed", m.clone()),
        }
    }

    pub(crate) fn to_error_body(&self) -> cadenza_proto::wire::ErrorBody {
        let (code, message) = self.code_message();
        cadenza_proto::wire::ErrorBody::new(code, message)
    }
}

impl std::fmt::Display for DiscardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (code, msg) = self.code_message();
        write!(f, "[{code}] {msg}")
    }
}

/// Discard an imported Jira issue: refuse a dirty worktree unless forced,
/// remove the worktree, revoke the run secret, delete the record, and forget
/// subtask sidecars. RETAINS the branch, the produced subtask Tasks, and any
/// aggregate review packages (audit trail). Keyed by `(site, issue_id)`; the
/// `delete_task` path never calls this.
pub(crate) async fn jira_discard_core(
    state: &AppState,
    args: &proto_ops::jira_discard::Args,
) -> Result<proto_ops::jira_discard::Result, DiscardError> {
    let site = args.jira_site.as_str();
    let issue = args.jira_issue_id.as_str();

    // 1. Read record.
    let record = state
        .repo
        .read_jira_issue(site, issue)
        .await
        .map_err(|e| DiscardError::Internal(e.to_string()))?
        .ok_or(DiscardError::NotFound)?;

    // 2. Busy check — refuse if a subtask agent is live for this issue.
    {
        let active = state
            .jira_active_executors
            .lock()
            .map_err(|e| DiscardError::Internal(e.to_string()))?;
        let sessions = state
            .sessions
            .lock()
            .map_err(|e| DiscardError::Internal(e.to_string()))?;
        let key = (site.to_string(), issue.to_string());
        if crate::jira::worktree::issue_executor_busy(&active, &sessions, &key) {
            return Err(DiscardError::Busy);
        }
    }

    // 3. Dirty check + 4. remove worktree.
    let mut worktree_removed = false;
    if let Some(wt) = record.worktree_path.as_deref() {
        let wt_path = Path::new(wt);
        if wt_path.exists() {
            let dirty = crate::git::worktree_dirty_files(wt_path)
                .await
                .map_err(|e| DiscardError::RemoveFailed(e.to_string()))?;
            if !dirty.is_empty() && !args.force {
                // Count only — never the file names (no sensitive paths on the
                // wire). The caller learns work would be lost.
                return Err(DiscardError::WorktreeDirty {
                    changed_files: dirty.len() as u32,
                });
            }
            // Resolve the repo path from the record's project_id.
            let repo = record
                .project_id
                .as_deref()
                .and_then(|pid| {
                    state.config.lock().ok().and_then(|cfg| {
                        cfg.projects
                            .iter()
                            .find(|p| p.id == pid)
                            .map(|p| p.path.clone())
                    })
                })
                .ok_or_else(|| {
                    DiscardError::RemoveFailed(
                        "cannot resolve repo path for worktree removal".to_string(),
                    )
                })?;
            crate::git::remove_worktree(&repo, wt_path, args.force)
                .await
                .map_err(|e| DiscardError::RemoveFailed(e.to_string()))?;
            if let Err(e) = crate::git::worktree_prune(&repo).await {
                tracing::warn!(error = %e, "worktree_prune after discard failed (advisory)");
            }
            worktree_removed = true;
        }
    }

    // 5. Revoke the run secret (idempotent, best-effort).
    if let Some(run_id) = record.analysis_run_id.as_deref() {
        if let Err(e) = revoke_run_secret(state, run_id).await {
            tracing::warn!(error = %e, "failed to revoke run secret during discard");
        }
    }

    // 6. Delete the record (drops raw_adf + secret columns with the row).
    state
        .repo
        .delete_jira_issue(site, issue)
        .await
        .map_err(|e| DiscardError::Internal(e.to_string()))?;

    // 7. Cascade sidecars (best-effort, warn-on-err). Enumerate subtask task
    //    ids bound to this issue from the task store (no reverse index on
    //    TaskWorktrees), then forget each task_worktrees entry.
    let mut forgotten_task_worktrees = 0u32;
    match state.repo.list_tasks(None).await {
        Ok(tasks) => {
            for task in tasks {
                let enriched = state.task_jira.enrich(task);
                let belongs = enriched.jira_site.as_deref() == Some(site)
                    && enriched.jira_issue_id.as_deref() == Some(issue);
                if belongs && state.task_worktrees.get(&enriched.id).is_some() {
                    if let Err(e) = state.task_worktrees.forget(&enriched.id) {
                        tracing::warn!(error = ?e, task = %enriched.id, "task_worktrees.forget during discard failed");
                    } else {
                        forgotten_task_worktrees += 1;
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = ?e, "list_tasks during discard cascade failed");
        }
    }

    // Drop the in-process lock + executor slot for this issue.
    if let Ok(mut locks) = state.jira_worktree_locks.lock() {
        locks.remove(&(site.to_string(), issue.to_string()));
    }
    if let Ok(mut active) = state.jira_active_executors.lock() {
        active.remove(&(site.to_string(), issue.to_string()));
    }

    Ok(proto_ops::jira_discard::Result {
        jira_site: site.to_string(),
        jira_issue_id: issue.to_string(),
        worktree_removed,
        forgotten_task_worktrees,
    })
}

#[tauri::command]
pub async fn jira_discard(
    state: State<'_, Arc<AppState>>,
    args: proto_ops::jira_discard::Args,
) -> Result<proto_ops::jira_discard::Result, String> {
    jira_discard_core(&state, &args)
        .await
        .map_err(|e| e.to_string())
}

/// Deterministic content key for an aggregate review attempt: a repeat build on
/// the SAME branch state (same `base_sha`/`head_sha`) dedups to a no-op, while
/// a new branch HEAD yields a new attempt. Hashed so the raw site/issue is
/// never a path/key component on the file backend.
fn issue_review_idempotency_key(
    site: &str,
    issue_id: &str,
    base_sha: &str,
    head: Option<&str>,
) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(site.as_bytes());
    h.update([0]);
    h.update(issue_id.as_bytes());
    h.update([0]);
    h.update(base_sha.as_bytes());
    h.update([0]);
    h.update(head.unwrap_or("").as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Shared `jira_review` logic (Slice 5): build the aggregate (issue-owned)
/// branch-diff review, stamp the deterministic idempotency key, and persist it.
///
/// STATE-NEUTRAL: this builds the committed branch diff via the hardened,
/// read-only git layer and persists ONLY the aggregate package — it NEVER
/// calls `set_estado`/`done`/`apply_review_decision`, never appends a task log,
/// and never reads or mutates any subtask estado.
pub(crate) async fn jira_review_core(
    state: &AppState,
    jira_site: &str,
    jira_issue_id: &str,
) -> Result<crate::store::IssueReviewPackage, crate::review::issue::IssueReviewError> {
    use crate::review::issue::IssueReviewError;
    let mut pkg =
        crate::review::issue::build_issue_review(state.repo.as_ref(), jira_site, jira_issue_id)
            .await?;
    pkg.idempotency_key = issue_review_idempotency_key(
        jira_site,
        jira_issue_id,
        &pkg.base_sha,
        pkg.head_sha.as_deref(),
    );
    let stored = state
        .repo
        .upsert_issue_review_package(&pkg)
        .await
        .map_err(|e| IssueReviewError::DiffFailed(e.to_string()))?;
    Ok(stored)
}

#[tauri::command]
pub async fn jira_review(
    state: State<'_, Arc<AppState>>,
    jira_site: String,
    jira_issue_id: String,
) -> Result<crate::store::IssueReviewPackage, String> {
    jira_review_core(&state, &jira_site, &jira_issue_id)
        .await
        .map_err(|e| e.to_string())
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
    // Slice 4: clear the one-executor-per-issue registry for whichever issue
    // (if any) this session was the active executor of, so a follow-up task
    // for the same issue can start immediately.
    if let Ok(mut active) = state.jira_active_executors.lock() {
        // Drop only the `Live` slot for this session; leave any `Reserving`
        // slot (owned by an in-flight start) untouched.
        active.retain(
            |_, slot| !matches!(slot, crate::jira::worktree::ExecutorSlot::Live(sid) if sid == &session_id),
        );
    }
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

/// Bytes that resync a lagged subscriber: home the cursor, clear the
/// viewport (`2J`) and xterm's scrollback (`3J`), then replay the current
/// authoritative ring. The clear avoids doubling content the viewer
/// already rendered before the lag.
fn resync_payload(snapshot: Vec<u8>) -> Vec<u8> {
    const CLEAR: &[u8] = b"\x1b[H\x1b[2J\x1b[3J";
    let mut payload = Vec::with_capacity(snapshot.len() + CLEAR.len());
    payload.extend_from_slice(CLEAR);
    payload.extend_from_slice(&snapshot);
    payload
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

    // Cloned for the resync path below — the forwarding loop needs to
    // re-snapshot the ring after a broadcast lag.
    let session_for_resync = session.clone();
    let handle = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(bytes) => {
                    if channel.send(bytes).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // The subscriber fell behind and the broadcast dropped
                    // `n` chunks. Resuming from the next live chunk would
                    // leave a hole mid-stream and corrupt xterm's escape-
                    // sequence state. Resync instead: atomically grab a
                    // fresh snapshot + receiver (so no chunk is lost or
                    // doubled across the swap), clear the viewport +
                    // scrollback, and replay the current authoritative
                    // ring. Best-effort — a backend terminal emulator
                    // would resync without the visible reset.
                    tracing::warn!(lagged = n, "pty broadcast lagged; resyncing from ring");
                    let (resync_snap, fresh_rx) = session_for_resync.subscribe_with_snapshot();
                    rx = fresh_rx;
                    if channel.send(resync_payload(resync_snap)).is_err() {
                        break;
                    }
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

/// Persist the Jira API token to the OS keyring, keyed on the site
/// `base_url`. The token never touches `config.json`. Idempotent —
/// overwrites an existing entry.
#[tauri::command]
pub fn set_jira_token(base_url: String, token: String) -> Result<(), String> {
    secrets::set_jira_token(&base_url, &token).map_err(to_str_err)
}

/// Remove the Jira API token from the keyring. Returns Ok even if the
/// entry didn't exist (idempotent — matches `delete_password`).
#[tauri::command]
pub fn clear_jira_token(base_url: String) -> Result<(), String> {
    secrets::delete_jira_token(&base_url).map_err(to_str_err)
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

/// RAII reservation for the one-executor-per-issue slot. Created (as
/// `Reserving`) at guard time; `commit(session_id)` flips it to `Live` once
/// the session exists. If dropped without `commit` — any early return or
/// panic during the async spawn window — the still-`Reserving` slot is
/// removed so the issue is never wedged. A `Live` slot is never clobbered.
struct ExecutorReservation {
    state: Arc<AppState>,
    key: (String, String),
    committed: bool,
}

impl ExecutorReservation {
    fn new(state: Arc<AppState>, key: (String, String)) -> Self {
        Self {
            state,
            key,
            committed: false,
        }
    }

    fn commit(mut self, session_id: String) {
        if let Ok(mut active) = self.state.jira_active_executors.lock() {
            active.insert(
                self.key.clone(),
                crate::jira::worktree::ExecutorSlot::Live(session_id),
            );
        }
        self.committed = true;
    }
}

impl Drop for ExecutorReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Ok(mut active) = self.state.jira_active_executors.lock() {
            // Only clear our own `Reserving` slot — never a `Live` one.
            if matches!(
                active.get(&self.key),
                Some(crate::jira::worktree::ExecutorSlot::Reserving)
            ) {
                active.remove(&self.key);
            }
        }
    }
}

/// Launch the configured agent CLI in a PTY
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
    // Absent/null from older callers -> false.
    auto_mode: Option<bool>,
) -> Result<StartTaskAgentResult, String> {
    let mode = mode.unwrap_or_default();
    let auto_mode = auto_mode.unwrap_or(false);
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
    if mode == TaskAgentMode::Execute {
        ensure_task_unblocked(&state, &task_id).await?;
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
            .filter(|a| a.kind == agent_kind)
            .and_then(|a| a.command.clone())
            .or_else(|| {
                cfg.agente
                    .as_ref()
                    .filter(|a| a.kind == agent_kind)
                    .and_then(|a| a.command.clone())
            });
        (project_path, cmd)
    };

    if !project_path.exists() {
        return Err(format!(
            "project path does not exist: {} — fix it in Settings → Projetos",
            project_path.display()
        ));
    }

    // 2b. One-executor-per-issue guard (Slice 4). Only execution runs
    //     contend for the issue's shared worktree; planning is exempt. The
    //     guard refuses a second start while another task for the same Jira
    //     issue has a LIVE agent (registry entry whose session is still in
    //     `state.sessions`); a stale entry (session gone after kill/exit)
    //     is reaped here and the start proceeds. Checked BEFORE any session
    //     is created and before the `Fazendo` transition, so a refusal
    //     leaves no side effects.
    // Held from the guard until just after the session is created, so the
    // slot is claimed atomically (closing the check-then-act TOCTOU); if any
    // step below early-returns, the reservation's Drop clears the slot.
    let mut executor_reservation: Option<ExecutorReservation> = None;
    if mode == TaskAgentMode::Execute {
        if let (Some(site), Some(issue)) = (&task.jira_site, &task.jira_issue_id) {
            let key = (site.clone(), issue.clone());
            {
                let mut active = state.jira_active_executors.lock().map_err(to_str_err)?;
                let sessions = state.sessions.lock().map_err(to_str_err)?;
                if crate::jira::worktree::issue_executor_busy(&active, &sessions, &key) {
                    return Err(
                        "jira_worktree_busy: another task for this Jira issue already has a running agent"
                            .to_string(),
                    );
                }
                // Not busy (no entry, or a stale Live whose session is gone)
                // → claim the slot as Reserving under the held lock so a
                // concurrent start is refused during our async spawn window.
                active.insert(key.clone(), crate::jira::worktree::ExecutorSlot::Reserving);
            }
            executor_reservation = Some(ExecutorReservation::new(Arc::clone(state.inner()), key));
        }
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
        let mut prompt = render_initial_task_prompt(&state.i18n, &task_id, &task_titulo, mode);
        // Inject the project's curated memory on a fresh execution start so
        // the agent knows the project's durable facts/decisions/conventions.
        // Omitted when empty (no block) and for planning runs.
        if mode == TaskAgentMode::Execute {
            match state.repo.list_memory(&project_id).await {
                Ok(items) if !items.is_empty() => {
                    prompt.push_str(&render_memory_block(&state.i18n, &items));
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = ?e, project = %project_id, "load project memory for prompt failed")
                }
            }
        }
        Some(prompt)
    };
    let plan: LaunchPlan = agent::plan_launch_with_options(
        agent_kind,
        &model,
        command_override.as_deref(),
        &cwd,
        &task_id,
        &project_id,
        existing_conv_id.as_deref(),
        initial_prompt.as_deref(),
        agent::LaunchOptions { auto_mode },
    )?;
    let LaunchPlan {
        spawn,
        conversation_id_known,
        pending_codex_capture,
        mut pending_opencode_capture,
        prompt_delivery,
    } = plan;

    // 4a. OpenCode needs a snapshot of existing session ids taken *before*
    //     spawn so the post-spawn poll can isolate the new one. The probe
    //     is a blocking `opencode session list` subprocess, so run it on
    //     the blocking pool rather than stalling the async runtime.
    if let Some(capture) = pending_opencode_capture.as_mut() {
        let command = capture.command.clone();
        let cwd = capture.cwd.clone();
        capture.before_ids = tauri::async_runtime::spawn_blocking(move || {
            agent::snapshot_opencode_session_ids(&command, &cwd)
        })
        .await
        .unwrap_or_default();
    }

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
    // Slice 4: commit the one-executor reservation to `Live` now that the
    // session exists (execution runs only; `None` for non-jira/plan starts).
    // The pre-spawn guard already claimed the slot as `Reserving`.
    if let Some(reservation) = executor_reservation.take() {
        reservation.commit(session_id.clone());
    }
    tracing::info!(
        task = %task_id, agent = ?agent_kind, model = %model, resumed, auto_mode,
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

    // 6./7. Persist the run record and (Codex/Antigravity/OpenCode first-run only)
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

        if let Some(capture) = pending_opencode_capture {
            let task_runs = state.task_runs.clone();
            let app_handle = state.app_handle.lock().ok().and_then(|h| h.clone());
            let task_id_clone = task_id.clone();
            tauri::async_runtime::spawn(async move {
                let found = wait_for_opencode_session(capture).await;
                match found {
                    Some(session_id) => {
                        if let Err(e) = task_runs.set_conversation_id(&task_id_clone, &session_id) {
                            tracing::warn!(error = ?e, task = %task_id_clone, "set_conversation_id failed");
                        } else {
                            tracing::info!(task = %task_id_clone, session = %session_id, "captured opencode session id");
                            if let Some(app) = app_handle {
                                let _ = app.emit("task_run_changed", &task_id_clone);
                            }
                        }
                    }
                    None => {
                        tracing::warn!(task = %task_id_clone, "opencode session capture timed out");
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

/// Poll `opencode session list --format json` until a new session appears
/// or we give up. The command is non-interactive but still a process
/// spawn, so each attempt runs on the blocking pool.
async fn wait_for_opencode_session(capture: OpenCodeCapture) -> Option<String> {
    use tokio::time::{sleep, Duration};
    for _ in 0..20 {
        let capture_for_poll = capture.clone();
        let found = tauri::async_runtime::spawn_blocking(move || {
            agent::find_opencode_session_id(&capture_for_poll)
        })
        .await
        .ok()
        .flatten();
        if found.is_some() {
            return found;
        }
        sleep(Duration::from_millis(500)).await;
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
            .filter(|a| a.kind == agent_kind)
            .and_then(|a| a.command.clone())
            .or_else(|| {
                cfg.agente
                    .as_ref()
                    .filter(|a| a.kind == agent_kind)
                    .and_then(|a| a.command.clone())
            });
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
        // OpenCode capture is intentionally unused here: idea-decomposition
        // runs are synthetic (IDEIA-*) and never recorded, so there is no run
        // record to patch and nothing to resume. Skipping it also avoids the
        // post-spawn `opencode session list` polling subprocesses entirely.
        pending_opencode_capture: _,
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

// ───────────────────── memória compartilhada por projeto ─────────────────────
//
// A memória oficial é uma lista curada de itens por projeto. O usuário é
// o curador: edita itens manualmente, promove aprendizados sugeridos pelo
// agente de execução (no review da task) e aprova/rejeita ops de reeval.
// Nada gerado por agente entra na memória sem passar por aqui.

fn emit_memory_changed(state: &AppState, project_id: &str) {
    if let Some(app) = state.app_handle.lock().ok().and_then(|h| h.clone()) {
        let _ = app.emit(cadenza_proto::ops::EV_MEMORY_CHANGED, project_id);
    }
}

#[tauri::command]
pub async fn get_project_memory(
    state: State<'_, Arc<AppState>>,
    project_id: String,
) -> Result<Vec<MemoryItem>, String> {
    state
        .repo
        .list_memory(&project_id)
        .await
        .map_err(to_str_err)
}

/// Adiciona um item à memória oficial (edição manual do usuário). O
/// backend gera o id estável (`M-<uuid>`) e o timestamp.
#[tauri::command]
pub async fn add_memory_item(
    state: State<'_, Arc<AppState>>,
    project_id: String,
    texto: String,
) -> Result<MemoryItem, String> {
    let texto = texto.trim();
    if texto.is_empty() {
        return Err("texto is required".to_string());
    }
    let item = MemoryItem {
        id: format!("M-{}", Uuid::new_v4().simple()),
        texto: texto.to_string(),
        origem_task: None,
        criado_em: chrono::Utc::now().timestamp_millis(),
    };
    state
        .repo
        .add_memory_item(&project_id, &item)
        .await
        .map_err(to_str_err)?;
    emit_memory_changed(&state, &project_id);
    Ok(item)
}

#[tauri::command]
pub async fn update_memory_item(
    state: State<'_, Arc<AppState>>,
    project_id: String,
    item_id: String,
    texto: String,
) -> Result<(), String> {
    let texto = texto.trim();
    if texto.is_empty() {
        return Err("texto is required".to_string());
    }
    state
        .repo
        .update_memory_item(&project_id, &item_id, texto)
        .await
        .map_err(to_str_err)?;
    emit_memory_changed(&state, &project_id);
    Ok(())
}

#[tauri::command]
pub async fn delete_memory_item(
    state: State<'_, Arc<AppState>>,
    project_id: String,
    item_id: String,
) -> Result<(), String> {
    state
        .repo
        .delete_memory_item(&project_id, &item_id)
        .await
        .map_err(to_str_err)?;
    emit_memory_changed(&state, &project_id);
    Ok(())
}

#[tauri::command]
pub async fn list_memory_suggestions(
    state: State<'_, Arc<AppState>>,
    project_id: String,
) -> Result<Vec<MemorySuggestion>, String> {
    state
        .repo
        .list_memory_suggestions(&project_id)
        .await
        .map_err(to_str_err)
}

/// Resolve uma sugestão pendente: `aprovar=true` aplica a op à memória
/// oficial e remove a sugestão; `aprovar=false` apenas descarta. Tudo é
/// curadoria explícita do usuário — o agente nunca chega aqui.
///
/// `Contradicao` é informativa: aprovar não muda a memória (o usuário
/// resolve editando), então tratamos qualquer resolução como descarte.
#[tauri::command]
pub async fn resolve_memory_suggestion(
    state: State<'_, Arc<AppState>>,
    suggestion_id: String,
    aprovar: bool,
) -> Result<(), String> {
    let suggestion = state
        .repo
        .read_memory_suggestion(&suggestion_id)
        .await
        .map_err(to_str_err)?
        .ok_or_else(|| format!("memory suggestion '{suggestion_id}' not found"))?;
    let project_id = suggestion.project_id.clone();

    // Claim the suggestion by removing it *first*, so a concurrent resolve
    // (double-click, a second window) can't apply the same op twice — the
    // `mint`-based ops below add a fresh `M-<uuid>` on every call and are
    // not idempotent. If it's already gone, another caller handled it.
    match state.repo.delete_memory_suggestion(&suggestion_id).await {
        Ok(()) => {}
        Err(StoreError::NotFound(_)) => return Ok(()),
        Err(e) => return Err(to_str_err(e)),
    }
    if aprovar {
        apply_memory_suggestion(&state, &project_id, &suggestion.kind).await?;
    }
    emit_memory_changed(&state, &project_id);
    Ok(())
}

/// Muta a memória oficial conforme a op aprovada. Não remove a sugestão —
/// isso é responsabilidade do chamador (`resolve_memory_suggestion`).
async fn apply_memory_suggestion(
    state: &AppState,
    project_id: &str,
    kind: &SuggestionKind,
) -> Result<(), String> {
    let mint = |texto: &str, origem_task: Option<String>| MemoryItem {
        id: format!("M-{}", Uuid::new_v4().simple()),
        texto: texto.to_string(),
        origem_task,
        criado_em: chrono::Utc::now().timestamp_millis(),
    };
    match kind {
        SuggestionKind::Aprendizado { texto, origem_task } => {
            let item = mint(texto, origem_task.clone());
            state
                .repo
                .add_memory_item(project_id, &item)
                .await
                .map_err(to_str_err)?;
        }
        SuggestionKind::Nova { texto } => {
            let item = mint(texto, None);
            state
                .repo
                .add_memory_item(project_id, &item)
                .await
                .map_err(to_str_err)?;
        }
        SuggestionKind::Remover { target_id } => {
            // A target that's already gone is the desired end-state — tolerate
            // it (as Mesclar does) so approving a stale op doesn't error and
            // wedge the suggestion in the queue.
            match state.repo.delete_memory_item(project_id, target_id).await {
                Ok(()) => {}
                Err(StoreError::NotFound(_)) => {
                    tracing::warn!(target = %target_id, "remover: target already absent");
                }
                Err(e) => return Err(to_str_err(e)),
            }
        }
        SuggestionKind::Reescrever {
            target_id,
            novo_texto,
        } => {
            // Rewriting an item that's already gone is moot — tolerate it
            // rather than erroring so the suggestion still clears.
            match state
                .repo
                .update_memory_item(project_id, target_id, novo_texto)
                .await
            {
                Ok(()) => {}
                Err(StoreError::NotFound(_)) => {
                    tracing::warn!(target = %target_id, "reescrever: target already absent");
                }
                Err(e) => return Err(to_str_err(e)),
            }
        }
        SuggestionKind::Mesclar {
            target_ids,
            texto_mesclado,
        } => {
            // Cria o item fundido primeiro; só então remove os originais,
            // para que uma falha no meio não perca todo o conteúdo.
            let item = mint(texto_mesclado, None);
            state
                .repo
                .add_memory_item(project_id, &item)
                .await
                .map_err(to_str_err)?;
            for target in target_ids {
                if let Err(e) = state.repo.delete_memory_item(project_id, target).await {
                    // Item já removido / inexistente não é fatal — a fusão
                    // já registrou o texto consolidado.
                    tracing::warn!(error = ?e, target = %target, "merge: delete target failed");
                }
            }
        }
        SuggestionKind::Contradicao { .. } => {
            // Informativa — aprovar não altera a memória.
        }
    }
    Ok(())
}

/// Dispara um agente em PTY na pasta do projeto para **reavaliar** a
/// memória oficial. Espelha `destrinchar_ideia`: resolve projeto + cwd,
/// planeja o comando do agente, seta `CADENZA_MEMORY_REEVAL=1` e registra
/// a sessão. O agente lê a memória atual e emite sugestões de reeval via
/// `cadenza-cli memory revise` — nada é aplicado automaticamente.
#[tauri::command]
pub async fn reavaliar_memoria(
    state: State<'_, Arc<AppState>>,
    project_id: String,
    agent_kind: AgenteKind,
    model: String,
) -> Result<StartTaskAgentResult, String> {
    // 1. Resolver projeto + cwd + override de comando do agente.
    let (cwd, command_override) = {
        let cfg = state.config.lock().map_err(to_str_err)?;
        let project = cfg
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .ok_or_else(|| format!("project '{project_id}' not found in config"))?;
        let project_path = project.path.clone();
        let cmd = project
            .agente
            .as_ref()
            .filter(|a| a.kind == agent_kind)
            .and_then(|a| a.command.clone())
            .or_else(|| {
                cfg.agente
                    .as_ref()
                    .filter(|a| a.kind == agent_kind)
                    .and_then(|a| a.command.clone())
            });
        (project_path, cmd)
    };

    if !cwd.exists() {
        return Err(format!(
            "project path does not exist: {} — fix it in Settings → Projetos",
            cwd.display()
        ));
    }

    // 2. Plan + env. Sempre fresh; sempre há prompt inicial.
    let synthetic_task_id = format!("MEMORY-{project_id}");
    let prompt = render_memory_reeval_prompt(&state.i18n, &project_id);
    let plan: LaunchPlan = agent::plan_launch(
        agent_kind,
        &model,
        command_override.as_deref(),
        &cwd,
        &synthetic_task_id,
        &project_id,
        None,
        Some(&prompt),
    );
    let LaunchPlan {
        spawn,
        conversation_id_known,
        pending_codex_capture,
        pending_opencode_capture: _,
        prompt_delivery,
    } = plan;
    let spawn = spawn.memory_reeval_env(&project_id);

    // 3. Spawn PTY + registrar sessão.
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
        project = %project_id, agent = ?agent_kind, model = %model,
        session = %session_id, "memory reeval agent started"
    );

    // 3a. Entrega do prompt inicial (argv ou type-in), igual aos demais.
    if prompt_delivery == PromptDelivery::TypeIn {
        let session_for_prompt = session.clone();
        tauri::async_runtime::spawn(async move {
            send_initial_prompt(&session_for_prompt, &prompt).await;
        });
    }

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

/// Build the project-memory block appended to a fresh execution prompt.
/// One bullet per curated item. Returns a leading-blank-line-separated
/// section so it reads as its own block after the base prompt.
fn render_memory_block(i18n_slot: &Mutex<I18n>, items: &[MemoryItem]) -> String {
    let bullets = items
        .iter()
        .map(|i| format!("- {}", i.texto))
        .collect::<Vec<_>>()
        .join("\n");
    let mut args = FluentArgs::new();
    args.set("itens", bullets.clone());
    match i18n_slot.lock() {
        Ok(i18n) => format!("\n\n{}", i18n.t_with("agent-memory-block", Some(&args))),
        Err(_) => format!(
            "\n\nProject memory — durable facts, decisions and conventions for this project:\n{bullets}"
        ),
    }
}

fn render_memory_reeval_prompt(i18n_slot: &Mutex<I18n>, project_id: &str) -> String {
    let mut args = FluentArgs::new();
    args.set("project_id", project_id.to_string());
    match i18n_slot.lock() {
        Ok(i18n) => i18n.t_with("agent-initial-prompt-memory-reeval", Some(&args)),
        Err(_) => format!(
            "Use the `cadenza` skill to coordinate with Cadenza. You are in MEMORY REEVALUATION mode for project {project_id}. Read the current memory with `cadenza-cli memory list --json` and emit review suggestions (remove obsolete, merge duplicates, rewrite confusing items, flag contradictions, propose new) via `cadenza-cli memory revise --op ...`. Do not change anything directly — the human curates."
        ),
    }
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
            .filter(|a| a.kind == agent_kind)
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
        crate::models::discover_models(
            &cmd_for_spawn,
            agent_kind,
            8,
            6,
            1,
            refresh.unwrap_or(false),
        )
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
        let source = if agent_kind == AgenteKind::OpenCode {
            "`opencode models` output"
        } else {
            "the agent's `/model` menu"
        };
        return Err(format!(
            "no models parsed from `{command}` — {source} likely changed shape; please report this"
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

// ───────── review packages (PLAN §D.13) ─────────────────────────────────

/// Latest review package for a task (highest attempt), or `None` when the
/// task has never been `done` with evidence. The webview's "Revisão" tab
/// renders the returned [`ReviewPackage`] directly (PLAN §D.13).
#[tauri::command]
pub async fn get_review_package(
    state: State<'_, Arc<AppState>>,
    task_id: String,
) -> Result<Option<crate::store::ReviewPackage>, String> {
    state
        .repo
        .latest_review_package(&task_id)
        .await
        .map_err(to_str_err)
}

/// One file's diff text within a [`DiffGroup`].
#[derive(Serialize)]
pub struct DiffFile {
    pub path: String,
    /// Unified-diff text, size-capped by the engine.
    pub patch: String,
    /// True when the per-file cap clipped this file's patch.
    pub truncated: bool,
}

/// Files bucketed under one stored intent-group label (uncovered → "Other").
#[derive(Serialize)]
pub struct DiffGroup {
    pub label: String,
    pub files: Vec<DiffFile>,
}

/// Response of [`get_review_diff`] (PLAN §D.13).
#[derive(Serialize)]
pub struct ReviewDiff {
    /// True when the current worktree fingerprint differs from the stored
    /// one: the live committed diff would be divergent, so we serve the
    /// stored capped uncommitted patch instead.
    pub stale: bool,
    /// Committed diff bucketed by the stored intent groups. Empty when the
    /// base/head shas are missing or the worktree is gone (see `note`).
    pub groups: Vec<DiffGroup>,
    /// The stored capped+redacted uncommitted patch, served only when
    /// `stale` is true.
    pub uncommitted: Option<crate::review::CappedPatch>,
    /// True when the worktree was unreadable / base or head sha is missing,
    /// so no live committed diff could be computed.
    pub diff_unavailable: bool,
    /// True when a per-file or total size cap clipped the live diff.
    pub truncated: bool,
    /// Files dropped entirely by the live diff's count/total caps.
    pub files_omitted: u32,
}

/// Bucket label for files not covered by any stored intent group.
const DIFF_OTHER_LABEL: &str = "Other";

/// Recompute the committed diff live from the stored `base_sha..head_sha`
/// and bucket it by the stored intent groups (uncovered files → "Other").
///
/// Staleness: the current worktree fingerprint is recomputed (hardened
/// `git status --porcelain=v1 -z`) and compared to the one stored at
/// `done` time. On mismatch we set `stale = true` and serve the stored
/// capped+redacted uncommitted patch (a live uncommitted diff would now be
/// divergent), per PLAN §D.13. The committed diff is always recomputed from
/// the immutable shas. Missing worktree / unresolved base ⇒ empty groups +
/// `diff_unavailable`.
#[tauri::command]
pub async fn get_review_diff(
    state: State<'_, Arc<AppState>>,
    task_id: String,
) -> Result<ReviewDiff, String> {
    let Some(pkg) = state
        .repo
        .latest_review_package(&task_id)
        .await
        .map_err(to_str_err)?
    else {
        return Err(format!("no review package for task {task_id}"));
    };

    // Resolve the worktree out of the side-mapping (mirrors done_op).
    let worktree_path = state
        .task_worktrees
        .get(&task_id)
        .and_then(|w| w.worktree_path)
        .filter(|p| !p.trim().is_empty());

    // Staleness: compare the live fingerprint to the stored one. An
    // unreadable live fingerprint (worktree gone) is treated as "cannot
    // compare" ⇒ not stale, and the committed diff below will be empty.
    let mut stale = false;
    if let (Some(wt), Some(stored_fp)) =
        (worktree_path.as_deref(), pkg.worktree_fingerprint.as_ref())
    {
        if let Some(live_fp) = crate::review::worktree_fingerprint(Path::new(wt)).await {
            stale = &live_fp != stored_fp;
        }
    }

    // Committed diff is always reproducible from the immutable shas.
    let paths: Vec<String> = pkg.changed_files.iter().map(|f| f.path.clone()).collect();
    let (groups, diff_unavailable, truncated, files_omitted) = match (
        worktree_path.as_deref(),
        pkg.base_sha.as_deref(),
        pkg.head_sha.as_deref(),
    ) {
        (Some(wt), Some(base), Some(head)) => {
            let live =
                crate::review::recompute_committed_diff(Path::new(wt), base, head, &paths).await;
            (
                bucket_diff_by_groups(live.files, &pkg.groups),
                false,
                live.truncated,
                live.files_omitted,
            )
        }
        // Missing worktree or unresolved base/head ⇒ no live committed diff.
        _ => (Vec::new(), true, false, 0),
    };

    Ok(ReviewDiff {
        stale,
        groups,
        uncommitted: if stale {
            pkg.uncommitted_patch.clone()
        } else {
            None
        },
        diff_unavailable,
        truncated,
        files_omitted,
    })
}

/// Bucket live diff files by the stored intent groups, preserving group
/// display order; files not listed in any group fall into a trailing
/// "Other" group. Each file lands in the first group that lists it. Empty
/// groups are dropped so the UI never renders an empty section.
fn bucket_diff_by_groups(
    files: Vec<crate::review::LiveDiffFile>,
    groups: &[crate::review::IntentGroup],
) -> Vec<DiffGroup> {
    use std::collections::HashMap;

    // path → first group index that claims it.
    let mut owner: HashMap<&str, usize> = HashMap::new();
    for (gi, g) in groups.iter().enumerate() {
        for f in &g.files {
            owner.entry(f.as_str()).or_insert(gi);
        }
    }

    let mut buckets: Vec<Vec<DiffFile>> = (0..groups.len()).map(|_| Vec::new()).collect();
    let mut other: Vec<DiffFile> = Vec::new();
    for f in files {
        let df = DiffFile {
            path: f.path.clone(),
            patch: f.patch,
            truncated: f.truncated,
        };
        match owner.get(f.path.as_str()) {
            Some(&gi) => buckets[gi].push(df),
            None => other.push(df),
        }
    }

    let mut out: Vec<DiffGroup> = Vec::new();
    for (g, files) in groups.iter().zip(buckets) {
        if !files.is_empty() {
            out.push(DiffGroup {
                label: g.label.clone(),
                files,
            });
        }
    }
    if !other.is_empty() {
        out.push(DiffGroup {
            label: DIFF_OTHER_LABEL.to_string(),
            files: other,
        });
    }
    out
}

/// The human approve / request-changes transition from the webview
/// (PLAN §E.16). Shares the transition guard + atomic state/log/decision
/// writes with the NDJSON `review_decision_op` via
/// [`apply_review_decision`]. Returns the new estado string so the UI can
/// refresh without a follow-up read.
#[tauri::command]
pub async fn review_decision(
    state: State<'_, Arc<AppState>>,
    task_id: String,
    verdict: String,
    note: String,
) -> Result<String, String> {
    use cadenza_proto::ops::review_decision::Verdict;
    let verdict = match verdict.as_str() {
        "aprovado" => Verdict::Aprovado,
        "pedir_alteracoes" => Verdict::PedirAlteracoes,
        other => return Err(format!("invalid verdict: {other}")),
    };
    apply_review_decision(state.repo.as_ref(), &task_id, verdict, &note)
        .await
        .map(|estado| estado.as_str().to_string())
        .map_err(|e| e.message)
}

/// Latest `evidence_state` per task, for the board badge (PLAN §E.15).
/// One round-trip instead of an `N+1` `get_review_package` per card: the
/// caller reads every package once and keeps the highest attempt per task.
#[tauri::command]
pub async fn list_review_states(
    state: State<'_, Arc<AppState>>,
) -> Result<HashMap<String, crate::review::EvidenceState>, String> {
    let all = state.repo.all_review_packages().await.map_err(to_str_err)?;
    // Keep the highest-attempt package per task.
    let mut latest_attempt: HashMap<String, u32> = HashMap::new();
    let mut states: HashMap<String, crate::review::EvidenceState> = HashMap::new();
    for pkg in all {
        let slot = latest_attempt.entry(pkg.task_id.clone()).or_insert(0);
        if pkg.attempt >= *slot {
            *slot = pkg.attempt;
            states.insert(pkg.task_id.clone(), pkg.evidence_state);
        }
    }
    Ok(states)
}

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
