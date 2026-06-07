//! Skills (CLI snippet) command handlers — split out of the `commands`
//! god-module. Pure relocation. Re-exported via `commands`'s
//! `pub use skills::*;` so `commands::skill_install` / `commands::skill_remove`
//! / `commands::skill_status` still resolve unchanged (Tauri
//! `generate_handler!` in lib.rs references these paths).
//!
//! Wrappers around `skills-core`. The actual filesystem work (writing
//! SKILL.md, editing AGENTS.md, deleting on remove) lives in the shared
//! crate so the cadenza-cli command and these handlers stay in lockstep.
//!
//! `skill_install` uses the app's active locale as the body language —
//! the Settings UI doesn't expose a locale picker here because switching
//! the app language already covers it.

// Bring in the parent module's imports and shared helpers (AppState,
// to_str_err, etc.). Parent-private items are visible to this child module.
use super::*;

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
