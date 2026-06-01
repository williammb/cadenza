use std::path::PathBuf;
use std::sync::Arc;

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager,
};

use cadenza_i18n::I18n;

mod agent;
mod attachments;
mod auth;
mod blockers;
mod commands;
mod config;
mod git;
mod ipc;
mod models;
mod notify;
mod observ;
mod ordering;
mod projects;
mod runs;
mod secrets;
mod spawn;
mod store;
mod terminal;
mod worktrees;

use commands::AppState;

fn data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".cadenza")
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _log_guard = match observ::init() {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("warning: failed to initialize logging: {e}");
            None
        }
    };
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting cadenza");

    let app_state = match AppState::init() {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("fatal: failed to initialize app state: {e:#}");
            std::process::exit(1);
        }
    };

    let dir = data_dir();
    let _token = match auth::ensure_token(&dir) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fatal: failed to ensure auth token: {e:#}");
            std::process::exit(1);
        }
    };

    // The IPC server is spawned from `.setup()` below so it can share
    // an mpsc channel with the AppHandle for forwarding webview events
    // (e.g. `proposta_pendente` reaching the triage modal).
    let ipc_state = app_state.clone();
    let ipc_dir = dir.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        // Tauri's State<T> wants T directly; we hold the same Arc<AppState>
        // separately for the IPC server, then manage a clone here so
        // both halves see the same data behind one Arc.
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            // tasks
            commands::list_tasks,
            commands::read_task,
            commands::next_task_id,
            commands::create_task,
            commands::set_estado,
            commands::set_task_order,
            commands::set_titulo,
            commands::append_log,
            commands::update_task_body,
            commands::delete_task,
            commands::current_task,
            // attachments (images embedded in task/ideia bodies)
            commands::save_attachment,
            commands::read_attachment,
            // triage
            commands::list_pending_propostas,
            commands::read_proposta,
            commands::read_decisao,
            commands::decidir_proposta,
            commands::propose,
            commands::await_proposta_decisao,
            // PTY
            commands::pty_spawn,
            commands::pty_write,
            commands::pty_resize,
            commands::pty_kill,
            commands::pty_snapshot,
            commands::pty_attach,
            // agent runs (task → agent/model/conversation_id)
            commands::start_task_agent,
            commands::read_task_run,
            commands::list_task_runs,
            commands::clear_task_run,
            // i18n / config
            commands::get_locale,
            commands::set_locale,
            commands::load_translations,
            commands::get_config,
            commands::save_config,
            // task ↔ project
            commands::list_task_projects,
            commands::set_task_project,
            commands::set_active_project,
            // task ↔ worktree/branch
            commands::list_task_worktrees,
            commands::set_task_blockers,
            commands::set_task_worktree,
            commands::task_worktree_defaults,
            // ideias (Inbox)
            commands::list_ideias,
            commands::read_ideia,
            commands::create_ideia,
            commands::delete_ideia,
            commands::set_ideia_status,
            commands::destrinchar_ideia,
            // memória compartilhada por projeto (T-34)
            commands::get_project_memory,
            commands::add_memory_item,
            commands::update_memory_item,
            commands::delete_memory_item,
            commands::list_memory_suggestions,
            commands::resolve_memory_suggestion,
            commands::reavaliar_memoria,
            commands::set_storage_backend,
            commands::test_db_connection,
            commands::set_pg_password,
            commands::clear_pg_password,
            commands::restart_app,
            commands::check_update,
            commands::install_update_and_restart,
            // skills (CLI snippets for supported agents)
            commands::skill_install,
            commands::skill_remove,
            commands::skill_status,
            commands::list_installed_agents,
            commands::list_agent_models,
            commands::app_version,
        ])
        .setup(move |app| {
            // Stash the AppHandle on AppState so background tasks
            // (e.g. the Codex session-uuid capture) can emit webview
            // events without needing a State<_> at call time.
            if let Some(state) = app.try_state::<Arc<AppState>>() {
                if let Ok(mut slot) = state.app_handle.lock() {
                    *slot = Some(app.handle().clone());
                }
            }

            // Pipe IPC-emitted webview events into AppHandle::emit. The
            // channel buffer is modest: if a burst overflows, we drop —
            // the UI reconciles via list_pending_propostas / list_tasks
            // on next interaction.
            let (webview_tx, mut webview_rx) =
                tokio::sync::mpsc::channel::<(String, serde_json::Value)>(64);
            let server_deps = ipc::ServerDeps {
                state: ipc_state,
                data_dir: ipc_dir,
                webview_events: webview_tx,
            };
            // The IPC server runs forever in the happy path. If
            // `run_server` returns `Err` (e.g. Windows named-pipe name
            // collision, missing `~/.cadenza/run/` permission), the
            // UI keeps running but the socket is dead — every
            // `cadenza-cli` call would then exit 10 "app not running"
            // with the user staring at the visibly-alive webview. Log
            // loud AND emit a webview event so the UI can surface a
            // banner instead of failing silently.
            let crash_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                match ipc::run_server(server_deps).await {
                    Ok(()) => tracing::warn!("ipc server stopped"),
                    Err(e) => {
                        tracing::error!(error = ?e, "ipc server exited");
                        let _ = crash_handle.emit(
                            "ipc_server_crashed",
                            format!("{e:#}"),
                        );
                    }
                }
            });
            let emit_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                while let Some((event, payload)) = webview_rx.recv().await {
                    if let Err(e) = emit_handle.emit(&event, payload) {
                        tracing::warn!(error = ?e, event = %event, "emit failed");
                    }
                }
            });

            // Updater: boot check + hourly recurring poll. `Interval::tick`
            // resolves immediately on the first call, so we get the boot
            // check for free without a separate up-front invocation.
            // Failures (signature mismatch, network down, no release yet)
            // are warn-level because they're expected in dev / offline
            // runs and shouldn't drown out real errors.
            let updater_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut ticker = tokio::time::interval(
                    std::time::Duration::from_secs(60 * 60),
                );
                // Default Burst would replay every missed tick back-to-back
                // after a long suspend, firing a flurry of checks and
                // OS notifications at once. Skip collapses them to one.
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    check_for_updates(&updater_handle).await;
                }
            });

            // Skill freshness: a newer app build can ship a newer skill
            // body, but the copy already written to the user's agent dirs
            // stays stale until reinstalled. Warn once at boot if any
            // installed copy is outdated; the Settings panel shows the
            // per-agent badge and the reinstall button.
            let skill_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                notify_outdated_skills(&skill_handle).await;
            });

            // Tray labels follow the active app locale. Resolve once at
            // build time; live re-translation of the tray menu would
            // need rebuilding the menu after every locale switch, which
            // is out of scope for the MVP.
            let labels = TrayLabels::resolve(app);

            let abrir = MenuItem::with_id(app, "abrir", &labels.abrir, true, None::<&str>)?;
            let settings =
                MenuItem::with_id(app, "settings", &labels.settings, true, None::<&str>)?;
            let sep1 = PredefinedMenuItem::separator(app)?;
            let lang_pt = MenuItem::with_id(
                app,
                "lang:pt-BR",
                &labels.lang_pt,
                true,
                None::<&str>,
            )?;
            let lang_en = MenuItem::with_id(
                app,
                "lang:en",
                &labels.lang_en,
                true,
                None::<&str>,
            )?;
            let sep2 = PredefinedMenuItem::separator(app)?;
            let reiniciar = MenuItem::with_id(
                app,
                "reiniciar",
                &labels.reiniciar,
                true,
                None::<&str>,
            )?;
            let revogar =
                MenuItem::with_id(app, "revogar", &labels.revogar, true, None::<&str>)?;
            let copiar_diag = MenuItem::with_id(
                app,
                "copiar-diag",
                &labels.copiar_diag,
                true,
                None::<&str>,
            )?;
            let sep3 = PredefinedMenuItem::separator(app)?;
            let sair = MenuItem::with_id(app, "sair", &labels.sair, true, None::<&str>)?;

            let menu = Menu::with_items(
                app,
                &[
                    &abrir, &settings, &sep1, &lang_pt, &lang_en, &sep2, &reiniciar, &revogar,
                    &sep3, &copiar_diag, &sair,
                ],
            )?;

            let _tray = TrayIconBuilder::with_id("main")
                .tooltip("Cadenza")
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| {
                    let id = event.id.as_ref();
                    match id {
                        "abrir" => focus_main(app),
                        "settings" => {
                            focus_main(app);
                            if let Err(e) = app.emit("open_settings", ()) {
                                tracing::warn!(error = ?e, "emit open_settings");
                            }
                        }
                        "lang:pt-BR" => change_locale(app, "pt-BR"),
                        "lang:en" => change_locale(app, "en"),
                        "reiniciar" => {
                            tracing::info!("tray restart");
                            app.restart();
                        }
                        "sair" => {
                            tracing::info!("tray quit");
                            app.exit(0);
                        }
                        "copiar-diag" => {
                            let dir = data_dir();
                            let auth_path = dir.join("auth");
                            let socket = if cfg!(windows) {
                                format!(
                                    "\\\\.\\pipe\\cadenza-{}",
                                    std::env::var("USERNAME")
                                        .unwrap_or_else(|_| "<user>".into())
                                )
                            } else {
                                dir.join("run").join("socket").display().to_string()
                            };
                            // "exists" alone is misleading for an empty
                            // auth file — validate() rejects it, but the
                            // path is on disk. Surface the empty case so
                            // a support reader doesn't trust a useless
                            // token file.
                            let auth_status = match std::fs::read(&auth_path) {
                                Ok(b) if b.iter().any(|c| !c.is_ascii_whitespace()) => "exists",
                                Ok(_) => "EMPTY",
                                Err(_) => "MISSING",
                            };
                            let diag = format!(
                                "cadenza {ver}\nprotocol: {proto}\ndata dir: {data}\nauth file: {auth} ({auth_status})\nsocket: {socket}",
                                ver = env!("CARGO_PKG_VERSION"),
                                proto = cadenza_proto::MAX_PROTOCOL,
                                data = dir.display(),
                                auth = auth_path.display(),
                            );
                            match arboard::Clipboard::new()
                                .and_then(|mut cb| cb.set_text(&diag))
                            {
                                Ok(_) => tracing::info!("diagnostic copied to clipboard"),
                                Err(e) => tracing::warn!(error = ?e, "failed to copy diagnostic"),
                            }
                        }
                        "revogar" => {
                            let dir = data_dir();
                            match auth::revoke(&dir) {
                                Ok(_) => {
                                    tracing::info!("cli token revoked");
                                    // Bump the epoch so already-open
                                    // IPC connections get kicked on
                                    // their next op (token rotation
                                    // is otherwise invisible to them
                                    // because auth is only validated
                                    // at hello).
                                    if let Some(state) =
                                        app.try_state::<Arc<commands::AppState>>()
                                    {
                                        state.token_epoch.fetch_add(
                                            1,
                                            std::sync::atomic::Ordering::Release,
                                        );
                                    }
                                }
                                Err(e) => tracing::warn!(error = ?e, "failed to revoke token"),
                            }
                        }
                        other => {
                            tracing::debug!(item = %other, "tray menu (not wired)");
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running cadenza");
}

/// Restore + focus the main window. Shared by the `abrir` and
/// `settings` tray handlers — the latter has to bring the window up
/// before the emit lands, otherwise nothing visible happens.
fn focus_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Poll the configured update endpoint once. Emits `update_available`
/// (with the new version string) to the webview and surfaces an OS
/// notification when there's something to install — the UI decides
/// whether to prompt the user; we never auto-install.
///
/// Shared between the hourly ticker spawned in `setup()` and the
/// `check_update` Tauri command (manual trigger from the UI).
pub(crate) async fn check_for_updates(app: &tauri::AppHandle) {
    use tauri_plugin_updater::UpdaterExt;
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(error = ?e, "updater handle unavailable");
            return;
        }
    };
    match updater.check().await {
        Ok(Some(update)) => {
            let version = update.version.clone();
            tracing::info!(version = %version, "update available");
            if let Err(e) = app.emit("update_available", &version) {
                tracing::warn!(error = ?e, "emit update_available");
            }
            if let Err(e) =
                notify::show_info(app, "update-available-title", "update-available-body")
            {
                tracing::warn!(error = ?e, "show update notification");
            }
        }
        Ok(None) => tracing::debug!("no update available"),
        Err(e) => tracing::warn!(error = ?e, "updater check failed"),
    }
}

