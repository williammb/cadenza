//! Run timeline / audit event log (feature #8) — emit helpers.
//!
//! The store layer ([`crate::store::Repository::append_event`]) is the durable
//! sink; this module is the thin emit side every call site uses.
//!
//! **Emitting an audit event is BEST-EFFORT.** A failure to persist an event
//! is logged and swallowed — it must never fail the user action it records (a
//! `done` / review / proposal decision has to succeed even if the audit sink
//! hiccups). Ids are uuid v4 (prefixed `E-`); timestamps are epoch-ms minted
//! here so the wire type stays clock/uuid-free.

use crate::store::Repository;
use cadenza_proto::{RunEvent, RunEventKind};

/// Current wall-clock in epoch-ms (mirrors the server-side timestamp
/// convention used elsewhere, e.g. `ipc.rs`).
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Best-effort snake_case tag for any serde enum that serializes to a bare
/// string (e.g. `AgenteKind`, `Decisao`, `TaskAgentMode`). Falls back to the
/// `Debug` form if the value doesn't serialize to a string.
pub fn serde_tag<T: serde::Serialize + std::fmt::Debug>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|j| j.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{v:?}"))
}

/// Build a [`RunEvent`] with a fresh id + current timestamp.
pub fn event(task_id: Option<String>, kind: RunEventKind) -> RunEvent {
    RunEvent::new(
        format!("E-{}", uuid::Uuid::new_v4()),
        now_ms(),
        task_id,
        kind,
    )
}

/// Append an event, best-effort: logs and swallows any store error so the
/// caller's primary action is never affected by the audit write.
pub async fn record(repo: &dyn Repository, task_id: Option<String>, kind: RunEventKind) {
    let ev = event(task_id, kind);
    if let Err(e) = repo.append_event(&ev).await {
        tracing::warn!(error = %e, kind = ev.kind_tag(), "failed to append audit event");
    }
}
