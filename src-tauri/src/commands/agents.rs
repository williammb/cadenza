//! Agent launch + run-tracking + discovery command handlers — split out of
//! the `commands` god-module. Pure relocation. Re-exported via `commands`'s
//! `pub use agents::*;` so every existing `commands::*` path still resolves
//! unchanged (Tauri `generate_handler!` in lib.rs references these paths;
//! sibling submodules reach `StartTaskAgentResult` through `super::*`).
//!
//! `send_initial_prompt` and `wait_for_codex_uuid` stay in `mod.rs` (they're
//! also used by `ideias.rs`/`jira.rs`); `start_task_agent` reaches them via
//! `super::`. `render_memory_block` lives in `commands/memory.rs`
//! (re-exported, `pub(crate)`) and is reached via `super::render_memory_block`.

// Bring in the parent module's imports and shared helpers (AppState,
// to_str_err, enrich_task, ensure_task_unblocked, prepare_task_workspace,
// send_initial_prompt, wait_for_codex_uuid, render_memory_block, agent,
// AgenteKind, Estado, TaskRun, LaunchPlan, PromptDelivery, PtyHandle,
// TerminalSession, CodexCapture, OpenCodeCapture, FluentArgs, I18n, etc.).
// Parent-private items are visible to this child module.
use super::*;

#[derive(Debug, Serialize)]
pub struct StartTaskAgentResult {
    pub session_id: String,
    pub conversation_id: Option<String>,
    pub resumed: bool,
}

/// Whether `start_task_agent` runs the task or plans it. In `Plan` mode
/// the agent is told to interview the human and persist a refined plan
/// (via `cadenza-cli plan`) instead of implementing; the task stays in
/// `a_fazer` and no run record is kept, so a later `Execute` run is a
/// clean start that reads the saved plan from the task body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskAgentMode {
    #[default]
    Execute,
    Plan,
}

/// RAII reservation for the one-executor-per-issue slot. Created (as
/// `Reserving`) at guard time; `commit(session_id)` flips it to `Live` once
/// the session exists. If dropped without `commit` — any early return or
/// panic during the async spawn window — the still-`Reserving` slot is
/// removed so the issue is never wedged. A `Live` slot is never clobbered.
struct ExecutorReservation {
    state: Arc<AppState>,
    key: (String, String),
    committed: bool,
}

impl ExecutorReservation {
    fn new(state: Arc<AppState>, key: (String, String)) -> Self {
        Self {
            state,
            key,
            committed: false,
        }
    }

    fn commit(mut self, session_id: String) {
        if let Ok(mut active) = self.state.jira_active_executors.lock() {
            active.insert(
                self.key.clone(),
                crate::jira::worktree::ExecutorSlot::Live(session_id),
            );
        }
        self.committed = true;
    }
}

impl Drop for ExecutorReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Ok(mut active) = self.state.jira_active_executors.lock() {
            // Only clear our own `Reserving` slot — never a `Live` one.
            if matches!(
                active.get(&self.key),
                Some(crate::jira::worktree::ExecutorSlot::Reserving)
            ) {
                active.remove(&self.key);
            }
        }
    }
}

