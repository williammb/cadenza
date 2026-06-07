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
}

#[tauri::command]
pub async fn get_run_analytics(state: State<'_, Arc<AppState>>) -> Result<RunAnalytics, String> {
    let events = state.repo.all_events().await.map_err(to_str_err)?;
    let mut a = RunAnalytics {
        total_events: events.len(),
        ..Default::default()
    };
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
            _ => {}
        }
    }
    Ok(a)
}
