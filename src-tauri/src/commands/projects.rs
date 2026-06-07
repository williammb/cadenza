//! Task↔project mapping + active-project command handlers — split out of
//! the `commands` god-module. Pure relocation. Re-exported via `commands`'s
//! `pub use projects::*;` so every existing `commands::*` path still resolves
//! unchanged (Tauri `generate_handler!` in lib.rs references these paths).

// Bring in the parent module's imports and shared helpers (AppState,
// to_str_err, Config, HashMap, State, Arc, etc.). Parent-private items are
// visible here.
use super::*;

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
