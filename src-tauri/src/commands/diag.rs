//! Diagnostics export + logs-folder command handlers — split out of the
//! `commands` god-module. Pure relocation. Re-exported via `commands`'s
//! `pub use diag::*;` so every existing `commands::*` path still resolves
//! unchanged (Tauri `generate_handler!` in lib.rs references these paths).
//! Named `diag` to avoid confusion with the crate-level `crate::diagnostics`
//! module this delegates to.

// Bring in the parent module's imports and shared helpers (to_str_err,
// Serialize, etc.). Parent-private items are visible here.
use super::*;

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

    let data_dir = crate::data_dir();
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
    // If the directory can't be created the reveal cannot succeed, so surface
    // the error instead of spawning the file manager on a non-existent path
    // (`explorer.exe` spawns successfully regardless and would mask the
    // failure, leaving the UI to report a phantom success).
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = ?e, "open_logs_folder: create_dir_all failed");
        return Err(to_str_err(e));
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
