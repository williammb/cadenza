//! Review-package command handlers + the shared human-decision transition
//! core — split out of the `commands` god-module. Pure relocation.
//! Re-exported via `commands`'s `pub use review::*;` so every existing
//! `commands::*` path still resolves unchanged (Tauri `generate_handler!`
//! in lib.rs; `crate::commands::apply_review_decision` in ipc.rs).

// Bring in the parent module's imports and shared helpers (AppState,
// to_str_err, Repository, Estado, StoreError, etc.). Parent-private items
// are visible to this child module.
use super::*;

// The human approve / request-changes decision transition core
// (`apply_review_decision` + `ReviewDecisionError`) now lives in the review
// domain at `crate::review` — the command handler below only adapts args and
// stringifies the typed error. ipc.rs's `review_decision_op` calls the same
// `crate::review::apply_review_decision` directly.

/// Latest review package for a task (highest attempt), or `None` when the
/// task has never been `done` with evidence. The webview's "Revisão" tab
/// renders the returned [`ReviewPackage`] directly (PLAN §D.13).
#[tauri::command]
pub async fn get_review_package(
    state: State<'_, Arc<AppState>>,
    task_id: String,
) -> Result<Option<crate::store::ReviewPackage>, String> {
    state
        .repo
        .latest_review_package(&task_id)
        .await
        .map_err(to_str_err)
}

/// One file's diff text within a [`DiffGroup`].
#[derive(Serialize)]
pub struct DiffFile {
    pub path: String,
    /// Unified-diff text, size-capped by the engine.
    pub patch: String,
    /// True when the per-file cap clipped this file's patch.
    pub truncated: bool,
}

/// Files bucketed under one stored intent-group label (uncovered → "Other").
#[derive(Serialize)]
pub struct DiffGroup {
    pub label: String,
    pub files: Vec<DiffFile>,
}

/// Response of [`get_review_diff`] (PLAN §D.13).
#[derive(Serialize)]
pub struct ReviewDiff {
    /// True when the current worktree fingerprint differs from the stored
    /// one: the live committed diff would be divergent, so we serve the
    /// stored capped uncommitted patch instead.
    pub stale: bool,
    /// Committed diff bucketed by the stored intent groups. Empty when the
    /// base/head shas are missing or the worktree is gone (see `note`).
    pub groups: Vec<DiffGroup>,
    /// The stored capped+redacted uncommitted patch, served only when
    /// `stale` is true.
    pub uncommitted: Option<crate::review::CappedPatch>,
    /// True when the worktree was unreadable / base or head sha is missing,
    /// so no live committed diff could be computed.
    pub diff_unavailable: bool,
    /// True when a per-file or total size cap clipped the live diff.
    pub truncated: bool,
    /// Files dropped entirely by the live diff's count/total caps.
    pub files_omitted: u32,
}

/// Bucket label for files not covered by any stored intent group.
const DIFF_OTHER_LABEL: &str = "Other";

/// Recompute the committed diff live from the stored `base_sha..head_sha`
/// and bucket it by the stored intent groups (uncovered files → "Other").
///
/// Staleness: the current worktree fingerprint is recomputed (hardened
/// `git status --porcelain=v1 -z`) and compared to the one stored at
/// `done` time. On mismatch we set `stale = true` and serve the stored
/// capped+redacted uncommitted patch (a live uncommitted diff would now be
/// divergent), per PLAN §D.13. The committed diff is always recomputed from
/// the immutable shas. Missing worktree / unresolved base ⇒ empty groups +
/// `diff_unavailable`.
#[tauri::command]
pub async fn get_review_diff(
    state: State<'_, Arc<AppState>>,
    task_id: String,
) -> Result<ReviewDiff, String> {
    let Some(pkg) = state
        .repo
        .latest_review_package(&task_id)
        .await
        .map_err(to_str_err)?
    else {
        return Err(format!("no review package for task {task_id}"));
    };

    // Resolve the worktree out of the side-mapping (mirrors done_op).
    let worktree_path = state
        .task_worktrees
        .get(&task_id)
        .and_then(|w| w.worktree_path)
        .filter(|p| !p.trim().is_empty());

    // Staleness: compare the live fingerprint to the stored one. An
    // unreadable live fingerprint (worktree gone) is treated as "cannot
    // compare" ⇒ not stale, and the committed diff below will be empty.
    let mut stale = false;
    if let (Some(wt), Some(stored_fp)) =
        (worktree_path.as_deref(), pkg.worktree_fingerprint.as_ref())
    {
        if let Some(live_fp) = crate::review::worktree_fingerprint(Path::new(wt)).await {
            stale = &live_fp != stored_fp;
        }
    }

    // Committed diff is always reproducible from the immutable shas.
    let paths: Vec<String> = pkg.changed_files.iter().map(|f| f.path.clone()).collect();
    let (groups, diff_unavailable, truncated, files_omitted) = match (
        worktree_path.as_deref(),
        pkg.base_sha.as_deref(),
        pkg.head_sha.as_deref(),
    ) {
        (Some(wt), Some(base), Some(head)) => {
            let live =
                crate::review::recompute_committed_diff(Path::new(wt), base, head, &paths).await;
            (
                bucket_diff_by_groups(live.files, &pkg.groups),
                false,
                live.truncated,
                live.files_omitted,
            )
        }
        // Missing worktree or unresolved base/head ⇒ no live committed diff.
        _ => (Vec::new(), true, false, 0),
    };

    Ok(ReviewDiff {
        stale,
        groups,
        uncommitted: if stale {
            pkg.uncommitted_patch.clone()
        } else {
            None
        },
        diff_unavailable,
        truncated,
        files_omitted,
    })
}