/// Launch the configured agent CLI in a PTY
/// running inside the task's project directory. The frontend then
/// calls `pty_attach` with the returned `session_id` to stream output.
///
/// Persists the run in `~/.cadenza/task-runs.json` so a subsequent call
/// for the same `task_id` becomes a resume.
///
/// Returns errors as user-facing strings (the UI surfaces them in a
/// toast). Failure modes the user is expected to fix:
///   - task not found / not in `fazendo`
///   - task has no project mapping (can't decide a cwd)
///   - configured project path doesn't exist
///   - CLI binary not on PATH (and no override in Settings)
#[tauri::command]
pub async fn start_task_agent(
    state: State<'_, Arc<AppState>>,
    task_id: String,
    agent_kind: AgenteKind,
    model: String,
    // Absent/null from older callers → `Execute`.
    mode: Option<TaskAgentMode>,
    // Absent/null from older callers -> false.
    auto_mode: Option<bool>,
    // Feature #7: a caller-supplied follow-up prompt (e.g. compiled review
    // comments). On RESUME it is delivered to the same conversation instead of
    // the usual `None`; on a fresh start it is appended to the task prompt.
    // Absent/null from older callers -> no follow-up (unchanged behavior).
    followup_prompt: Option<String>,
) -> Result<StartTaskAgentResult, String> {
    let mode = mode.unwrap_or_default();
    let auto_mode = auto_mode.unwrap_or(false);
    // 1. Task must exist and not be `feito`. The transition to `fazendo`
    //    (if not already there) happens AFTER a successful spawn — see
    //    step 5b — so a failed start doesn't leave the kanban moved.
    //
    // Enrich the raw row: on the file backend the Jira identity
    // (`jira_site`/`jira_issue_id`) lives in the `task-jira.json` sidecar, not
    // on the task row, so a raw `read_task` reports `None` for both. The
    // one-executor-per-issue guard and the shared-worktree ensure below both
    // key off that identity, so without enrichment they would silently no-op
    // on the file backend (letting two execute agents share one issue
    // worktree). SQL backends carry the columns on the row, so `enrich_task`
    // is a no-op there.
    let task = enrich_task(
        &state,
        state.repo.read_task(&task_id).await.map_err(to_str_err)?,
    );
    if task.estado == Estado::Feito {
        return Err(format!(
            "task '{}' is in state '{}', can't start an agent on a completed task",
            task_id,
            task.estado.as_str()
        ));
    }
    if mode == TaskAgentMode::Plan && task.estado != Estado::AFazer {
        return Err(format!(
            "task '{}' is in state '{}'; plan mode requires the task to be in a_fazer",
            task_id,
            task.estado.as_str()
        ));
    }
    if mode == TaskAgentMode::Execute {
        ensure_task_unblocked(&state, &task_id).await?;
    }
    let original_estado = task.estado;
    let task_titulo = task.titulo.clone();

    // 2. Resolve project + cwd.
    let project_id = state
        .task_projects
        .snapshot()
        .get(&task_id)
        .cloned()
        .ok_or_else(|| {
            format!(
                "task '{}' has no project assigned — assign one in the card menu so the agent has a working directory",
                task_id
            )
        })?;

    let (project_path, command_override) = {
        let cfg = state.config.lock().map_err(to_str_err)?;
        let project = cfg
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .ok_or_else(|| format!("project '{}' not found in config", project_id))?;
        let project_path = project.path.clone();
        // Per-project agente override wins over the global one.
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

    if !project_path.exists() {
        return Err(format!(
            "project path does not exist: {} — fix it in Settings → Projetos",
            project_path.display()
        ));
    }

    // 2b. One-executor-per-issue guard (Slice 4). Only execution runs
    //     contend for the issue's shared worktree; planning is exempt. The
    //     guard refuses a second start while another task for the same Jira
    //     issue has a LIVE agent (registry entry whose session is still in
    //     `state.sessions`); a stale entry (session gone after kill/exit)
    //     is reaped here and the start proceeds. Checked BEFORE any session
    //     is created and before the `Fazendo` transition, so a refusal
    //     leaves no side effects.
    // Held from the guard until just after the session is created, so the
    // slot is claimed atomically (closing the check-then-act TOCTOU); if any
    // step below early-returns, the reservation's Drop clears the slot.
    let mut executor_reservation: Option<ExecutorReservation> = None;
    if mode == TaskAgentMode::Execute {
        if let (Some(site), Some(issue)) = (&task.jira_site, &task.jira_issue_id) {
            let key = (site.clone(), issue.clone());
            {
                let mut active = state.jira_active_executors.lock().map_err(to_str_err)?;
                let sessions = state.sessions.lock().map_err(to_str_err)?;
                if crate::jira::worktree::issue_executor_busy(&active, &sessions, &key) {
                    return Err(
                        "jira_worktree_busy: another task for this Jira issue already has a running agent"
                            .to_string(),
                    );
                }
                // Not busy (no entry, or a stale Live whose session is gone)
                // → claim the slot as Reserving under the held lock so a
                // concurrent start is refused during our async spawn window.
                active.insert(key.clone(), crate::jira::worktree::ExecutorSlot::Reserving);
            }
            executor_reservation = Some(ExecutorReservation::new(Arc::clone(state.inner()), key));
        }
    }

    // 2c. Ensure the issue's shared worktree exists before preparing the
    //     workspace. The accept path creates it best-effort (a failure there
    //     only logs and lets task creation proceed), so the worktree may be
    //     missing — or its record left `Failed`/`Creating` — by the time an
    //     agent starts. `ensure_issue_worktree` is idempotent: it recovers a
    //     half-created worktree, recreates a failed one, and re-associates the
    //     task (writing the `task-worktrees.json` sidecar that
    //     `prepare_task_workspace` reads). This is the "retried at agent
    //     start" the accept path defers to. Unlike accept, a failure HERE is
    //     fatal: running the agent in the bare project repo instead of the
    //     isolated worktree would silently break per-issue isolation, so we
    //     surface the error and let the reservation's Drop release the slot.
    if mode == TaskAgentMode::Execute {
        if let (Some(site), Some(issue)) = (&task.jira_site, &task.jira_issue_id) {
            // Prefer the record's canonical display key for the branch name,
            // matching the accept path; fall back to the task title.
            let summary = match state.repo.read_jira_issue(site, issue).await {
                Ok(Some(rec)) => rec.jira_key,
                _ => task_titulo.clone(),
            };
            crate::jira::worktree::ensure_issue_worktree(
                &state,
                site,
                issue,
                &project_id,
                &task_id,
                &summary,
            )
            .await
            .map_err(to_str_err)?;
        }
    }

    // Prepare the git workspace from the task's declarative config: pull the
    // origin branch, create/switch the destination branch, and create the
    // worktree when requested. A pull or git failure blocks the start with
    // the error surfaced to the caller. `cwd` is the worktree when used,
    // otherwise the project repo.
    let cwd = prepare_task_workspace(&state, &task_id).await?;

    // Run timeline (#6): snapshot the workspace BEFORE the agent runs so a
    // later "revert this run" can rewind to this exact pre-run state. Execute
    // only (plan records no run); best-effort — a checkpoint failure (e.g. cwd
    // not a git repo) must NOT block the start, it just means revert won't be
    // available for this run. The CheckpointCriado event is emitted below once
    // the run is committed.
    let checkpoint: Option<(String, String)> = if mode == TaskAgentMode::Execute {
        let git_ref = format!(
            "refs/cadenza/checkpoints/{task_id}/{}",
            Uuid::new_v4().simple()
        );
        match crate::git::create_checkpoint(&cwd, &git_ref).await {
            Ok(commit) => Some((git_ref, commit)),
            Err(e) => {
                tracing::warn!(error = %e, task = %task_id, "checkpoint at agent start failed; revert unavailable for this run");
                None
            }
        }
    } else {
        None
    };

    // 3. Decide new vs resume from `task-runs.json`. Plan mode always
    //    starts fresh: planning runs are never recorded, and matching an
    //    earlier *execution* conversation would resume the wrong posture.
    let existing = match mode {
        TaskAgentMode::Plan => None,
        TaskAgentMode::Execute => state.task_runs.get(&task_id),
    };
    // Resume only when the saved entry agrees with the user's current
    // choice. Switching agent/model means a new conversation. (Claude
    // can change --model on resume but the agent kind has to match;
    // simpler to start fresh on any change.)
    let existing_conv_id = existing.as_ref().and_then(|r| {
        if r.agent == agent_kind && r.conversation_id.is_some() {
            r.conversation_id.clone()
        } else {
            None
        }
    });
    let resumed = existing_conv_id.is_some();

    // 4. Render the initial prompt (fresh start only) and build the
    //    SpawnConfig via the per-agent planner. Preferred delivery is argv:
    //    the planner bakes the prompt into the command line so the backend
    //    never types into the live PTY (no race with the agent's UI boot).
    let initial_prompt: Option<String> = if resumed {
        // Resume carries ONLY a caller-supplied follow-up (feature #7), if any
        // — the agent already has the task context. Plain "Continuar" (no
        // follow-up) keeps the prior `None` behavior.
        followup_prompt.clone()
    } else {
        let mut prompt = render_initial_task_prompt(&state.i18n, &task_id, &task_titulo, mode);
        // Inject the project's curated memory on a fresh execution start so
        // the agent knows the project's durable facts/decisions/conventions.
        // Omitted when empty (no block) and for planning runs.
        if mode == TaskAgentMode::Execute {
            match state.repo.list_memory(&project_id).await {
                Ok(items) if !items.is_empty() => {
                    prompt.push_str(&render_memory_block(&state.i18n, &items));
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = ?e, project = %project_id, "load project memory for prompt failed")
                }
            }
        }
        // A follow-up on a FRESH start (no conversation to resume — e.g. capture
        // failed) is appended so the agent still sees the comments, even though
        // it's a new conversation.
        if let Some(f) = &followup_prompt {
            prompt.push('\n');
            prompt.push_str(f);
        }
        Some(prompt)
    };
    let plan: LaunchPlan = agent::plan_launch_with_options(
        agent_kind,
        &model,
        command_override.as_deref(),
        &cwd,
        &task_id,
        &project_id,
        existing_conv_id.as_deref(),
        initial_prompt.as_deref(),
        agent::LaunchOptions { auto_mode },
    )?;
    let LaunchPlan {
        spawn,
        conversation_id_known,
        pending_codex_capture,
        mut pending_opencode_capture,
        prompt_delivery,
    } = plan;

    // 4a. OpenCode needs a snapshot of existing session ids taken *before*
    //     spawn so the post-spawn poll can isolate the new one. The probe
    //     is a blocking `opencode session list` subprocess, so run it on
    //     the blocking pool rather than stalling the async runtime.
    if let Some(capture) = pending_opencode_capture.as_mut() {
        let command = capture.command.clone();
        let cwd = capture.cwd.clone();
        capture.before_ids = tauri::async_runtime::spawn_blocking(move || {
            agent::snapshot_opencode_session_ids(&command, &cwd)
        })
        .await
        .unwrap_or_default();
    }

    // 5. Spawn PTY + register session in AppState.
    let pty = PtyHandle::spawn(spawn).map_err(|e| {
        // ENOENT-style failures are the most common; surface a clear hint.
        format!("failed to start agent: {e}. Is the CLI installed and on PATH? You can override the binary path in Settings.")
    })?;
    let session_id = format!("S-{}", Uuid::new_v4().simple());
    // Run timeline (feature #8): for execution runs, register an end hook that
    // records `session_ended` when the PTY reader exits (natural exit, error,
    // or explicit kill). Plan launches record no run, so they get no hook —
    // keeping the timeline consistent with `agent_started`. The hook fires on
    // the reader thread (off the runtime), so it hops onto a captured handle.
    let on_end: Option<crate::terminal::EndHook> = if mode == TaskAgentMode::Execute {
        let repo = state.repo.clone();
        let handle = tokio::runtime::Handle::current();
        let task_for_hook = task_id.clone();
        let sid_for_hook = session_id.clone();
        Some(Box::new(move |reason| {
            let motivo = match reason {
                crate::terminal::EndReason::Killed => "encerrada",
                crate::terminal::EndReason::Eof => "eof",
                crate::terminal::EndReason::Error => "erro",
            };
            handle.spawn(async move {
                crate::audit::record(
                    repo.as_ref(),
                    Some(task_for_hook),
                    cadenza_proto::RunEventKind::SessaoEncerrada {
                        session_id: Some(sid_for_hook),
                        motivo: motivo.to_string(),
                    },
                )
                .await;
            });
        }))
    } else {
        None
    };
    let session = TerminalSession::start_with_end_hook(session_id.clone(), pty, on_end)
        .map_err(to_str_err)?;
    state
        .sessions
        .lock()
        .map_err(to_str_err)?
        .insert(session_id.clone(), session.clone());
    // Slice 4: commit the one-executor reservation to `Live` now that the
    // session exists (execution runs only; `None` for non-jira/plan starts).
    // The pre-spawn guard already claimed the slot as `Reserving`.
    if let Some(reservation) = executor_reservation.take() {
        reservation.commit(session_id.clone());
    }
    tracing::info!(
        task = %task_id, agent = ?agent_kind, model = %model, resumed, auto_mode,
        session = %session_id, "task agent started"
    );

    // 5a. Deliver the initial prompt on a fresh start. The planner has
    //     already baked it into the spawn argv for agents that support it
    //     (PromptDelivery::Argv) — those need nothing here. Only agents
    //     without a verified initial-prompt flag fall back to typing it
    //     into the PTY after a delay, which races the agent's UI boot.
    if let (Some(prompt), PromptDelivery::TypeIn) = (initial_prompt, prompt_delivery) {
        let session_for_prompt = session.clone();
        tauri::async_runtime::spawn(async move {
            send_initial_prompt(&session_for_prompt, &prompt).await;
        });
    }

    // 5b. With the spawn confirmed, move the task to `fazendo` if it
    //     wasn't already. Logged-only on failure: the agent is already
    //     running, the user can move the card manually if needed.
    //     Plan mode leaves the task in `a_fazer` — planning happens
    //     *before* execution, so the card must not move yet.
    if mode == TaskAgentMode::Execute && original_estado != Estado::Fazendo {
        if let Err(e) = state.repo.set_estado(&task_id, Estado::Fazendo).await {
            tracing::warn!(error = ?e, task = %task_id, "set_estado(fazendo) after spawn failed");
        }
    }

    // 6./7. Persist the run record and (Codex/Antigravity/OpenCode first-run only)
    //        kick off async session-UUID capture — but ONLY for execution
    //        runs. A planning run is intentionally not recorded so it can't
    //        be resumed into a later execution; with no record there is also
    //        nothing for the capture task to patch.
    if mode == TaskAgentMode::Execute {
        let run = TaskRun {
            agent: agent_kind,
            model: model.clone(),
            conversation_id: conversation_id_known.clone(),
            last_started_at: chrono::Utc::now(),
            last_session_id: Some(session_id.clone()),
        };
        if let Err(e) = state.task_runs.upsert(&task_id, run) {
            tracing::warn!(error = ?e, task = %task_id, "task_runs.upsert failed");
        }

        // Run timeline (feature #8): record the agent start. Execute runs
        // only — a plan launch records no run, so it emits no agent_started
        // either (keeps the timeline consistent with the run record).
        crate::audit::record(
            state.repo.as_ref(),
            Some(task_id.clone()),
            cadenza_proto::RunEventKind::AgenteIniciado {
                agente: crate::audit::serde_tag(&agent_kind),
                model: Some(model.clone()),
                modo: Some("execute".to_string()),
                resumido: resumed,
                session_id: Some(session_id.clone()),
            },
        )
        .await;

        // Run timeline (#6): record the pre-run checkpoint so the UI can offer
        // "revert this run".
        if let Some((git_ref, commit)) = &checkpoint {
            crate::audit::record(
                state.repo.as_ref(),
                Some(task_id.clone()),
                cadenza_proto::RunEventKind::CheckpointCriado {
                    git_ref: git_ref.clone(),
                    commit: commit.clone(),
                    dir: cwd.to_string_lossy().into_owned(),
                },
            )
            .await;
        }

        if let Some(capture) = pending_codex_capture {
            let task_runs = state.task_runs.clone();
            let app_handle = state.app_handle.lock().ok().and_then(|h| h.clone());
            let task_id_clone = task_id.clone();
            tauri::async_runtime::spawn(async move {
                let found = wait_for_codex_uuid(capture).await;
                match found {
                    Some(uuid) => {
                        if let Err(e) = task_runs.set_conversation_id(&task_id_clone, &uuid) {
                            tracing::warn!(error = ?e, task = %task_id_clone, "set_conversation_id failed");
                        } else {
                            tracing::info!(task = %task_id_clone, uuid = %uuid, "captured codex session uuid");
                            if let Some(app) = app_handle {
                                let _ = app.emit("task_run_changed", &task_id_clone);
                            }
                        }
                    }
                    None => {
                        tracing::warn!(task = %task_id_clone, "codex uuid capture timed out");
                    }
                }
            });
        }

        if let Some(capture) = pending_opencode_capture {
            let task_runs = state.task_runs.clone();
            let app_handle = state.app_handle.lock().ok().and_then(|h| h.clone());
            let task_id_clone = task_id.clone();
            tauri::async_runtime::spawn(async move {
                let found = wait_for_opencode_session(capture).await;
                match found {
                    Some(session_id) => {
                        if let Err(e) = task_runs.set_conversation_id(&task_id_clone, &session_id) {
                            tracing::warn!(error = ?e, task = %task_id_clone, "set_conversation_id failed");
                        } else {
                            tracing::info!(task = %task_id_clone, session = %session_id, "captured opencode session id");
                            if let Some(app) = app_handle {
                                let _ = app.emit("task_run_changed", &task_id_clone);
                            }
                        }
                    }
                    None => {
                        tracing::warn!(task = %task_id_clone, "opencode session capture timed out");
                    }
                }
            });
        }
    }

    Ok(StartTaskAgentResult {
        session_id,
        conversation_id: conversation_id_known,
        resumed,
    })
}

