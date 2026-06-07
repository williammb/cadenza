//! Task worktree/branch command handlers + workspace preparation — split
//! out of the `commands` god-module. Pure relocation. Re-exported via
//! `commands`'s `pub use worktrees::*;` so every existing `commands::*` path
//! still resolves unchanged (Tauri `generate_handler!` in lib.rs;
//! `crate::commands::suggested_worktree_path` in `jira/worktree.rs`).
//! `prepare_task_workspace` / `suggested_worktree_path` are `pub(crate)` so
//! sibling submodules reach them via `super::` (e.g. `agents.rs`'s
//! `start_task_agent`).

// Bring in the parent module's imports and shared helpers (AppState,
// to_str_err, emit_tasks_changed, WorktreeInfo, Config, Path, PathBuf,
// State, Arc, Serialize, etc.). Parent-private items are visible here.
use super::*;

/// Snapshot of every task→worktree/branch mapping. Currently unused by
/// the board — `list_tasks`/`read_task`/`current_task` already enrich
/// each task with `worktree_path`/`branch` inline (see
/// `TaskWorktrees::enrich`), so there is no client-side join. Kept as a
/// command for a future board view that needs the mapping standalone;
/// do not remove the inline enrichment on the assumption the UI joins here.
#[tauri::command]
pub fn list_task_worktrees(
    state: State<'_, Arc<AppState>>,
) -> Result<HashMap<String, WorktreeInfo>, String> {
    Ok(state.task_worktrees.snapshot())
}

/// Persist the task's declarative branch/worktree config from the modal:
/// origin → destination, the use-worktree intent, and the worktree path.
/// No git runs here — the actual pull/branch/worktree happens at agent
/// start (`prepare_task_workspace`). An all-empty config clears the entry.
#[tauri::command]
pub fn set_task_worktree(
    state: State<'_, Arc<AppState>>,
    task_id: String,
    worktree_path: Option<String>,
    branch: Option<String>,
    origin_branch: Option<String>,
    use_worktree: Option<bool>,
) -> Result<(), String> {
    // Normalize empty strings to None so a cleared field doesn't persist
    // as `Some("")` and later defeat the `is_empty`/fallback checks.
    let norm = |s: Option<String>| s.filter(|v| !v.trim().is_empty());
    state
        .task_worktrees
        .set(
            &task_id,
            WorktreeInfo {
                worktree_path: norm(worktree_path),
                branch: norm(branch),
                origin_branch: norm(origin_branch),
                use_worktree: use_worktree.unwrap_or(false),
            },
        )
        .map_err(to_str_err)
}

/// What the task modal needs to pre-fill its worktree/branch section in
/// one round-trip: the project repo path, its *current* branch (the
/// default shown to the user), a suggested sibling worktree path, and any
/// association already stored for this task.
#[derive(Serialize)]
pub struct TaskWorktreeDefaults {
    pub project_path: String,
    pub current_branch: String,
    pub suggested_worktree_path: String,
    pub stored: WorktreeInfo,
    /// Local branches in the repo, to populate the origin/destination
    /// pickers. Empty when the repo has no commits yet or git fails.
    pub branches: Vec<String>,
    /// The project's configured default branch (`None`/empty when unset);
    /// the UI pre-fills origin with it before falling back to current.
    pub default_branch: Option<String>,
}

/// Resolve the on-disk repo path for a task via its project mapping.
/// Mirrors the project-resolution step in `start_task_agent`.
fn project_path_for_task(state: &AppState, task_id: &str) -> Result<PathBuf, String> {
    let project_id = state
        .task_projects
        .snapshot()
        .get(task_id)
        .cloned()
        .ok_or_else(|| {
            format!(
                "task '{task_id}' has no project assigned — assign one so the worktree has a repo"
            )
        })?;
    let cfg = state.config.lock().map_err(to_str_err)?;
    let project = cfg
        .projects
        .iter()
        .find(|p| p.id == project_id)
        .ok_or_else(|| format!("project '{project_id}' not found in config"))?;
    Ok(project.path.clone())
}

/// The configured default branch for a task's project, or `None` when the
/// task has no project, the project is gone, or its `default_branch` is
/// unset/blank. Mirrors `project_path_for_task`'s task→project resolution.
fn default_branch_for_task(state: &AppState, task_id: &str) -> Result<Option<String>, String> {
    let cfg = state.config.lock().map_err(to_str_err)?;
    Ok(state
        .task_projects
        .snapshot()
        .get(task_id)
        .and_then(|pid| cfg.projects.iter().find(|p| &p.id == pid))
        .and_then(|p| p.default_branch.clone())
        .filter(|b| !b.trim().is_empty()))
}

