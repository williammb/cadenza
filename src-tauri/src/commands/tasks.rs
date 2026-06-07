//! Task CRUD + ordering + blocker command handlers — split out of the
//! `commands` god-module. Pure relocation. Re-exported via `commands`'s
//! `pub use tasks::*;` so every existing `commands::*` path still resolves
//! unchanged (Tauri `generate_handler!` in lib.rs references these paths;
//! `ipc.rs` uses `crate::commands::{next_task_id, highest_task_number,
//! sort_tasks_by_order}`; `proposals.rs` reaches `mint_next_task_id` /
//! `highest_task_number` via `super::`).

// Bring in the parent module's imports and shared helpers (AppState,
// to_str_err, enrich_task, emit_tasks_changed, Estado, Task, HashMap,
// Ordering, State, Arc, etc.). Parent-private items are visible here.
use super::*;

// ───────────────────────── tasks ─────────────────────────

#[tauri::command]
pub async fn list_tasks(
    state: State<'_, Arc<AppState>>,
    estado: Option<String>,
) -> Result<Vec<Task>, String> {
    let filter = estado.as_deref().and_then(Estado::parse);
    let tasks = state.repo.list_tasks(filter).await.map_err(to_str_err)?;
    let mut tasks: Vec<Task> = tasks.into_iter().map(|t| enrich_task(&state, t)).collect();
    sort_tasks_by_order(&mut tasks, &state.task_order.snapshot());
    Ok(tasks)
}

/// Sort tasks by the per-column priority order from `task-order.json`,
/// in place. Tasks are kept grouped by estado (deterministic across
/// backends); within a column, ids present in that column's list come
/// first in list order, and any task not listed (a freshly created card,
/// or one moved in out-of-band) sorts after them by ascending `T-<n>`
/// number — so the newest task lands last. Stale ids in the list (a
/// deleted task, or one whose estado changed) simply never match a real
/// task and are ignored.
pub(crate) fn sort_tasks_by_order(tasks: &mut [Task], order: &HashMap<String, Vec<String>>) {
    tasks.sort_by(|a, b| {
        let (ea, eb) = (a.estado.as_str(), b.estado.as_str());
        if ea != eb {
            return ea.cmp(eb);
        }
        let list = order.get(ea);
        let rank = |id: &str| list.and_then(|l| l.iter().position(|x| x == id));
        match (rank(&a.id), rank(&b.id)) {
            (Some(i), Some(j)) => i.cmp(&j),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => task_num(&a.id)
                .cmp(&task_num(&b.id))
                .then_with(|| a.id.cmp(&b.id)),
        }
    });
}

/// Numeric component of a `T-<n>` id, or `u64::MAX` for any other shape
/// so non-`T-` ids sort to the end. Used to keep unlisted tasks ordered
/// newest-last.
fn task_num(id: &str) -> u64 {
    id.strip_prefix("T-")
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(u64::MAX)
}

#[tauri::command]
pub async fn read_task(state: State<'_, Arc<AppState>>, id: String) -> Result<Task, String> {
    let task = state.repo.read_task(&id).await.map_err(to_str_err)?;
    Ok(enrich_task(&state, task))
}

/// Compute the next sequential task id (`T-<n>`) by scanning existing
/// tasks. The frontend calls this just before submitting a new task so
/// IDs read like a notebook (T-1, T-2, ...) instead of opaque UUIDs.
///
/// Source of truth is `repo.list_tasks(None)` — that survives external
/// writes from the Node.js task-ai version sharing `~/.cadenza/tasks/`.
/// Two near-simultaneous creates can theoretically race to the same
/// number, but the cost is a benign rename; the file backend overwrites
/// safely, and the UI does this in one user-initiated submit.
#[tauri::command]
pub async fn next_task_id(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    mint_next_task_id(state.repo.as_ref()).await
}

/// Compute the next sequential `T-<n>` id from the repo's current tasks.
/// Shared by `next_task_id` (UI pre-fill) and `create_task_from_proposta`
/// (derived-task materialization) so the id scheme lives in one place.
pub(crate) async fn mint_next_task_id(repo: &dyn Repository) -> Result<String, String> {
    let tasks = repo.list_tasks(None).await.map_err(to_str_err)?;
    let next = highest_task_number(tasks.iter().map(|t| t.id.as_str())) + 1;
    Ok(format!("T-{next}"))
}

