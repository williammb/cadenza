//! Settings / storage / updater command handlers + repository
//! construction — split out of the `commands` god-module. Pure relocation.
//! Re-exported via `commands`'s `pub use config::*;` so every existing
//! `commands::*` path still resolves unchanged (Tauri `generate_handler!`
//! in lib.rs references these paths; `AppState::init` in mod.rs calls
//! `build_repo` / `ensure_default_project_and_bind_orphans` unqualified).

// Bring in the parent module's imports and shared helpers (AppState,
// to_str_err, Config, PgConfig, PgSslMode, StorageBackend, FileRepository,
// SqliteRepository, PgRepository, PgConnectionParams, PgSslModeChoice,
// migrate, secrets, Repository, TaskProjects, etc.). Parent-private items
// are visible to this child module.
use super::*;

/// Amarra tasks órfãs (sem entrada em `task-projects.json`) ao primeiro
/// projeto do config. Chamado em `AppState::init` antes de qualquer
/// comando rodar — preserva a constraint "toda task tem projeto" para
/// bases migradas da versão Node.js legacy. Se não há projetos, retorna
/// sem fazer nada; a UI detecta esse estado e guia o usuário a criar o
/// primeiro projeto.
pub(crate) fn ensure_default_project_and_bind_orphans(
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
pub(crate) fn build_repo(
    home: &std::path::Path,
    config: &Config,
) -> anyhow::Result<Arc<dyn Repository>> {
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

#[tauri::command]
pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