/// Resolve the localized initial prompt sent to the agent when a task
/// is started fresh. The key depends on `mode`: execution uses
/// `agent-initial-prompt`, planning uses `agent-planning-prompt`. Falls
/// back to a plain English message if the key isn't in either bundle.
fn render_initial_task_prompt(
    i18n_slot: &Mutex<I18n>,
    task_id: &str,
    titulo: &str,
    mode: TaskAgentMode,
) -> String {
    let key = match mode {
        TaskAgentMode::Execute => "agent-initial-prompt",
        TaskAgentMode::Plan => "agent-planning-prompt",
    };
    let mut args = FluentArgs::new();
    args.set("task_id", task_id.to_string());
    args.set("titulo", titulo.to_string());
    match i18n_slot.lock() {
        Ok(i18n) => i18n.t_with(key, Some(&args)),
        Err(_) => match mode {
            TaskAgentMode::Execute => format!(
                "Use the `cadenza` skill to coordinate with Cadenza through cadenza-cli. Your task is {task_id} ({titulo}). Start by running `cadenza-cli current --json`."
            ),
            TaskAgentMode::Plan => format!(
                "Use the `cadenza` skill to coordinate with Cadenza. You are in PLANNING mode for task {task_id} ({titulo}) — do NOT write or run any code yet. Read the task with `cadenza-cli list --json` and find {task_id}. Ask clarifying questions, in batches, until scope and acceptance criteria are clear. When we agree, save the plan by piping markdown into stdin: `cadenza-cli plan {task_id}` (omit --body so the plan is read from stdin). Do not mark anything done and do not start implementing — a separate execution run comes later."
            ),
        },
    }
}

