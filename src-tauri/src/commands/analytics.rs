//! Run timeline / analytics commands (feature #8).
//!
//! Read surface over the append-only event log. `list_run_events` powers the
//! timeline view; `get_run_analytics` aggregates the same events into the
//! headline counts the UI shows. Both are thin: all persistence lives in the
//! store layer (`Repository::list_events` / `all_events`).

use super::*;
use cadenza_proto::{RunEvent, RunEventKind};
use std::collections::HashMap;

/// Events for the run timeline, oldest-first. `task_id` scopes to one task;
/// `limit` keeps only the most-recent N (still oldest-first).
#[tauri::command]
pub async fn list_run_events(
    state: State<'_, Arc<AppState>>,
    task_id: Option<String>,
    limit: Option<i64>,
) -> Result<Vec<RunEvent>, String> {
    state
        .repo
        .list_events(task_id.as_deref(), limit)
        .await
        .map_err(to_str_err)
}

/// Aggregate headline metrics over the whole event log. A consumer of the
/// foundation — not a second source of truth.
#[derive(Debug, Default, Serialize)]
pub struct RunAnalytics {
    pub total_events: usize,
    /// Count per event kind tag (`agente_iniciado`, `done_enviado`, …).
    pub by_kind: HashMap<String, usize>,
    /// `agente_iniciado` counts per agent tag (`claude_code`, `codex`, …).
    pub by_agent: HashMap<String, usize>,
    pub proposals_aceitas: usize,
    pub proposals_rejeitadas: usize,
    pub proposals_mescladas: usize,
    pub reviews_aprovados: usize,
    pub reviews_alteracoes: usize,
    /// Measured token totals (feature #1), summed from the LATEST usage
    /// observation per task so a resumed (cumulative) conversation isn't
    /// double-counted. Zero when no run reported usage.
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
}

#[tauri::command]
pub async fn get_run_analytics(state: State<'_, Arc<AppState>>) -> Result<RunAnalytics, String> {
    let events = state.repo.all_events().await.map_err(to_str_err)?;
    let mut a = RunAnalytics {
        total_events: events.len(),
        ..Default::default()
    };
    // Usage is cumulative per conversation, so a task's LATEST UsoObservado is
    // its current total. Keep last-per-task (events are insertion-ordered),
    // then sum — summing every observation would double-count resumes.
    let mut latest_usage: HashMap<(String, String), cadenza_proto::UsageObservation> =
        HashMap::new();
    for ev in &events {
        *a.by_kind.entry(ev.kind_tag().to_string()).or_insert(0) += 1;
        match &ev.kind {
            RunEventKind::AgenteIniciado { agente, .. } => {
                *a.by_agent.entry(agente.clone()).or_insert(0) += 1;
            }
            RunEventKind::PropostaDecidida { decisao, .. } => match decisao.as_str() {
                "aceita" => a.proposals_aceitas += 1,
                "rejeitada" => a.proposals_rejeitadas += 1,
                "mesclada" => a.proposals_mescladas += 1,
                _ => {}
            },
            RunEventKind::RevisaoDecidida { verdict, .. } => match verdict.as_str() {
                "aprovado" => a.reviews_aprovados += 1,
                "pedir_alteracoes" => a.reviews_alteracoes += 1,
                _ => {}
            },
            RunEventKind::UsoObservado {
                usage,
                conversation_id,
            } => {
                // Key by (task_id, conversation_id): the same conversation
                // re-observed (cumulative) collapses to its LATEST value, while
                // DISTINCT conversations on the same task each survive and are
                // summed below. Fall back to the event id when either is absent.
                let task = ev.task_id.clone().unwrap_or_else(|| ev.id.clone());
                let conv = conversation_id.clone().unwrap_or_else(|| ev.id.clone());
                latest_usage.insert((task, conv), usage.clone());
            }
            _ => {}
        }
    }
    for usage in latest_usage.values() {
        a.total_input_tokens = a.total_input_tokens.saturating_add(usage.input_tokens);
        a.total_output_tokens = a.total_output_tokens.saturating_add(usage.output_tokens);
        a.total_cache_read_tokens = a
            .total_cache_read_tokens
            .saturating_add(usage.cache_read_tokens);
        a.total_cache_creation_tokens = a
            .total_cache_creation_tokens
            .saturating_add(usage.cache_creation_tokens);
    }
    Ok(a)
}

/// Measured token usage for a task's latest run (feature #1). Claude-only for
/// now; returns `None` ("unavailable") for other agents, no recorded run, or a
/// missing transcript. Live read of the agent's session file — always current.
#[tauri::command]
pub async fn get_task_usage(
    state: State<'_, Arc<AppState>>,
    task_id: String,
) -> Result<Option<cadenza_proto::UsageObservation>, String> {
    let Some(run) = state.task_runs.get(&task_id) else {
        return Ok(None);
    };
    if run.agent != crate::config::AgenteKind::ClaudeCode {
        return Ok(None);
    }
    let Some(conv) = run.conversation_id else {
        return Ok(None);
    };
    // Reading + parsing the transcript is blocking disk I/O (a session JSONL can
    // be many MB) — keep it off the runtime thread.
    tauri::async_runtime::spawn_blocking(move || crate::usage::claude_usage(&conv))
        .await
        .map_err(to_str_err)
}
