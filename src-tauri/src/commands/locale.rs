//! Locale / i18n command handlers — split out of the `commands` god-module.
//! Pure relocation: the get/load/set locale commands. Re-exported via
//! `commands`'s `pub use locale::*;` so `commands::get_locale` /
//! `commands::load_translations` / `commands::set_locale` still resolve
//! unchanged (Tauri `generate_handler!` in lib.rs references these paths).

// Bring in the parent module's imports and shared helpers (AppState,
// to_str_err, I18n, HashMap, etc.). Parent-private items are visible to this
// child module.
use super::*;

// `super::*` would re-export this submodule's own name (`commands::locale`)
// over the `cadenza_i18n::locale` re-export in the parent, so import the i18n
// locale helpers explicitly to avoid the self-shadowing clash.
use cadenza_i18n::locale;

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
