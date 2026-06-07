//! Triage proposal command handlers — split out of the `commands`
//! god-module. Pure relocation. Re-exported via `commands`'s
//! `pub use proposals::*;` so every existing `commands::*` path still
//! resolves unchanged (Tauri `generate_handler!` in lib.rs references these
//! paths). `create_task_from_proposta` is `pub(crate)` so the Jira flow
//! (`commands/jira.rs`) reaches it via `super::`; `proposta_to_body` is
//! `pub(crate)` so the `mod.rs` test block reaches it via `super::`.

// Bring in the parent module's imports and shared helpers (AppState,
// to_str_err, enrich_proposta, emit_tasks_changed, mint_next_task_id,
// Decisao, DecisaoRegistro, Estado, NewProposta, Proposta, Task, State,
// Arc, Duration, etc.). Parent-private items are visible here.
use super::*;

// ───────────────────────── triage ─────────────────────────

#[tauri::command]
pub async fn list_pending_propostas(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<Proposta>, String> {
    let propostas = state
        .repo
        .list_pending_propostas()
        .await
        .map_err(to_str_err)?;
    Ok(propostas
        .into_iter()
        .map(|p| enrich_proposta(&state, p))
        .collect())
}

#[tauri::command]
pub async fn read_proposta(
    state: State<'_, Arc<AppState>>,
    proposta_id: String,
) -> Result<Option<Proposta>, String> {
    let proposta = state
        .repo
        .read_proposta(&proposta_id)
        .await
        .map_err(to_str_err)?;
    Ok(proposta.map(|p| enrich_proposta(&state, p)))
}

#[tauri::command]
pub async fn read_decisao(
    state: State<'_, Arc<AppState>>,
    proposta_id: String,
) -> Result<Option<DecisaoRegistro>, String> {
    state
        .repo
        .read_decisao(&proposta_id)
        .await
        .map_err(to_str_err)
}

/// Persist a decision — frontend calls this from the modal or the
/// notification action handler.
///
/// When the decision is `Aceita` and no `task_id` was supplied, we
/// materialize the derived task here and stamp its id into the registro
/// before persisting. Doing it backend-side keeps create+decision atomic:
/// the UI can't crash between the two steps and leave a proposal accepted
/// without a task. (`Mesclada` carries an existing `task_id`; `Rejeitada`
/// keeps `task_id = None` — neither creates anything.)
#[tauri::command]
pub async fn decidir_proposta(
    state: State<'_, Arc<AppState>>,
    mut registro: DecisaoRegistro,
) -> Result<(), String> {
    if registro.decisao == Decisao::Aceita && registro.task_id.is_none() {
        // Serializa read→create→write para que um duplo-clique não deixe
        // duas chamadas concorrentes lerem "sem decisão" e materializarem
        // duas tasks. O segundo a entrar enxerga a decisão do primeiro e
        // reaproveita a task. (Re-tentativa após crash entre create e
        // write ainda pode duplicar — fechar essa janela exige persistir
        // create+decisão numa transação, o que depende do backend.)
        let _guard = state.decision_lock.lock().await;
        let existing = state
            .repo
            .read_decisao(&registro.proposta_id)
            .await
            .map_err(to_str_err)?
            .and_then(|d| d.task_id);
        let task_id = match existing {
            Some(id) => id,
            None => create_task_from_proposta(&state, &registro.proposta_id).await?,
        };
        registro.task_id = Some(task_id);
        return state.repo.write_decisao(registro).await.map_err(to_str_err);
    }
    state.repo.write_decisao(registro).await.map_err(to_str_err)
}

/// Materialize the derived task for an accepted proposal and return its
/// new `T-<n>` id. The project is inherited from the proposal's `parent`
/// task (via the task→project mapping), falling back to the active
/// project; errors when neither is known, since `create_task` requires a
/// valid project.
pub(crate) async fn create_task_from_proposta(
    state: &AppState,
    proposta_id: &str,
) -> Result<String, String> {
    let proposta = state
        .repo
        .read_proposta(proposta_id)
        .await
        .map_err(to_str_err)?
        .ok_or_else(|| format!("proposta not found: {proposta_id}"))?;

    // Projeto: herda do parent, senão usa o projeto ativo do config.
    let project_id = proposta
        .parent
        .as_deref()
        .and_then(|p| state.task_projects.get(p))
        .or_else(|| {
            state
                .config
                .lock()
                .ok()
                .and_then(|cfg| cfg.active_project_id.clone())
        })
        .ok_or_else(|| {
            "cannot create derived task: proposta has no parent project and no active project is set"
                .to_string()
        })?;

    // Mesmo guard de `create_task`: o projeto precisa existir no config
    // (pode ter sido removido entre a proposta e a aceitação).
    {
        let cfg = state.config.lock().map_err(to_str_err)?;
        if !cfg.projects.iter().any(|p| p.id == project_id) {
            return Err(format!("unknown project_id: {project_id}"));
        }
    }

    // Mint a sequential T-<n>, matching the in-app and CLI create paths.
    let task_id = mint_next_task_id(state.repo.as_ref()).await?;

    let task = Task {
        id: task_id.clone(),
        titulo: proposta.title.clone(),
        estado: Estado::AFazer,
        responsavel: "humano".to_string(),
        body: proposta_to_body(&proposta),
        worktree_path: None,
        branch: None,
        blocked_by: Vec::new(),
        jira_site: proposta.jira_site.clone(),
        jira_issue_id: proposta.jira_issue_id.clone(),
        jira_key_display: None,
    };
    state.repo.create_task(&task).await.map_err(to_str_err)?;
    // File backend has no Jira columns on the task row, so persist identity
    // to the `task-jira.json` sidecar. On SQL backends the row already
    // carries it (create_task above); the sidecar is harmless redundancy
    // and keeps `enrich_task` backend-agnostic.
    if let (Some(site), Some(issue)) = (&task.jira_site, &task.jira_issue_id) {
        state
            .task_jira
            .set(&task_id, site, issue)
            .map_err(to_str_err)?;
    }
    state
        .task_projects
        .set(&task_id, Some(&project_id))
        .map_err(to_str_err)?;
    // Slice 4: ensure the issue's shared worktree exists and associate this
    // task with it. Runs under `decision_lock` (held by `decidir_proposta`),
    // but that lock is global, not per-issue, so `ensure_issue_worktree` also
    // takes a per-issue guard + persisted reservation to converge re-accepts
    // of different propostas for the same issue onto one worktree. This is
    // best-effort for the accept path: a worktree failure must not fail task
    // creation (the task exists; the worktree can be retried at agent start),
    // so a failure is logged, not propagated.
    if let (Some(site), Some(issue)) = (&task.jira_site, &task.jira_issue_id) {
        // The record (seeded by prior slices) carries the canonical
        // display key; use it as the branch-name source. Fall back to the
        // task title when the record can't be read.
        let summary = match state.repo.read_jira_issue(site, issue).await {
            Ok(Some(rec)) => rec.jira_key,
            _ => task.titulo.clone(),
        };
        if let Err(e) = crate::jira::worktree::ensure_issue_worktree(
            state,
            site,
            issue,
            &project_id,
            &task_id,
            &summary,
        )
        .await
        {
            tracing::warn!(
                error = %e, jira_site = %site, jira_issue_id = %issue, task = %task_id,
                "ensure_issue_worktree failed during accept; task created without worktree"
            );
        }
    }
    emit_tasks_changed(state, &task_id);
    Ok(task_id)
}

/// Render an accepted proposal into the derived task's markdown body so
/// the task keeps the full context the agent reported. Mirrors the fields
/// shown in the triage modal (pt-BR primary locale).
pub(crate) fn proposta_to_body(p: &Proposta) -> String {
    let mut body = String::new();
    let file = p.file.trim();
    if !file.is_empty() {
        body.push_str(&format!("**Arquivo:** {file}\n\n"));
    }
    body.push_str(&format!("## Como reproduzir\n{}\n\n", p.repro.trim()));
    body.push_str(&format!("## O que falhou\n{}\n\n", p.what_failed.trim()));
    body.push_str(&format!("## Ação proposta\n{}\n", p.action.trim()));
    body.push_str(&format!("\n---\nDerivada da proposta {}.\n", p.proposta_id));
    body
}

/// Used by the CLI's `propose` path (will go through the NDJSON socket
/// in Phase 4 — this Tauri-side variant is for in-app testing / tooling).
#[tauri::command]
pub async fn propose(
    state: State<'_, Arc<AppState>>,
    args: NewProposta,
) -> Result<Proposta, String> {
    // Hardening (Slice 2 §C): the public propose surface must not let a
    // caller forge a Jira identity. Only `jira_materialize` (which stamps
    // identity server-side from a verified capability secret) may set
    // these. Mirrors the IPC `OP_PROPOSE` guard.
    if args.jira_site.is_some() || args.jira_issue_id.is_some() {
        return Err("jira_site/jira_issue_id may only be set via jira_materialize".to_string());
    }
    state.repo.propose(args).await.map_err(to_str_err)
}

#[tauri::command]
pub async fn await_proposta_decisao(
    state: State<'_, Arc<AppState>>,
    proposta_id: String,
    timeout_ms: u64,
) -> Result<Option<DecisaoRegistro>, String> {
    state
        .repo
        .await_decisao(&proposta_id, Duration::from_millis(timeout_ms))
        .await
        .map_err(to_str_err)
}
