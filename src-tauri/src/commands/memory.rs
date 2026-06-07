//! Per-project shared-memory command handlers — split out of the `commands`
//! god-module. Pure relocation. Re-exported via `commands`'s
//! `pub use memory::*;` so every existing `commands::*_memory*` path still
//! resolves unchanged (Tauri `generate_handler!` in lib.rs references these
//! paths).
//!
//! A memória oficial é uma lista curada de itens por projeto. O usuário é
//! o curador: edita itens manualmente, promove aprendizados sugeridos pelo
//! agente de execução (no review da task) e aprova/rejeita ops de reeval.
//! Nada gerado por agente entra na memória sem passar por aqui.

// Bring in the parent module's imports and shared helpers (AppState,
// to_str_err, send_initial_prompt, wait_for_codex_uuid, StartTaskAgentResult,
// AgenteKind, LaunchPlan, PromptDelivery, PtyHandle, TerminalSession, agent,
// MemoryItem, MemorySuggestion, SuggestionKind, StoreError, FluentArgs, I18n,
// etc.). Parent-private items are visible to this child module.
use super::*;

fn emit_memory_changed(state: &AppState, project_id: &str) {
    if let Some(app) = state.app_handle.lock().ok().and_then(|h| h.clone()) {
        let _ = app.emit(cadenza_proto::ops::EV_MEMORY_CHANGED, project_id);
    }
}

#[tauri::command]
pub async fn get_project_memory(
    state: State<'_, Arc<AppState>>,
    project_id: String,
) -> Result<Vec<MemoryItem>, String> {
    state
        .repo
        .list_memory(&project_id)
        .await
        .map_err(to_str_err)
}

/// Adiciona um item à memória oficial (edição manual do usuário). O
/// backend gera o id estável (`M-<uuid>`) e o timestamp.
#[tauri::command]
pub async fn add_memory_item(
    state: State<'_, Arc<AppState>>,
    project_id: String,
    texto: String,
) -> Result<MemoryItem, String> {
    let texto = texto.trim();
    if texto.is_empty() {
        return Err("texto is required".to_string());
    }
    let item = MemoryItem {
        id: format!("M-{}", Uuid::new_v4().simple()),
        texto: texto.to_string(),
        origem_task: None,
        criado_em: chrono::Utc::now().timestamp_millis(),
    };
    state
        .repo
        .add_memory_item(&project_id, &item)
        .await
        .map_err(to_str_err)?;
    emit_memory_changed(&state, &project_id);
    Ok(item)
}

#[tauri::command]
pub async fn update_memory_item(
    state: State<'_, Arc<AppState>>,
    project_id: String,
    item_id: String,
    texto: String,
) -> Result<(), String> {
    let texto = texto.trim();
    if texto.is_empty() {
        return Err("texto is required".to_string());
    }
    state
        .repo
        .update_memory_item(&project_id, &item_id, texto)
        .await
        .map_err(to_str_err)?;
    emit_memory_changed(&state, &project_id);
    Ok(())
}

#[tauri::command]
pub async fn delete_memory_item(
    state: State<'_, Arc<AppState>>,
    project_id: String,
    item_id: String,
) -> Result<(), String> {
    state
        .repo
        .delete_memory_item(&project_id, &item_id)
        .await
        .map_err(to_str_err)?;
    emit_memory_changed(&state, &project_id);
    Ok(())
}

#[tauri::command]
pub async fn list_memory_suggestions(
    state: State<'_, Arc<AppState>>,
    project_id: String,
) -> Result<Vec<MemorySuggestion>, String> {
    state
        .repo
        .list_memory_suggestions(&project_id)
        .await
        .map_err(to_str_err)
}

