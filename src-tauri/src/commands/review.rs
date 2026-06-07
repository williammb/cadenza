//! Review-package command handlers + the shared human-decision transition
//! core — split out of the `commands` god-module. Pure relocation.
//! Re-exported via `commands`'s `pub use review::*;` so every existing
//! `commands::*` path still resolves unchanged (Tauri `generate_handler!`
//! in lib.rs; `crate::commands::apply_review_decision` in ipc.rs).

// Bring in the parent module's imports and shared helpers (AppState,
// to_str_err, Repository, Estado, StoreError, etc.). Parent-private items
// are visible to this child module.
use super::*;

/// Typed failure of [`apply_review_decision`]. Carries a stable machine
/// `code` so the IPC handler can map it to an `ErrorBody` and the Tauri
/// command can stringify it, without duplicating the transition guard.
pub(crate) struct ReviewDecisionError {
    /// Stable code: `bad_args`, `bad_state`, `task_not_found`, `internal`.
    pub code: &'static str,
    pub message: String,
}

impl ReviewDecisionError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Shared core of the human approve / request-changes transition
/// (PLAN §E.16). Both the NDJSON `review_decision_op` (CLI/agent surface)
/// and the `review_decision` Tauri command (webview surface) call this so
/// the transition guard lives in exactly one place.
///
/// Guard: `estado == aguardando_revisao` AND a latest non-terminal
/// (`Pending`) review package exists; otherwise `bad_state`. On success the
/// package decision mark, the `[revisão]` log line (both verdicts), and the
/// estado flip are applied in order decision → log → estado (each
/// idempotent, so a crash leaves a recoverable, never silently-finished
/// state) and the new [`Estado`] is returned.
pub(crate) async fn apply_review_decision(
    repo: &dyn Repository,
    task_id: &str,
    verdict: cadenza_proto::ops::review_decision::Verdict,
    note: &str,
) -> Result<Estado, ReviewDecisionError> {
    use crate::store::PackageStatus;
    use cadenza_proto::ops::review_decision::Verdict;

    crate::store::validate_id(task_id)
        .map_err(|e| ReviewDecisionError::new("bad_args", e.to_string()))?;

    // Guard: task must be awaiting review.
    let task = repo
        .read_task(task_id)
        .await
        .map_err(map_decision_store_err)?;
    if task.estado != Estado::AguardandoRevisao {
        return Err(ReviewDecisionError::new(
            "bad_state",
            "task is not awaiting review (estado != aguardando_revisao)",
        ));
    }

    // Guard: a latest, undecided (Pending) package must exist.
    let latest = repo
        .latest_review_package(task_id)
        .await
        .map_err(map_decision_store_err)?;
    let Some(latest) = latest.filter(|p| p.status == PackageStatus::Pending) else {
        return Err(ReviewDecisionError::new(
            "bad_state",
            "no pending review package to decide",
        ));
    };

    let (target_estado, decision, label) = match verdict {
        Verdict::Aprovado => (Estado::Feito, PackageStatus::Aprovado, "aprovado"),
        Verdict::PedirAlteracoes => (
            Estado::Fazendo,
            PackageStatus::AlteracoesSolicitadas,
            "pedir_alteracoes",
        ),
    };
    let log_line = if note.trim().is_empty() {
        format!("[revisão] {label}")
    } else {
        format!("[revisão] {label}: {note}")
    };

    // decision → log → estado: each idempotent; the order guarantees a
    // crash never leaves the task `feito`/`fazendo` while the package still
    // looks undecided.
    repo.set_package_decision(task_id, latest.attempt, decision)
        .await
        .map_err(map_decision_store_err)?;
    repo.append_log(task_id, &log_line)
        .await
        .map_err(map_decision_store_err)?;
    repo.set_estado(task_id, target_estado)
        .await
        .map_err(map_decision_store_err)?;

    Ok(target_estado)
}

fn map_decision_store_err(e: StoreError) -> ReviewDecisionError {
    match e {
        StoreError::NotFound(id) => ReviewDecisionError::new("task_not_found", id),
        StoreError::Busy => ReviewDecisionError::new("task_busy", e.to_string()),
        other => ReviewDecisionError::new("internal", other.to_string()),
    }
}

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
    apply_review_decision(state.repo.as_ref(), &task_id, verdict, &note)
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