/// Bucket live diff files by the stored intent groups, preserving group
/// display order; files not listed in any group fall into a trailing
/// "Other" group. Each file lands in the first group that lists it. Empty
/// groups are dropped so the UI never renders an empty section.
fn bucket_diff_by_groups(
    files: Vec<crate::review::LiveDiffFile>,
    groups: &[crate::review::IntentGroup],
) -> Vec<DiffGroup> {
    use std::collections::HashMap;

    // path → first group index that claims it.
    let mut owner: HashMap<&str, usize> = HashMap::new();
    for (gi, g) in groups.iter().enumerate() {
        for f in &g.files {
            owner.entry(f.as_str()).or_insert(gi);
        }
    }

    let mut buckets: Vec<Vec<DiffFile>> = (0..groups.len()).map(|_| Vec::new()).collect();
    let mut other: Vec<DiffFile> = Vec::new();
    for f in files {
        let df = DiffFile {
            path: f.path.clone(),
            patch: f.patch,
            truncated: f.truncated,
        };
        match owner.get(f.path.as_str()) {
            Some(&gi) => buckets[gi].push(df),
            None => other.push(df),
        }
    }

    let mut out: Vec<DiffGroup> = Vec::new();
    for (g, files) in groups.iter().zip(buckets) {
        if !files.is_empty() {
            out.push(DiffGroup {
                label: g.label.clone(),
                files,
            });
        }
    }
    if !other.is_empty() {
        out.push(DiffGroup {
            label: DIFF_OTHER_LABEL.to_string(),
            files: other,
        });
    }
    out
}

/// The human approve / request-changes transition from the webview
/// (PLAN §E.16). Shares the transition guard + atomic state/log/decision
/// writes with the NDJSON `review_decision_op` via
/// [`apply_review_decision`]. Returns the new estado string so the UI can
/// refresh without a follow-up read.
#[tauri::command]
pub async fn review_decision(
    state: State<'_, Arc<AppState>>,
    task_id: String,
    verdict: String,
    note: String,
) -> Result<String, String> {
    use cadenza_proto::ops::review_decision::Verdict;
    let verdict = match verdict.as_str() {
        "aprovado" => Verdict::Aprovado,
        "pedir_alteracoes" => Verdict::PedirAlteracoes,
        other => return Err(format!("invalid verdict: {other}")),
    };
    crate::review::apply_review_decision(state.repo.as_ref(), &task_id, verdict, &note)
        .await
        .map(|estado| estado.as_str().to_string())
        .map_err(|e| e.message)
}

/// Latest `evidence_state` per task, for the board badge (PLAN §E.15).
/// One round-trip instead of an `N+1` `get_review_package` per card: the
/// caller reads every package once and keeps the highest attempt per task.
#[tauri::command]
pub async fn list_review_states(
    state: State<'_, Arc<AppState>>,
) -> Result<HashMap<String, crate::review::EvidenceState>, String> {
    let all = state.repo.all_review_packages().await.map_err(to_str_err)?;
    // Keep the highest-attempt package per task.
    let mut latest_attempt: HashMap<String, u32> = HashMap::new();
    let mut states: HashMap<String, crate::review::EvidenceState> = HashMap::new();
    for pkg in all {
        let slot = latest_attempt.entry(pkg.task_id.clone()).or_insert(0);
        if pkg.attempt >= *slot {
            *slot = pkg.attempt;
            states.insert(pkg.task_id.clone(), pkg.evidence_state);
        }
    }
    Ok(states)
}