/// Inspect `T-<n>` ids, ignore any other shape, and return the highest
/// `n` seen (0 if none). Pure — call from anywhere that has an
/// iterator of task ids.
pub fn highest_task_number<'a, I: Iterator<Item = &'a str>>(ids: I) -> u64 {
    let mut max = 0u64;
    for id in ids {
        let Some(rest) = id.strip_prefix("T-") else {
            continue;
        };
        if let Ok(n) = rest.parse::<u64>() {
            if n > max {
                max = n;
            }
        }
    }
    max
}

#[tauri::command]
pub async fn create_task(
    state: State<'_, Arc<AppState>>,
    task: Task,
    project_id: String,
) -> Result<(), String> {
    // Toda task precisa de projeto. O ID precisa existir em
    // `config.projects` — caso contrário a UI/CLI tentou usar um
    // projeto inválido (digitação, projeto removido entre passos).
    let pid = project_id.trim();
    if pid.is_empty() {
        return Err("project_id is required".to_string());
    }
    {
        let cfg = state.config.lock().map_err(to_str_err)?;
        if !cfg.projects.iter().any(|p| p.id == pid) {
            return Err(format!("unknown project_id: {pid}"));
        }
    }
    let blocked_by =
        normalize_and_validate_blockers(state.repo.as_ref(), &task.id, task.blocked_by.clone())
            .await?;
    state.repo.create_task(&task).await.map_err(to_str_err)?;
    if !blocked_by.is_empty() {
        state
            .task_blockers
            .set(&task.id, blocked_by)
            .map_err(to_str_err)?;
    }
    state
        .task_projects
        .set(&task.id, Some(pid))
        .map_err(to_str_err)?;
    Ok(())
}

#[tauri::command]
pub async fn set_estado(
    state: State<'_, Arc<AppState>>,
    id: String,
    estado: String,
) -> Result<(), String> {
    let parsed = Estado::parse(&estado).ok_or_else(|| format!("invalid estado: {estado}"))?;
    if parsed == Estado::Fazendo {
        ensure_task_unblocked(&state, &id).await?;
    }
    state.repo.set_estado(&id, parsed).await.map_err(to_str_err)
}

/// Persist the priority order of one column. The UI sends the full
/// ordered id list for the affected estado after a drag-to-reorder (or
/// cross-column drop), so the call is idempotent and self-correcting —
/// it overwrites whatever was stored. Ordering is a GUI-only concern, so
/// there is no matching NDJSON op: the CLI never reorders.
#[tauri::command]
pub async fn set_task_order(
    state: State<'_, Arc<AppState>>,
    estado: String,
    ids: Vec<String>,
) -> Result<(), String> {
    Estado::parse(&estado).ok_or_else(|| format!("invalid estado: {estado}"))?;
    state.task_order.set(&estado, ids).map_err(to_str_err)
}

#[tauri::command]
pub async fn append_log(
    state: State<'_, Arc<AppState>>,
    id: String,
    text: String,
) -> Result<(), String> {
    state.repo.append_log(&id, &text).await.map_err(to_str_err)
}

#[tauri::command]
pub async fn update_task_body(
    state: State<'_, Arc<AppState>>,
    id: String,
    body: String,
) -> Result<(), String> {
    state
        .repo
        .update_task_body(&id, &body)
        .await
        .map_err(to_str_err)
}

