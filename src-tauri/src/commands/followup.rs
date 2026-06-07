//! Inline review comments → follow-up to the same agent (feature #7).
//!
//! The human annotates the review diff with per-file/per-line comments; this
//! command compiles them (plus an optional freeform note) into ONE message and
//! both (a) records it as a request-changes decision via the shared review core
//! — so it lands durably in the task body (the agent's resume channel) and the
//! #8 timeline — and (b) delivers it to the SAME agent: typed into the live PTY
//! if the session is still running, otherwise by resuming the conversation with
//! the message as the follow-up prompt.

use super::*;
use serde::Deserialize;

/// One inline comment anchored to a file (and optionally a new-side line).
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewComment {
    pub file: String,
    #[serde(default)]
    pub line: Option<u32>,
    pub body: String,
}

#[derive(Debug, Serialize)]
pub struct FollowupResult {
    /// How the message reached the agent: "live" (typed into the running PTY)
    /// or "resume" (relaunched with the follow-up prompt).
    pub delivery: String,
    /// The task's estado after recording the request-changes decision.
    pub new_estado: String,
}

/// Compile inline comments + an optional freeform note into one agent-facing
/// message. The header is localized; the comment list is a stable bullet form.
fn compile_followup(state: &AppState, comments: &[ReviewComment], note: Option<&str>) -> String {
    let header = match state.i18n.lock() {
        Ok(i18n) => i18n.t("review-followup-header"),
        Err(_) => "Please address the following review feedback:".to_string(),
    };
    let mut s = String::new();
    s.push_str(&header);
    s.push('\n');
    for c in comments {
        match c.line {
            Some(l) => s.push_str(&format!("- {}:{} — {}\n", c.file, l, c.body)),
            None => s.push_str(&format!("- {} — {}\n", c.file, c.body)),
        }
    }
    if let Some(n) = note {
        let n = n.trim();
        if !n.is_empty() {
            s.push('\n');
            s.push_str(n);
        }
    }
    s
}

/// Send the human's inline review comments to the task's agent (feature #7).
#[tauri::command]
pub async fn send_review_followup(
    state: State<'_, Arc<AppState>>,
    task_id: String,
    comments: Vec<ReviewComment>,
    note: Option<String>,
) -> Result<FollowupResult, String> {
    let note_has_text = note
        .as_deref()
        .map(str::trim)
        .is_some_and(|n| !n.is_empty());
    if comments.is_empty() && !note_has_text {
        return Err("no comments to send".to_string());
    }

    // Must have a recorded run to follow up on (agent + model + ids).
    let run = state
        .task_runs
        .get(&task_id)
        .ok_or_else(|| "no agent run recorded for this task".to_string())?;

    let compiled = compile_followup(&state, &comments, note.as_deref());

    // 1. Record as a request-changes decision through the shared core: durable
    //    `[revisão]` log line (the agent's resume channel), RevisaoDecidida
    //    timeline event, and estado → fazendo. Reuses the exact transition the
    //    plain "Pedir alterações" button uses.
    use cadenza_proto::ops::review_decision::Verdict;
    let new_estado = crate::review::apply_review_decision(
        state.repo.as_ref(),
        &task_id,
        Verdict::PedirAlteracoes,
        &compiled,
    )
    .await
    .map_err(|e| e.message)?;

    // 2. Deliver to the SAME agent. Prefer the LIVE PTY (same conversation, any
    //    agent); fall back to resuming the conversation with the follow-up.
    //
    //    The decision above is ALREADY durably committed (estado→fazendo +
    //    `[revisão]` log line the agent reads on its next run), so any delivery
    //    failure below is recoverable via "Continuar" — we report it honestly
    //    rather than failing the whole command or claiming false success.
    let live_session = run
        .last_session_id
        .as_ref()
        .and_then(|sid| state.sessions.lock().ok().and_then(|s| s.get(sid).cloned()));

    let delivery = if let Some(session) = live_session {
        // Agent still running — type the message into its TUI. AWAIT the write
        // so a broken/closed pipe is reported (`live_failed`) instead of being
        // swallowed and shown as success. No boot delay: the agent is already up.
        match deliver_to_live(&session, &compiled).await {
            Ok(()) => "live",
            Err(e) => {
                tracing::warn!(error = %e, task = %task_id, "live follow-up delivery failed; decision recorded, use Continuar");
                "live_failed"
            }
        }
    } else {
        // No live session — resume the conversation, delivering the compiled
        // message as the follow-up prompt. Resume only carries the SAME
        // conversation when conversation_id was captured; otherwise it is a
        // FRESH start that still receives the comments (appended). Report which
        // actually happened instead of always claiming "resume".
        let res = start_task_agent(
            state,
            task_id.clone(),
            run.agent,
            run.model.clone(),
            Some(TaskAgentMode::Execute),
            Some(false),
            Some(compiled),
        )
        .await
        .map_err(|e| format!("comments recorded; could not start the agent: {e}"))?;
        if res.resumed {
            "resume"
        } else {
            "fresh"
        }
    };

    Ok(FollowupResult {
        delivery: delivery.to_string(),
        new_estado: new_estado.as_str().to_string(),
    })
}

/// Type a follow-up message into a still-running agent's PTY and submit it
/// (text, a short settle, then Enter). Returns the write outcome so the caller
/// can report a broken pipe rather than silently no-op. No boot delay — unlike
/// `send_initial_prompt`, the agent is already running.
async fn deliver_to_live(
    session: &Arc<crate::terminal::TerminalSession>,
    msg: &str,
) -> Result<(), String> {
    session.write(msg.as_bytes()).map_err(to_str_err)?;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    session.write(b"\r").map_err(to_str_err)?;
    Ok(())
}
