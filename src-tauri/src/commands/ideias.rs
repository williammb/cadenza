//! Ideias (Inbox) command handlers — split out of the `commands` god-module.
//! Pure relocation. Re-exported via `commands`'s `pub use ideias::*;` so every
//! existing `commands::*_ideia*` / `commands::list_ideias` path still resolves
//! unchanged (Tauri `generate_handler!` in lib.rs references these paths).
//!
//! Surface paralela à de tasks. Diferentemente das tasks, ideias têm o
//! `project_id` no próprio registro — não dependem do side-mapping.
//! O servidor minta `id` e `created_at_ms` quando ausentes para que
//! a UI possa só preencher `titulo` + `body` + `project_id`.
//!
//! `destrinchar_ideia` reuses the shared `send_initial_prompt` /
//! `wait_for_codex_uuid` helpers that stay in `mod.rs` (they're also used by
//! `start_task_agent` and the Jira flow); it reaches them via `super::`.

// Bring in the parent module's imports and shared helpers (AppState,
// to_str_err, send_initial_prompt, wait_for_codex_uuid, StartTaskAgentResult,
// AgenteKind, LaunchPlan, PromptDelivery, PtyHandle, TerminalSession, agent,
// Ideia, IdeiaStatus, I18n, FluentArgs, etc.). Parent-private items are visible
// to this child module.
use super::*;

fn render_initial_ideia_prompt(i18n_slot: &Mutex<I18n>, ideia_id: &str) -> String {
    let mut args = FluentArgs::new();
    args.set("ideia_id", ideia_id.to_string());
    match i18n_slot.lock() {
        Ok(i18n) => i18n.t_with("agent-initial-prompt-ideia", Some(&args)),
        Err(_) => format!(
            "Use the `cadenza` skill to coordinate with Cadenza through cadenza-cli. Break the ideia {ideia_id} down into actionable tasks."
        ),
    }
}

#[tauri::command]
pub async fn list_ideias(state: State<'_, Arc<AppState>>) -> Result<Vec<Ideia>, String> {
    state.repo.list_ideias().await.map_err(to_str_err)
}

#[tauri::command]
pub async fn read_ideia(
    state: State<'_, Arc<AppState>>,
    id: String,
) -> Result<Option<Ideia>, String> {
    state.repo.read_ideia(&id).await.map_err(to_str_err)
}

#[derive(Debug, Deserialize)]
pub struct NewIdeiaArgs {
    #[serde(default)]
    pub id: Option<String>,
    pub titulo: String,
    #[serde(default)]
    pub body: String,
    pub project_id: String,
}

#[tauri::command]
pub async fn create_ideia(
    state: State<'_, Arc<AppState>>,
    args: NewIdeiaArgs,
) -> Result<Ideia, String> {
    let pid = args.project_id.trim();
    if pid.is_empty() {
        return Err("project_id is required".to_string());
    }
    {
        let cfg = state.config.lock().map_err(to_str_err)?;
        if !cfg.projects.iter().any(|p| p.id == pid) {
            return Err(format!("unknown project_id: {pid}"));
        }
    }
    let id = args
        .id
        .unwrap_or_else(|| format!("I-{}", Uuid::new_v4().simple()));
    let created_at_ms = chrono::Utc::now().timestamp_millis();
    let ideia = Ideia {
        id,
        titulo: args.titulo,
        body: args.body,
        project_id: pid.to_string(),
        status: IdeiaStatus::Pendente,
        created_at_ms,
    };
    state.repo.create_ideia(&ideia).await.map_err(to_str_err)?;
    Ok(ideia)
}

#[tauri::command]
pub async fn delete_ideia(state: State<'_, Arc<AppState>>, id: String) -> Result<(), String> {
    state.repo.delete_ideia(&id).await.map_err(to_str_err)?;
    // Best-effort cleanup of any images embedded in the ideia body.
    crate::attachments::delete_owner("ideias", &id);
    Ok(())
}