/// Resolve uma sugestão pendente: `aprovar=true` aplica a op à memória
/// oficial e remove a sugestão; `aprovar=false` apenas descarta. Tudo é
/// curadoria explícita do usuário — o agente nunca chega aqui.
///
/// `Contradicao` é informativa: aprovar não muda a memória (o usuário
/// resolve editando), então tratamos qualquer resolução como descarte.
#[tauri::command]
pub async fn resolve_memory_suggestion(
    state: State<'_, Arc<AppState>>,
    suggestion_id: String,
    aprovar: bool,
) -> Result<(), String> {
    let suggestion = state
        .repo
        .read_memory_suggestion(&suggestion_id)
        .await
        .map_err(to_str_err)?
        .ok_or_else(|| format!("memory suggestion '{suggestion_id}' not found"))?;
    let project_id = suggestion.project_id.clone();

    // Claim the suggestion by removing it *first*, so a concurrent resolve
    // (double-click, a second window) can't apply the same op twice — the
    // `mint`-based ops below add a fresh `M-<uuid>` on every call and are
    // not idempotent. If it's already gone, another caller handled it.
    match state.repo.delete_memory_suggestion(&suggestion_id).await {
        Ok(()) => {}
        Err(StoreError::NotFound(_)) => return Ok(()),
        Err(e) => return Err(to_str_err(e)),
    }
    if aprovar {
        apply_memory_suggestion(&state, &project_id, &suggestion.kind).await?;
    }
    emit_memory_changed(&state, &project_id);
    Ok(())
}

/// Muta a memória oficial conforme a op aprovada. Não remove a sugestão —
/// isso é responsabilidade do chamador (`resolve_memory_suggestion`).
async fn apply_memory_suggestion(
    state: &AppState,
    project_id: &str,
    kind: &SuggestionKind,
) -> Result<(), String> {
    let mint = |texto: &str, origem_task: Option<String>| MemoryItem {
        id: format!("M-{}", Uuid::new_v4().simple()),
        texto: texto.to_string(),
        origem_task,
        criado_em: chrono::Utc::now().timestamp_millis(),
    };
    match kind {
        SuggestionKind::Aprendizado { texto, origem_task } => {
            let item = mint(texto, origem_task.clone());
            state
                .repo
                .add_memory_item(project_id, &item)
                .await
                .map_err(to_str_err)?;
        }
        SuggestionKind::Nova { texto } => {
            let item = mint(texto, None);
            state
                .repo
                .add_memory_item(project_id, &item)
                .await
                .map_err(to_str_err)?;
        }
        SuggestionKind::Remover { target_id } => {
            // A target that's already gone is the desired end-state — tolerate
            // it (as Mesclar does) so approving a stale op doesn't error and
            // wedge the suggestion in the queue.
            match state.repo.delete_memory_item(project_id, target_id).await {
                Ok(()) => {}
                Err(StoreError::NotFound(_)) => {
                    tracing::warn!(target = %target_id, "remover: target already absent");
                }
                Err(e) => return Err(to_str_err(e)),
            }
        }
        SuggestionKind::Reescrever {
            target_id,
            novo_texto,
        } => {
            // Rewriting an item that's already gone is moot — tolerate it
            // rather than erroring so the suggestion still clears.
            match state
                .repo
                .update_memory_item(project_id, target_id, novo_texto)
                .await
            {
                Ok(()) => {}
                Err(StoreError::NotFound(_)) => {
                    tracing::warn!(target = %target_id, "reescrever: target already absent");
                }
                Err(e) => return Err(to_str_err(e)),
            }
        }
        SuggestionKind::Mesclar {
            target_ids,
            texto_mesclado,
        } => {
            // Cria o item fundido primeiro; só então remove os originais,
            // para que uma falha no meio não perca todo o conteúdo.
            let item = mint(texto_mesclado, None);
            state
                .repo
                .add_memory_item(project_id, &item)
                .await
                .map_err(to_str_err)?;
            for target in target_ids {
                if let Err(e) = state.repo.delete_memory_item(project_id, target).await {
                    // Item já removido / inexistente não é fatal — a fusão
                    // já registrou o texto consolidado.
                    tracing::warn!(error = ?e, target = %target, "merge: delete target failed");
                }
            }
        }
        SuggestionKind::Contradicao { .. } => {
            // Informativa — aprovar não altera a memória.
        }
    }
    Ok(())
}