/// Default sibling worktree path: `<repo-parent>/<repo-name>-<branch>`,
/// with path separators in the branch flattened to `-` so it stays a
/// single directory name.
pub(crate) fn suggested_worktree_path(repo: &Path, branch: &str) -> PathBuf {
    let sanitized: String = branch
        .chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .collect();
    let name = repo.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
    let parent = repo.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{name}-{sanitized}"))
}

/// Pre-fill data for the task modal's worktree section. Reads the
/// project's current git branch; surfaces git errors to the UI (e.g. the
/// project path is not a git repo) so the modal can show a hint.
#[tauri::command]
pub async fn task_worktree_defaults(
    state: State<'_, Arc<AppState>>,
    task_id: String,
) -> Result<TaskWorktreeDefaults, String> {
    let repo = project_path_for_task(&state, &task_id)?;
    let current_branch = crate::git::current_branch(&repo)
        .await
        .map_err(to_str_err)?;
    let suggested = suggested_worktree_path(&repo, &current_branch);
    let stored = state.task_worktrees.get(&task_id).unwrap_or_default();
    let branches = crate::git::list_branches(&repo).await.unwrap_or_default();
    let default_branch = default_branch_for_task(&state, &task_id)?;
    Ok(TaskWorktreeDefaults {
        project_path: repo.to_string_lossy().into_owned(),
        current_branch,
        suggested_worktree_path: suggested.to_string_lossy().into_owned(),
        stored,
        branches,
        default_branch,
    })
}

/// Prepare the git workspace for a task right before an agent starts,
/// driven by the declarative config the modal stored (`set_task_worktree`).
///
/// Resolves the origin and destination branches, pulls origin (blocking on
/// a real failure; a no-op without an upstream), creates/switches the
/// destination branch, and creates the worktree when requested. Returns the
/// cwd the agent runs in — the worktree when used, otherwise the project
/// repo — and persists the resolved config back to the sidecar.
pub(crate) async fn prepare_task_workspace(
    state: &AppState,
    task_id: &str,
) -> Result<PathBuf, String> {
    let repo = project_path_for_task(state, task_id)?;
    let default_branch = default_branch_for_task(state, task_id)?;
    let stored = state.task_worktrees.get(task_id).unwrap_or_default();
    let current = crate::git::current_branch(&repo)
        .await
        .map_err(to_str_err)?;

    // 1. Resolve origin (stored → project default → current) and
    //    destination (stored → origin).
    let origin = stored
        .origin_branch
        .clone()
        .filter(|b| !b.trim().is_empty())
        .or(default_branch)
        .unwrap_or_else(|| current.clone())
        .trim()
        .to_string();
    let destination = stored
        .branch
        .clone()
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| origin.clone())
        .trim()
        .to_string();

    // 2. Pull origin. Blocks on a real failure; no-op without an upstream.
    crate::git::pull_branch(&repo, &origin)
        .await
        .map_err(to_str_err)?;

    let dest_exists = crate::git::branch_exists(&repo, &destination)
        .await
        .map_err(to_str_err)?;
    // New destination branches are based on origin; for an existing branch
    // git ignores the start point, so passing it is harmless either way.
    let start_point = if dest_exists {
        None
    } else {
        Some(origin.as_str())
    };

    // 3 + 4. Land on the destination branch, in a worktree when asked.
    let cwd = if stored.use_worktree {
        let wt_path = stored
            .worktree_path
            .clone()
            .filter(|p| !p.trim().is_empty())
            .ok_or_else(|| {
                format!("task '{task_id}' is set to use a worktree but has no worktree path")
            })?;
        let wt = PathBuf::from(&wt_path);
        if wt.exists() {
            // Reuse the existing worktree: switch it to the destination only
            // when it isn't already there.
            let on = crate::git::current_branch(&wt).await.map_err(to_str_err)?;
            if on != destination {
                crate::git::switch_branch(&wt, &destination, !dest_exists, start_point)
                    .await
                    .map_err(to_str_err)?;
            }
        } else {
            crate::git::add_worktree(&repo, &wt, &destination, !dest_exists, start_point)
                .await
                .map_err(to_str_err)?;
        }
        wt
    } else {
        // No worktree: operate on the project repo. Switch only when not
        // already on the destination ("se for igual só vai para o ramo se
        // já não estiver").
        if current != destination {
            crate::git::switch_branch(&repo, &destination, !dest_exists, start_point)
                .await
                .map_err(to_str_err)?;
        }
        repo.clone()
    };

    // 5. Persist the resolved config so the read-only displays and the next
    //    open reflect what actually happened.
    let resolved = WorktreeInfo {
        worktree_path: if stored.use_worktree {
            Some(cwd.to_string_lossy().into_owned())
        } else {
            None
        },
        branch: Some(destination),
        origin_branch: Some(origin),
        use_worktree: stored.use_worktree,
    };
    state
        .task_worktrees
        .set(task_id, resolved)
        .map_err(to_str_err)?;
    emit_tasks_changed(state, task_id);
    Ok(cwd)
}
