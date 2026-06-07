//! Attachment command handlers — split out of the `commands` god-module.
//! Pure relocation: the image save/read commands plus their UI-coupled error
//! mapping and the base64 transfer struct. Re-exported via `commands`'s
//! `pub use attachments::*;` so `commands::save_attachment` /
//! `commands::read_attachment` / `commands::AttachmentData` still resolve
//! unchanged (Tauri `generate_handler!` in lib.rs references these paths).
//!
//! NOTE: the crate-level `crate::attachments` module (the actual storage
//! layer) is distinct from this `commands::attachments` submodule. All
//! references below use the absolute `crate::attachments::` path, so there is
//! no clash with `use super::*;`.

// Bring in the parent module's imports and shared helpers (Serialize, etc.).
use super::*;

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