/// Check the installed agent skills against the current `SKILL_VERSION`
/// and, if any installed copy is outdated, emit `skill_update_available`
/// to the webview and surface an OS notification. The UI's Settings panel
/// also flags each outdated row with a reinstall button; this is the
/// at-boot nudge so the user notices without opening Settings.
pub(crate) async fn notify_outdated_skills(app: &tauri::AppHandle) {
    let outdated = skills_core::status(None)
        .into_iter()
        .filter(|r| r.installed && r.outdated)
        .count();
    if outdated == 0 {
        tracing::debug!("skills up to date");
        return;
    }
    tracing::info!(count = outdated, "outdated skill installs detected");
    if let Err(e) = app.emit("skill_update_available", outdated) {
        tracing::warn!(error = ?e, "emit skill_update_available");
    }
    if let Err(e) = notify::show_info(
        app,
        "skill-update-available-title",
        "skill-update-available-body",
    ) {
        tracing::warn!(error = ?e, "show skill update notification");
    }
}

/// Hot-swap the active locale and ask the UI to redraw with the new
/// strings. Called from the tray; the Settings dropdown does the same
/// thing via the Tauri command.
fn change_locale(app: &tauri::AppHandle, locale: &str) {
    if let Some(state) = app.try_state::<Arc<commands::AppState>>() {
        match state.i18n.lock() {
            Ok(mut slot) => *slot = I18n::new(locale),
            Err(e) => {
                tracing::warn!(error = ?e, "i18n lock poisoned");
                return;
            }
        }
        tracing::info!(locale = %locale, "locale changed via tray");
        if let Err(e) = app.emit("locale_changed", locale.to_string()) {
            tracing::warn!(error = ?e, "emit locale_changed");
        }
    }
}

/// Tray menu labels resolved once at setup time. The active locale
/// comes from `AppState.i18n`; if the state isn't reachable for any
/// reason we fall back to English so the tray is never blank.
struct TrayLabels {
    abrir: String,
    settings: String,
    lang_pt: String,
    lang_en: String,
    reiniciar: String,
    revogar: String,
    copiar_diag: String,
    sair: String,
}

impl TrayLabels {
    fn resolve(app: &tauri::App) -> Self {
        let state = app.try_state::<Arc<commands::AppState>>();
        let locale = state
            .as_ref()
            .and_then(|s| s.i18n.lock().ok().map(|i| i.active().to_string()))
            .unwrap_or_else(|| "en".to_string());
        let i18n = I18n::new(&locale);
        TrayLabels {
            abrir: i18n.t("tray-open"),
            settings: i18n.t("tray-settings"),
            lang_pt: i18n.t("tray-lang-pt"),
            lang_en: i18n.t("tray-lang-en"),
            reiniciar: i18n.t("tray-restart"),
            revogar: i18n.t("tray-revoke-token"),
            copiar_diag: i18n.t("tray-copy-diag"),
            sair: i18n.t("tray-quit"),
        }
    }
}