/// Dispara um agente em PTY na pasta do projeto para **reavaliar** a
/// memória oficial. Espelha `destrinchar_ideia`: resolve projeto + cwd,
/// planeja o comando do agente, seta `CADENZA_MEMORY_REEVAL=1` e registra
/// a sessão. O agente lê a memória atual e emite sugestões de reeval via
/// `cadenza-cli memory revise` — nada é aplicado automaticamente.
#[tauri::command]
pub async fn reavaliar_memoria(
    state: State<'_, Arc<AppState>>,
    project_id: String,
    agent_kind: AgenteKind,
    model: String,
) -> Result<StartTaskAgentResult, String> {
    // 1. Resolver projeto + cwd + override de comando do agente.
    let (cwd, command_override) = {
        let cfg = state.config.lock().map_err(to_str_err)?;
        let project = cfg
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .ok_or_else(|| format!("project '{project_id}' not found in config"))?;
        let project_path = project.path.clone();
        let cmd = project
            .agente
            .as_ref()
            .filter(|a| a.kind == agent_kind)
            .and_then(|a| a.command.clone())
            .or_else(|| {
                cfg.agente
                    .as_ref()
                    .filter(|a| a.kind == agent_kind)
                    .and_then(|a| a.command.clone())
            });
        (project_path, cmd)
    };

    if !cwd.exists() {
        return Err(format!(
            "project path does not exist: {} — fix it in Settings → Projetos",
            cwd.display()
        ));
    }

    // 2. Plan + env. Sempre fresh; sempre há prompt inicial.
    let synthetic_task_id = format!("MEMORY-{project_id}");
    let prompt = render_memory_reeval_prompt(&state.i18n, &project_id);
    let plan: LaunchPlan = agent::plan_launch(
        agent_kind,
        &model,
        command_override.as_deref(),
        &cwd,
        &synthetic_task_id,
        &project_id,
        None,
        Some(&prompt),
    );
    let LaunchPlan {
        spawn,
        conversation_id_known,
        pending_codex_capture,
        pending_opencode_capture: _,
        prompt_delivery,
    } = plan;
    let spawn = spawn.memory_reeval_env(&project_id);

    // 3. Spawn PTY + registrar sessão.
    let pty = PtyHandle::spawn(spawn).map_err(|e| {
        format!("failed to start agent: {e}. Is the CLI installed and on PATH? You can override the binary path in Settings.")
    })?;
    let session_id = format!("S-{}", Uuid::new_v4().simple());
    let session = TerminalSession::start(session_id.clone(), pty).map_err(to_str_err)?;
    state
        .sessions
        .lock()
        .map_err(to_str_err)?
        .insert(session_id.clone(), session.clone());
    tracing::info!(
        project = %project_id, agent = ?agent_kind, model = %model,
        session = %session_id, "memory reeval agent started"
    );

    // 3a. Entrega do prompt inicial (argv ou type-in), igual aos demais.
    if prompt_delivery == PromptDelivery::TypeIn {
        let session_for_prompt = session.clone();
        tauri::async_runtime::spawn(async move {
            send_initial_prompt(&session_for_prompt, &prompt).await;
        });
    }

    if let Some(capture) = pending_codex_capture {
        tauri::async_runtime::spawn(async move {
            let _ = wait_for_codex_uuid(capture).await;
        });
    }

    Ok(StartTaskAgentResult {
        session_id,
        conversation_id: conversation_id_known,
        resumed: false,
    })
}

/// Build the project-memory block appended to a fresh execution prompt.
/// One bullet per curated item. Returns a leading-blank-line-separated
/// section so it reads as its own block after the base prompt.
///
/// `pub(crate)` because the shared `start_task_agent` flow (still in
/// `mod.rs`) and the `commands::tests` unit tests reach it through the
/// `pub use memory::*;` re-export.
pub(crate) fn render_memory_block(i18n_slot: &Mutex<I18n>, items: &[MemoryItem]) -> String {
    let bullets = items
        .iter()
        .map(|i| format!("- {}", i.texto))
        .collect::<Vec<_>>()
        .join("\n");
    let mut args = FluentArgs::new();
    args.set("itens", bullets.clone());
    match i18n_slot.lock() {
        Ok(i18n) => format!("\n\n{}", i18n.t_with("agent-memory-block", Some(&args))),
        Err(_) => format!(
            "\n\nProject memory — durable facts, decisions and conventions for this project:\n{bullets}"
        ),
    }
}

fn render_memory_reeval_prompt(i18n_slot: &Mutex<I18n>, project_id: &str) -> String {
    let mut args = FluentArgs::new();
    args.set("project_id", project_id.to_string());
    match i18n_slot.lock() {
        Ok(i18n) => i18n.t_with("agent-initial-prompt-memory-reeval", Some(&args)),
        Err(_) => format!(
            "Use the `cadenza` skill to coordinate with Cadenza. You are in MEMORY REEVALUATION mode for project {project_id}. Read the current memory with `cadenza-cli memory list --json` and emit review suggestions (remove obsolete, merge duplicates, rewrite confusing items, flag contradictions, propose new) via `cadenza-cli memory revise --op ...`. Do not change anything directly — the human curates."
        ),
    }
}