#[tauri::command]
pub async fn set_ideia_status(
    state: State<'_, Arc<AppState>>,
    id: String,
    status: IdeiaStatus,
) -> Result<(), String> {
    state
        .repo
        .set_ideia_status(&id, status)
        .await
        .map_err(to_str_err)
}

/// Spawna um agente em PTY na pasta do projeto da ideia, seedando env
/// vars (`CADENZA_IDEIA_ID`, `CADENZA_IDEIA_BODY`) para o agente saber
/// qual ideia destrinchar. O agente roda o skill `cadenza-cli new-task`
/// para criar as tasks resultantes — a UI vê tudo via `tasks_changed`.
///
/// Modelado em `start_task_agent`: mesma sequência de checagens (projeto
/// existe, cwd existe, planejar comando do agente, registrar PTY).
#[tauri::command]
pub async fn destrinchar_ideia(
    state: State<'_, Arc<AppState>>,
    ideia_id: String,
    agent_kind: AgenteKind,
    model: String,
) -> Result<StartTaskAgentResult, String> {
    // 1. Ideia precisa existir.
    let ideia = state
        .repo
        .read_ideia(&ideia_id)
        .await
        .map_err(to_str_err)?
        .ok_or_else(|| format!("ideia '{}' not found", ideia_id))?;

    // 2. Resolver projeto + cwd a partir do `ideia.project_id`.
    let (cwd, command_override) = {
        let cfg = state.config.lock().map_err(to_str_err)?;
        let project = cfg
            .projects
            .iter()
            .find(|p| p.id == ideia.project_id)
            .ok_or_else(|| {
                format!(
                    "project '{}' from ideia not found in config",
                    ideia.project_id
                )
            })?;
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

    // 3. Decomposição é sempre uma nova conversa. Usamos um id sintético
    //    `IDEIA-<id>` no lugar de task_id para que logs e env continuem
    //    fazendo sentido sem precisar entrar em `task-runs.json`.
    let synthetic_task_id = format!("IDEIA-{}", ideia.id);

    // 4. Plan + adiciona env vars específicas da ideia. A decomposição é
    //    sempre fresh, então sempre há um prompt inicial — entregue via
    //    argv quando o agente suporta (igual a `start_task_agent`).
    let prompt = render_initial_ideia_prompt(&state.i18n, &ideia.id);
    let plan: LaunchPlan = agent::plan_launch(
        agent_kind,
        &model,
        command_override.as_deref(),
        &cwd,
        &synthetic_task_id,
        &ideia.project_id,
        None,
        Some(&prompt),
    );
    let LaunchPlan {
        spawn,
        conversation_id_known,
        pending_codex_capture,
        // OpenCode capture is intentionally unused here: idea-decomposition
        // runs are synthetic (IDEIA-*) and never recorded, so there is no run
        // record to patch and nothing to resume. Skipping it also avoids the
        // post-spawn `opencode session list` polling subprocesses entirely.
        pending_opencode_capture: _,
        prompt_delivery,
    } = plan;
    let spawn = spawn.ideia_env(&ideia.id, &ideia.body);

    // 5. Spawn PTY + registrar sessão.
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
        ideia = %ideia.id, agent = ?agent_kind, model = %model,
        session = %session_id, "destrinchar agent started"
    );

    // 5a. Deliver the initial prompt — argv when the agent supports it,
    //     otherwise type it in (same split as start_task_agent).
    if prompt_delivery == PromptDelivery::TypeIn {
        let session_for_prompt = session.clone();
        tauri::async_runtime::spawn(async move {
            send_initial_prompt(&session_for_prompt, &prompt).await;
        });
    }

    // 6. Capturar UUID do Codex se for o caso (mesmo padrão de
    //    `start_task_agent`). Não armazenamos em `task_runs` porque
    //    decomposição é one-shot — não há "continuar" depois.
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
