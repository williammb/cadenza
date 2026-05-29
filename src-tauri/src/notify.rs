//! OS notifications via `tauri-plugin-notification`.
//!
//! Titles and bodies are resolved through `cadenza-i18n` using the
//! current `AppState.i18n` locale, so the notification text matches
//! whatever the user picked in the switcher.
//!
//! Per DESIGN-desktop-v2.md § "Notificações nativas":
//! - Action labels are stable keys (`aceitar`/`rejeitar`/`abrir`), the
//!   text comes from `.ftl`. Tauri 2's notification builder doesn't
//!   currently expose action callbacks reliably across all OSes, so for
//!   this phase we emit text-only notifications and document the
//!   action wiring as a Phase 5 follow-up.
//!
//! Wired into Tauri commands in Phase 4 follow-ups; allow dead_code
//! until the first caller lands.
#![allow(dead_code)]

use anyhow::Result;
use cadenza_i18n::FluentArgs;
use cadenza_proto::Proposta;
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::commands::AppState;

/// Show a notification announcing a pending proposal. Uses the current
/// active locale's strings from `notification-proposal-{title,body}`.
pub fn show_proposta_pendente(app: &AppHandle, proposta: &Proposta) -> Result<()> {
    let state = app.state::<std::sync::Arc<AppState>>();
    let i18n = state
        .i18n
        .lock()
        .map_err(|e| anyhow::anyhow!("i18n lock: {e}"))?;

    let title = i18n.t("notification-proposal-title");
    let mut args = FluentArgs::new();
    args.set("task_title", proposta.parent.clone().unwrap_or_default());
    args.set("proposal_title", proposta.title.clone());
    let body = i18n.t_with("notification-proposal-body", Some(&args));
    drop(i18n);

    app.notification()
        .builder()
        .title(title)
        .body(body)
        .show()?;
    tracing::info!(proposta_id = %proposta.proposta_id, "notification shown");
    Ok(())
}

/// Generic informational notification (e.g. "update available").
pub fn show_info(app: &AppHandle, title_key: &str, body_key: &str) -> Result<()> {
    let state = app.state::<std::sync::Arc<AppState>>();
    let i18n = state
        .i18n
        .lock()
        .map_err(|e| anyhow::anyhow!("i18n lock: {e}"))?;
    let title = i18n.t(title_key);
    let body = i18n.t(body_key);
    drop(i18n);
    app.notification()
        .builder()
        .title(title)
        .body(body)
        .show()?;
    Ok(())
}