/// Poll `opencode session list --format json` until a new session appears
/// or we give up. The command is non-interactive but still a process
/// spawn, so each attempt runs on the blocking pool.
async fn wait_for_opencode_session(capture: OpenCodeCapture) -> Option<String> {
    use tokio::time::{sleep, Duration};
    for _ in 0..20 {
        let capture_for_poll = capture.clone();
        let found = tauri::async_runtime::spawn_blocking(move || {
            agent::find_opencode_session_id(&capture_for_poll)
        })
        .await
        .ok()
        .flatten();
        if found.is_some() {
            return found;
        }
        sleep(Duration::from_millis(500)).await;
    }
    None
}

#[tauri::command]
pub fn read_task_run(
    state: State<'_, Arc<AppState>>,
    task_id: String,
) -> Result<Option<TaskRun>, String> {
    Ok(state.task_runs.get(&task_id))
}

#[tauri::command]
pub fn list_task_runs(state: State<'_, Arc<AppState>>) -> Result<HashMap<String, TaskRun>, String> {
    Ok(state.task_runs.snapshot())
}

#[tauri::command]
pub fn clear_task_run(state: State<'_, Arc<AppState>>, task_id: String) -> Result<(), String> {
    state.task_runs.forget(&task_id).map_err(to_str_err)
}

