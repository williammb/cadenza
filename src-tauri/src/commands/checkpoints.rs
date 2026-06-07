//! Checkpoint / rollback commands (feature #6).
//!
//! "Revert this run" rewinds a task's workspace to the checkpoint captured
//! before the agent started (recorded as a `CheckpointCriado` event by
//! `start_task_agent`). Safety properties:
//!
//! - **Reversible.** Before mutating anything, the CURRENT state is snapshotted
//!   (working tree + staged index) AND recorded as its own `CheckpointCriado`
//!   event, so the user can rewind the rewind from the timeline — even if the
//!   restore itself fails partway.
//! - **History-safe.** Only the working tree changes; HEAD and the branch ref
//!   are never moved (see `git::restore_checkpoint`).
//! - **Won't fight a live agent.** Refuses while a session for the task is
//!   still running, so a concurrent `clean`/`checkout-index` can't corrupt the
//!   agent's in-flight edits.
//! - **Main-repo aware.** In a disposable per-task worktree the rewind cleans
//!   aggressively; in the human's main repo it does not remove nested git
//!   repos, and any untracked leftovers are reported back as a partial rewind.

use super::*;
use cadenza_proto::RunEventKind;
use std::path::PathBuf;

/// Outcome of a revert, surfaced to the UI.
#[derive(Debug, Serialize)]
pub struct RevertResult {
    /// Checkpoint commit the workspace was rewound to.
    pub reverted_commit: String,
    /// Snapshot of the pre-revert state (recorded as its own checkpoint, so
    /// the revert can be undone from the timeline).
    pub safety_commit: String,
    /// Workspace directory that was rewound.
    pub dir: String,
    /// How many paths were dirty before the rewind (informational).
    pub dirty_before: usize,
    /// Untracked paths still present after the rewind (e.g. a nested git repo
    /// `clean` refused to remove in main-repo mode). Empty = complete rewind.
    pub partial_leftovers: Vec<String>,
}

/// Rewind a task's workspace to a checkpoint. `commit` selects a specific
/// checkpoint; `None` uses the most-recent one. The pre-revert state is
/// snapshotted and recorded first so the operation is reversible.
#[tauri::command]
pub async fn revert_task_checkpoint(
    state: State<'_, Arc<AppState>>,
    task_id: String,
    commit: Option<String>,
) -> Result<RevertResult, String> {
    // Refuse while an agent is live in this task's workspace: a concurrent
    // checkout-index/clean would corrupt the running agent's edits and delete
    // untracked files it is mid-write. The user must stop the agent first.
    if let Some(sid) = state
        .task_runs
        .snapshot()
        .get(&task_id)
        .and_then(|r| r.last_session_id.clone())
    {
        let live = state
            .sessions
            .lock()
            .map_err(to_str_err)?
            .contains_key(&sid);
        if live {
            return Err(
                "agent_running: stop the running agent for this task before reverting".to_string(),
            );
        }
    }

    // Resolve the target checkpoint (commit + the dir it was taken in) from the
    // event log — the checkpoint is bound to that exact workspace path.
    let events = state
        .repo
        .list_events(Some(&task_id), None)
        .await
        .map_err(to_str_err)?;
    let checkpoints: Vec<(String, String)> = events
        .iter()
        .filter_map(|e| match &e.kind {
            RunEventKind::CheckpointCriado { commit, dir, .. } => {
                Some((commit.clone(), dir.clone()))
            }
            _ => None,
        })
        .collect();

    let (target_commit, dir) = match &commit {
        Some(c) => checkpoints
            .iter()
            .rev()
            .find(|(cm, _)| cm == c)
            .cloned()
            .ok_or_else(|| format!("no checkpoint with commit {c} for task {task_id}"))?,
        None => checkpoints
            .last()
            .cloned()
            .ok_or_else(|| format!("no checkpoint recorded for task {task_id}"))?,
    };
    let dir_path = PathBuf::from(&dir);

    // A disposable per-task worktree can be cleaned aggressively (nested repos
    // removed); the human's main repo cannot.
    let is_worktree = crate::git::is_linked_worktree(&dir_path)
        .await
        .unwrap_or(false);

    // Preflight: how dirty the workspace is now (informational — the safety
    // snapshot below makes the rewind recoverable regardless).
    let dirty = crate::git::worktree_dirty_files(&dir_path)
        .await
        .map_err(to_str_err)?;

    // Safety snapshot of the CURRENT state, RECORDED as a checkpoint BEFORE any
    // mutation: working tree (untracked included) + staged index. Recording it
    // up front makes it revertable through the normal UI path and durable even
    // if the restore below fails midway.
    let safety_ref = format!(
        "refs/cadenza/checkpoints/{task_id}/pre-revert-{}",
        uuid::Uuid::new_v4().simple()
    );
    let safety_commit = crate::git::create_checkpoint(&dir_path, &safety_ref)
        .await
        .map_err(to_str_err)?;
    // Keep staged-only blobs reachable too (narrow case: staged-then-edited the
    // same path). Best-effort — a failure here must not abort the revert.
    let index_ref = format!(
        "refs/cadenza/checkpoints/{task_id}/pre-revert-index-{}",
        uuid::Uuid::new_v4().simple()
    );
    if let Err(e) = crate::git::checkpoint_index(&dir_path, &index_ref).await {
        tracing::warn!(error = %e, task = %task_id, "pre-revert index snapshot failed");
    }
    crate::audit::record(
        state.repo.as_ref(),
        Some(task_id.clone()),
        RunEventKind::CheckpointCriado {
            git_ref: safety_ref,
            commit: safety_commit.clone(),
            dir: dir.clone(),
        },
    )
    .await;

    // Rewind the working tree (history untouched).
    let partial_leftovers = crate::git::restore_checkpoint(&dir_path, &target_commit, is_worktree)
        .await
        .map_err(to_str_err)?;

    crate::audit::record(
        state.repo.as_ref(),
        Some(task_id.clone()),
        RunEventKind::RunRevertido {
            commit: target_commit.clone(),
            safety_commit: Some(safety_commit.clone()),
        },
    )
    .await;

    Ok(RevertResult {
        reverted_commit: target_commit,
        safety_commit,
        dir,
        dirty_before: dirty.len(),
        partial_leftovers,
    })
}
