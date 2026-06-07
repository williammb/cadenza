//! Human approve / request-changes decision transition (PLAN §E.16).
//!
//! The shared core for both the NDJSON `review_decision_op` (CLI/agent
//! surface) and the `review_decision` Tauri command (webview surface). The
//! transition guard lives here, in the review domain, so the command layer
//! only adapts arguments and maps the typed error — no business logic in the
//! handler. Takes an explicit `&dyn Repository`; it never depends on
//! `AppState`.

use cadenza_proto::Estado;

use crate::store::{Repository, StoreError};

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

    // Run timeline (feature #8): emit in the shared core so BOTH callers (the
    // webview command and the IPC op) are covered exactly once. The core stays
    // AppState-free — it records through the same `&dyn Repository`.
    crate::audit::record(
        repo,
        Some(task_id.to_string()),
        cadenza_proto::RunEventKind::RevisaoDecidida {
            verdict: label.to_string(),
            nota: (!note.trim().is_empty()).then(|| note.to_string()),
            novo_estado: Some(target_estado.as_str().to_string()),
        },
    )
    .await;

    Ok(target_estado)
}

fn map_decision_store_err(e: StoreError) -> ReviewDecisionError {
    match e {
        StoreError::NotFound(id) => ReviewDecisionError::new("task_not_found", id),
        StoreError::Busy => ReviewDecisionError::new("task_busy", e.to_string()),
        other => ReviewDecisionError::new("internal", other.to_string()),
    }
}