#[tauri::command]
pub fn list_installed_agents() -> Vec<agent::AgentPresence> {
    agent::list_installed_agents()
}

/// Discover the models the agent's CLI exposes via its interactive
/// `/model` menu. The first call per `agent_kind` per process spawns
/// the binary under a PTY, drives it to the menu, parses the rendered
/// frame, and caches the result. Subsequent calls return the cached
/// list. `refresh=true` skips the cache and re-runs discovery.
///
/// We honor the *global* `Config.agente.command` override (project-level
/// overrides aren't applied here — model availability is per agent
/// install, not per project — and threading a task_id through this
/// surface would be a larger change for marginal correctness).
#[tauri::command]
pub async fn list_agent_models(
    state: State<'_, Arc<AppState>>,
    agent_kind: AgenteKind,
    refresh: Option<bool>,
    cached_only: Option<bool>,
) -> Result<Vec<crate::models::ModelEntry>, String> {
    let command = {
        let cfg = state.config.lock().map_err(to_str_err)?;
        cfg.agente
            .as_ref()
            .filter(|a| a.kind == agent_kind)
            .and_then(|a| a.command.clone())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| agent::default_command(agent_kind).to_string())
    };
    let cache_key = (agent_kind, command.clone());
    if !refresh.unwrap_or(false) {
        if let Some(cached) = state
            .agent_models
            .lock()
            .map_err(to_str_err)?
            .get(&cache_key)
        {
            return Ok(cached.clone());
        }
    }
    // Task-start path: never spawn the slow probe. A cache miss above means
    // nothing is loaded yet, so return empty and let the UI fall back to a
    // free-text model entry. Discovery lives in Settings → Modelos.
    if cached_only.unwrap_or(false) {
        return Ok(Vec::new());
    }
    // `discover_models` blocks ~10-15 s on PTY warmup + tail. Move it
    // off the tauri runtime so command dispatch (and the UI) stay
    // responsive.
    let cmd_for_spawn = command.clone();
    let entries = tauri::async_runtime::spawn_blocking(move || {
        // predismiss_enters=1: claude shows a trust dialog on first
        // launch in an unknown cwd; codex shows an onboarding step.
        // One Enter handles both with no false negative on already-
        // trusted setups (the extra Enter becomes a no-op at the prompt).
        crate::models::discover_models(
            &cmd_for_spawn,
            agent_kind,
            8,
            6,
            1,
            refresh.unwrap_or(false),
        )
    })
    .await
    .map_err(to_str_err)?
    .map_err(|e| {
        let msg = e.to_string();
        // Spawn couldn't find the binary anywhere (PATH + standard install
        // locations). Give an actionable hint instead of the raw os error.
        // Covers the Windows ("os error 2") and Unix/portable-pty
        // ("No viable candidates found in PATH …") not-found phrasings.
        if msg.contains("os error 2")
            || msg.contains("cannot find the file")
            || msg.contains("No viable candidates")
        {
            format!(
                "`{command}` not found on PATH or in its standard install location. \
                 Set its full path in Settings → agent command, or install it on your PATH."
            )
        } else {
            format!("discover_models({command}): {msg}")
        }
    })?;
    if entries.is_empty() {
        let source = if agent_kind == AgenteKind::OpenCode {
            "`opencode models` output"
        } else {
            "the agent's `/model` menu"
        };
        return Err(format!(
            "no models parsed from `{command}` — {source} likely changed shape; please report this"
        ));
    }
    state
        .agent_models
        .lock()
        .map_err(to_str_err)?
        .insert(cache_key, entries.clone());
    // Persist to config.json so the list survives restarts (seeded back
    // into the in-memory cache by AppState::init). Upsert by
    // `(kind, command)` to match the cache keying. Logged-only on failure:
    // the in-memory cache already holds the fresh list this session.
    if let Some(path) = dirs::home_dir().map(|h| h.join(".cadenza").join("config.json")) {
        let mut cfg = state.config.lock().map_err(to_str_err)?;
        let record = crate::models::CachedModels {
            kind: agent_kind,
            command: command.clone(),
            models: entries.clone(),
        };
        let list = cfg.agent_models.get_or_insert_with(Vec::new);
        if let Some(slot) = list
            .iter_mut()
            .find(|c| c.kind == agent_kind && c.command == command)
        {
            *slot = record;
        } else {
            list.push(record);
        }
        if let Err(e) = cfg.save_to(&path) {
            tracing::warn!(error = %e, "failed to persist discovered models to config");
        }
    }
    Ok(entries)
}
