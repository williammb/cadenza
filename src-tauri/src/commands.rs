//! Tauri `#[command]` handlers — the in-process IPC surface used by the
//! React frontend. Per DESIGN-desktop-v2.md § "commands.rs". The CLI
//! talks to the app over a separate NDJSON socket (Phase 4), not these
//! handlers.

use cadenza_i18n::{locale, FluentArgs, I18n, LocaleSources};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::ipc::Channel;
use tauri::{Emitter, State};
use uuid::Uuid;

use crate::agent::{self, CodexCapture, LaunchPlan};
use crate::config::{AgenteKind, Config, PgConfig, PgSslMode, Project, StorageBackend};
use crate::projects::TaskProjects;
use crate::runs::{TaskRun, TaskRuns};
use crate::secrets;
use crate::spawn::{PtyHandle, SpawnConfig};
use crate::store::{
    migrate, DecisaoRegistro, Estado, FileRepository, Ideia, IdeiaStatus, NewProposta,
    PgConnectionParams, PgRepository, PgSslModeChoice, Proposta, Repository, SqliteRepository,
    Task,
};
use crate::terminal::TerminalSession;

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
    /// AppHandle for emitting events (e.g. `task_run_changed` from the
    /// async Codex-uuid capture task). Set once during `setup()`.
    pub app_handle: Mutex<Option<tauri::AppHandle>>,
    /// Monotonic counter bumped by the tray "Revoke CLI token" handler.
    /// IPC connections capture the current value at hello-time; each
    /// dispatch checks against the live counter and rejects ops when
    /// they don't match so a revoked-mid-session connection can't keep
    /// driving the server until it disconnects on its own.
    pub token_epoch: AtomicU64,
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

        // Amarra tasks órfãs ao primeiro projeto. Idempotente.
        ensure_default_project_and_bind_orphans(&config, &task_projects, repo.as_ref())?;

        Ok(AppState {
            repo,
            config: Mutex::new(config),
            i18n: Mutex::new(i18n),
            sessions: Mutex::new(HashMap::new()),
            task_projects,
            task_runs,
            app_handle: Mutex::new(None),
            token_epoch: AtomicU64::new(0),
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
    state.repo.list_tasks(filter).await.map_err(to_str_err)
}

#[tauri::command]
pub async fn read_task(state: State<'_, Arc<AppState>>, id: String) -> Result<Task, String> {
    state.repo.read_task(&id).await.map_err(to_str_err)
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
    let tasks = state.repo.list_tasks(None).await.map_err(to_str_err)?;
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
    Ok(())
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
    state.repo.current_task().await.map_err(to_str_err)
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
#[tauri::command]
pub async fn decidir_proposta(
    state: State<'_, Arc<AppState>>,
    registro: DecisaoRegistro,
) -> Result<(), String> {
    state.repo.write_decisao(registro).await.map_err(to_str_err)
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

    // Replay scrollback first so reattaches don't lose context.
    let snap = session.snapshot();
    if !snap.is_empty() {
        let _ = channel.send(snap);
    }

    let mut rx = session.subscribe();
    tokio::spawn(async move {
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
) -> Result<StartTaskAgentResult, String> {
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

    let (cwd, command_override) = {
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

    if !cwd.exists() {
        return Err(format!(
            "project path does not exist: {} — fix it in Settings → Projetos",
            cwd.display()
        ));
    }

    // 3. Decide new vs resume from `task-runs.json`.
    let existing = state.task_runs.get(&task_id);
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

    // 4. Build SpawnConfig via the per-agent planner.
    let plan: LaunchPlan = agent::plan_launch(
        agent_kind,
        &model,
        command_override.as_deref(),
        &cwd,
        &task_id,
        &project_id,
        existing_conv_id.as_deref(),
    );
    let LaunchPlan {
        spawn,
        conversation_id_known,
        pending_codex_capture,
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

    // 5a. On a fresh start (not a resume), seed an initial prompt into
    //     the PTY so the agent knows which task to work on and that the
    //     `cadenza` skill is the contract. We delay ~1.5s to let the
    //     agent's UI initialize — writing before the input box is ready
    //     causes the bytes to be dropped on both Claude Code and Codex.
    if !resumed {
        let prompt = render_initial_task_prompt(&state.i18n, &task_id, &task_titulo);
        let session_for_prompt = session.clone();
        tauri::async_runtime::spawn(async move {
            send_initial_prompt(&session_for_prompt, &prompt).await;
        });
    }

    // 5b. With the spawn confirmed, move the task to `fazendo` if it
    //     wasn't already. Logged-only on failure: the agent is already
    //     running, the user can move the card manually if needed.
    if original_estado != Estado::Fazendo {
        if let Err(e) = state.repo.set_estado(&task_id, Estado::Fazendo).await {
            tracing::warn!(error = ?e, task = %task_id, "set_estado(fazendo) after spawn failed");
        }
    }

    // 6. Persist the run record.
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

    // 7. (Codex first-run only) spawn an async capture task that
    //    polls ~/.codex/sessions/ until the new rollout file appears,
    //    parses its UUID, and patches the run record.
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
/// is started fresh. Falls back to a plain English message if the
/// `agent-initial-prompt` key isn't in either bundle.
fn render_initial_task_prompt(i18n_slot: &Mutex<I18n>, task_id: &str, titulo: &str) -> String {
    let mut args = FluentArgs::new();
    args.set("task_id", task_id.to_string());
    args.set("titulo", titulo.to_string());
    match i18n_slot.lock() {
        Ok(i18n) => i18n.t_with("agent-initial-prompt", Some(&args)),
        Err(_) => format!(
            "Use the `cadenza` skill to coordinate with Cadenza through cadenza-cli. Your task is {task_id} ({titulo}). Start by running `cadenza-cli current --json`."
        ),
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
    state.repo.delete_ideia(&id).await.map_err(to_str_err)
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

    // 4. Plan + adiciona env vars específicas da ideia.
    let plan: LaunchPlan = agent::plan_launch(
        agent_kind,
        &model,
        command_override.as_deref(),
        &cwd,
        &synthetic_task_id,
        &ideia.project_id,
        None,
    );
    let LaunchPlan {
        spawn,
        conversation_id_known,
        pending_codex_capture,
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

    // 5a. Seed an initial prompt — same rationale as start_task_agent.
    let prompt = render_initial_ideia_prompt(&state.i18n, &ideia.id);
    let session_for_prompt = session.clone();
    tauri::async_runtime::spawn(async move {
        send_initial_prompt(&session_for_prompt, &prompt).await;
    });

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

#[cfg(test)]
mod tests {
    use super::highest_task_number;

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
}