#[tauri::command]
pub async fn delete_task(state: State<'_, Arc<AppState>>, id: String) -> Result<(), String> {
    state.repo.delete_task(&id).await.map_err(to_str_err)?;
    // Drop the task's review packages (+ any dangling done journal on the
    // file backend). Best-effort: the task row/file is already gone, so
    // orphaned packages only cost storage (PLAN §F.17).
    if let Err(e) = state.repo.delete_review_packages(&id).await {
        tracing::warn!(error = ?e, task = %id, "delete_review_packages failed");
    }
    // Drop the side-mapping entry so it doesn't dangle forever after
    // the task file is gone. Failure here is non-fatal — the task is
    // already deleted; a stale mapping entry just costs disk bytes.
    if let Err(e) = state.task_projects.forget(&id) {
        tracing::warn!(error = ?e, task = %id, "task_projects.forget failed");
    }
    if let Err(e) = state.task_runs.forget(&id) {
        tracing::warn!(error = ?e, task = %id, "task_runs.forget failed");
    }
    if let Err(e) = state.task_worktrees.forget(&id) {
        tracing::warn!(error = ?e, task = %id, "task_worktrees.forget failed");
    }
    if let Err(e) = state.task_blockers.forget(&id) {
        tracing::warn!(error = ?e, task = %id, "task_blockers.forget failed");
    }
    if let Err(e) = state.task_order.forget(&id) {
        tracing::warn!(error = ?e, task = %id, "task_order.forget failed");
    }
    // Drop any images the task body referenced. Best-effort: the task is
    // already gone, orphaned files only cost disk bytes.
    crate::attachments::delete_owner("tasks", &id);
    Ok(())
}

#[tauri::command]
pub async fn set_titulo(
    state: State<'_, Arc<AppState>>,
    id: String,
    titulo: String,
) -> Result<(), String> {
    state
        .repo
        .set_titulo(&id, &titulo)
        .await
        .map_err(to_str_err)
}

/// First task in `fazendo`, or null if none. Tooling convenience — the
/// CLI's `cadenza current` maps here.
#[tauri::command]
pub async fn current_task(state: State<'_, Arc<AppState>>) -> Result<Option<Task>, String> {
    let task = state.repo.current_task().await.map_err(to_str_err)?;
    Ok(task.map(|t| enrich_task(&state, t)))
}

/// Persist the blockers for a task. Blocker ids must point to existing
/// tasks and cannot include the task itself.
#[tauri::command]
pub async fn set_task_blockers(
    state: State<'_, Arc<AppState>>,
    task_id: String,
    blocked_by: Vec<String>,
) -> Result<(), String> {
    crate::store::validate_id(&task_id).map_err(to_str_err)?;
    state.repo.read_task(&task_id).await.map_err(to_str_err)?;
    let blocked_by =
        normalize_and_validate_blockers(state.repo.as_ref(), &task_id, blocked_by).await?;
    state
        .task_blockers
        .set(&task_id, blocked_by)
        .map_err(to_str_err)?;
    emit_tasks_changed(&state, &task_id);
    Ok(())
}

pub(crate) async fn normalize_and_validate_blockers(
    repo: &dyn Repository,
    task_id: &str,
    blocked_by: Vec<String>,
) -> Result<Vec<String>, String> {
    let mut normalized = Vec::new();
    for raw in blocked_by {
        let id = raw.trim();
        if id.is_empty() || normalized.iter().any(|existing| existing == id) {
            continue;
        }
        crate::store::validate_id(id).map_err(to_str_err)?;
        if id == task_id {
            return Err(format!("task '{task_id}' cannot block itself"));
        }
        repo.read_task(id).await.map_err(to_str_err)?;
        normalized.push(id.to_string());
    }
    Ok(normalized)
}

pub(crate) async fn ensure_task_unblocked(state: &AppState, task_id: &str) -> Result<(), String> {
    let blockers = state.task_blockers.get(task_id);
    if blockers.is_empty() {
        return Ok(());
    }

    let mut unfinished = Vec::new();
    for blocker_id in blockers {
        match state.repo.read_task(&blocker_id).await {
            Ok(task) if task.estado.satisfies_blocker() => {}
            Ok(task) => unfinished.push(format!(
                "{} '{}' is {}",
                task.id,
                task.titulo,
                task.estado.as_str()
            )),
            Err(StoreError::NotFound(_)) => unfinished.push(format!("{blocker_id} was not found")),
            Err(e) => return Err(to_str_err(e)),
        }
    }

    if unfinished.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "task '{task_id}' is blocked by unfinished task(s): {}",
            unfinished.join("; ")
        ))
    }
}
