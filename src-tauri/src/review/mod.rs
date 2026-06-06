//! Review-package types (PLAN §C/§E/§F).
//!
//! One review package is persisted per *done attempt* (keyed by
//! `(task_id, attempt)`). These types live entirely in `src-tauri`: the
//! CLI never reads a `ReviewPackage` — it only *sends* evidence on `done`
//! (wire type `proto::ops::done::Evidence`) and the webview reads the
//! package back via the `get_review_package` Tauri command. This mirrors
//! the existing `Project` (app) vs `ProjectInfo` (wire) boundary.
//!
//! Storage/engine/command wiring is implemented in later layers; allow
//! dead_code until those consumers exist.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// Review engine submodules (PLAN §C.11). The engine is pure-ish: given a
// worktree path + branch + contract + reported evidence it runs hardened,
// read-only git and returns a `ReviewPackage` body. It NEVER depends on
// `AppState`; the wire layer fills persistence-only fields and applies the
// atomic done transaction.
mod base;
mod caps;
mod collect;
mod git;
pub mod issue;
mod patch;
mod risk;
mod state;

pub use caps::validate_and_cap_evidence;
// `CapError` is the error type of the re-exported `validate_and_cap_evidence`,
// so it must be publicly nameable; callers only ever stringify it.
#[allow(unused_imports)]
pub use caps::CapError;

/// The app-side `≤ MAX_CHECKS` evidence cap, exposed for tests that need to
/// build an over-cap payload without re-stating the constant.
#[cfg(test)]
pub fn caps_max_checks() -> usize {
    caps::MAX_CHECKS
}

use crate::config::QualityProfile;
use cadenza_proto::ops::done::Evidence;
use std::path::Path;

/// Inputs the wire layer assembles for one `done` attempt (PLAN §C.11).
/// All git work is scoped to `worktree_path`; `None` means the worktree is
/// missing, so git is skipped entirely and the state is derived from the
/// reported checks alone (PLAN §C.12).
pub struct CollectInputs<'a> {
    /// The task's worktree. `None` ⇒ missing worktree ⇒ skip git.
    pub worktree_path: Option<&'a Path>,
    /// The task's recorded branch; `None`/empty ⇒ HEAD + `branch_unavailable`.
    pub task_branch: Option<&'a str>,
    /// The project's configured default branch (base-resolution priority).
    pub project_default_branch: Option<&'a str>,
    /// The live per-project quality contract. `None` with
    /// `contract_resolved == false` ⇒ `contract_unavailable`.
    pub contract: Option<&'a QualityProfile>,
    /// Whether the project (hence the contract) resolved.
    pub contract_resolved: bool,
    /// Agent-reported evidence (already validated + capped by the caller).
    pub reported: Evidence,
}

/// Build the review-package body for one `done` attempt (PLAN §C.11/§C.12).
///
/// This NEVER returns `Err` for a git/heuristic failure: every such failure
/// is folded into `collection_errors` and the relevant fallback applies.
/// Persistence-only fields the caller fills afterward: `task_id`,
/// `idempotency_key`, `attempt`, `status`, `summary` (already echoed here
/// as empty), and the final lifecycle. `created_at_ms`/`collection_duration_ms`
/// and all derived snapshot fields are populated here.
pub async fn build_package(inputs: CollectInputs<'_>) -> ReviewPackage {
    use base::resolve_base;
    use collect::collect_changes;
    use patch::capture_uncommitted;
    use risk::assess;
    use state::{derive, StateInputs};

    let started = std::time::Instant::now();
    let created_at_ms = now_ms();

    let current_contract_version = inputs.contract.map(|c| c.contract_version());
    let reported_contract_version = inputs.reported.contract_version.clone();

    let mut collection_errors: Vec<CollectionError> = Vec::new();
    let mut truncated = false;
    let mut changed_files: Vec<ChangedFile> = Vec::new();
    let mut risks: Vec<RiskFlag> = Vec::new();
    let mut secret_matches: Vec<SecretMatch> = Vec::new();
    let mut base_sha: Option<String> = None;
    let mut head_sha: Option<String> = None;
    let mut worktree_fingerprint: Option<String> = None;
    let mut uncommitted_patch: Option<CappedPatch> = None;
    let scope_trustworthy;

    match inputs.worktree_path {
        None => {
            // Missing worktree (PLAN §C.12): skip git, scope is unknown.
            collection_errors.push(CollectionError {
                code: "missing_worktree".into(),
                detail: "no worktree recorded for task; git collection skipped".into(),
            });
            scope_trustworthy = false;
        }
        Some(dir) => {
            let git = git::GitCtx::new(dir);
            let base = resolve_base(&git, inputs.task_branch, inputs.project_default_branch).await;
            base_sha = base.base_sha.clone();
            head_sha = base.head_sha.clone();
            if let Some(reason) = &base.base_unresolved {
                collection_errors.push(CollectionError {
                    code: "base_unresolved".into(),
                    detail: format!("committed diff skipped: {reason}"),
                });
            }
            if base.branch_unavailable {
                collection_errors.push(CollectionError {
                    code: "branch_unavailable".into(),
                    detail: "task had no recorded branch; diffed against HEAD".into(),
                });
            }

            let collected = collect_changes(&git, &base).await;
            truncated |= collected.truncated;
            worktree_fingerprint = collected.fingerprint.clone();
            collection_errors.extend(collected.errors.iter().cloned());
            changed_files = collected.files;

            // Scope is trustworthy only when the whole change set was
            // observable: base resolved, no diff source failed, not clipped.
            scope_trustworthy = base.base_unresolved.is_none()
                && !collected.diff_unavailable
                && !collected.truncated;

            let outcome = assess(&git, &changed_files, &base).await;
            risks = outcome.risks;
            secret_matches = outcome.secret_findings;

            uncommitted_patch = Some(capture_uncommitted(&git, &secret_matches).await);
            if let Some(p) = &uncommitted_patch {
                truncated |= p.truncated;
            }
        }
    }

    // File counts by change kind.
    let mut files_added = 0u32;
    let mut files_modified = 0u32;
    let mut files_deleted = 0u32;
    for f in &changed_files {
        match f.change {
            FileChange::Added => files_added += 1,
            FileChange::Deleted => files_deleted += 1,
            FileChange::Renamed | FileChange::Modified => files_modified += 1,
        }
    }

    let changed_paths: Vec<String> = changed_files.iter().map(|f| f.path.clone()).collect();
    let state_inputs = StateInputs {
        contract: inputs.contract,
        contract_resolved: inputs.contract_resolved,
        current_contract_version: current_contract_version.clone(),
        reported: &inputs.reported,
        changed_paths: if scope_trustworthy {
            Some(&changed_paths)
        } else {
            None
        },
        scope_trustworthy,
        risks: &risks,
    };
    let derived = derive(&state_inputs);

    // Map reported evidence into persisted echo fields.
    let checks: Vec<ReportedCheck> = inputs
        .reported
        .checks
        .iter()
        .map(|c| ReportedCheck {
            id: c.id.clone(),
            exit: c.exit,
            log_excerpt: c.log_excerpt.clone(),
            log_path: c.log_path.clone(),
        })
        .collect();
    let groups: Vec<IntentGroup> = inputs
        .reported
        .groups
        .iter()
        .map(|g| IntentGroup {
            label: g.label.clone(),
            files: g.files.clone(),
        })
        .collect();

    ReviewPackage {
        task_id: String::new(),
        attempt: 0,
        idempotency_key: String::new(),
        status: PackageStatus::Pending,
        checks,
        groups,
        open_questions: inputs.reported.open_questions.clone(),
        summary: String::new(),
        changed_files,
        files_added,
        files_modified,
        files_deleted,
        risks,
        secret_matches,
        evidence_state: derived.state,
        needs_focused_human_review: derived.needs_focused_human_review,
        validation_scope_unknown: derived.validation_scope_unknown,
        base_sha,
        head_sha,
        worktree_fingerprint,
        contract_version: current_contract_version,
        reported_contract_version,
        risk_heuristic_version: RISK_HEURISTIC_VERSION,
        created_at_ms,
        collection_duration_ms: started.elapsed().as_millis() as u64,
        collection_errors,
        truncated,
        uncommitted_patch,
    }
}

/// One file's live-recomputed committed-diff text (PLAN §D.13). The patch
/// is the hardened `git diff base..head -- <path>` output, size-capped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveDiffFile {
    pub path: String,
    /// Unified-diff text, capped at [`LIVE_PER_FILE_CAP`].
    pub patch: String,
    /// True when the per-file cap clipped this file's patch.
    #[serde(default)]
    pub truncated: bool,
}

/// Result of recomputing the committed diff live from `base..head`
/// (PLAN §D.13). Files are returned in `changed_files` order; the caller
/// buckets them by the stored intent groups.
#[derive(Debug, Clone)]
pub struct LiveCommittedDiff {
    /// One entry per file with a non-empty `base..head` diff.
    pub files: Vec<LiveDiffFile>,
    /// True when a per-file or total cap clipped output.
    pub truncated: bool,
    /// Files dropped entirely by the ≤ [`LIVE_MAX_FILES`] / total cap.
    pub files_omitted: u32,
    /// Structured, non-fatal git failures (never aborts the read path).
    pub errors: Vec<CollectionError>,
}

/// Caps for the live committed-diff recompute, mirroring the persisted
/// uncommitted-patch caps so a UI render is bounded the same way.
const LIVE_TOTAL_CAP: usize = 512 * 1024;
const LIVE_PER_FILE_CAP: usize = 64 * 1024;
const LIVE_MAX_FILES: usize = 200;

/// Compute the current worktree/index fingerprint of `worktree_path`
/// (PLAN §C.11.b / §D.13): `sha256` of `git status --porcelain=v1 -z`.
///
/// Returns `None` when the status read fails (e.g. the worktree is gone);
/// the read path treats an unreadable fingerprint as "cannot compare" and
/// therefore not stale.
pub async fn worktree_fingerprint(worktree_path: &Path) -> Option<String> {
    let git = git::GitCtx::new(worktree_path);
    let out = git.run(&["status", "--porcelain=v1", "-z"]).await.ok()?;
    Some(collect::fingerprint_of(&out.stdout))
}

/// Recompute the committed diff live from immutable `base..head` shas
/// (PLAN §D.13), one hardened `git diff` per file, size-capped. Never
/// returns `Err`: per-file git failures are folded into `errors` and the
/// file is skipped, so a single bad path can't sink the whole render.
///
/// `paths` is the stored package's `changed_files` paths (committed +
/// uncommitted); only files with a non-empty `base..head` diff appear in
/// the result (uncommitted-only files naturally yield empty committed
/// diffs and are dropped). The caller buckets the result by intent groups.
pub async fn recompute_committed_diff(
    worktree_path: &Path,
    base_sha: &str,
    head_sha: &str,
    paths: &[String],
) -> LiveCommittedDiff {
    let git = git::GitCtx::new(worktree_path);
    let range = format!("{base_sha}..{head_sha}");

    let mut files: Vec<LiveDiffFile> = Vec::new();
    let mut errors: Vec<CollectionError> = Vec::new();
    let mut files_omitted = 0u32;
    let mut total_truncated = false;
    let mut total_bytes = 0usize;

    for path in paths {
        if files.len() >= LIVE_MAX_FILES {
            files_omitted = files_omitted.saturating_add(1);
            continue;
        }
        match git.run_diff(&["diff", &range, "--", path.as_str()]).await {
            Ok(out) => {
                if out.stdout.is_empty() {
                    // No committed change for this file (uncommitted-only).
                    continue;
                }
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                let mut truncated = out.truncated;
                if text.len() > LIVE_PER_FILE_CAP {
                    text.truncate(floor_char_boundary(&text, LIVE_PER_FILE_CAP));
                    truncated = true;
                }
                if total_bytes.saturating_add(text.len()) > LIVE_TOTAL_CAP {
                    // Total cap reached: drop the rest rather than clip
                    // mid-file (cleaner UI than a half hunk).
                    files_omitted = files_omitted.saturating_add(1);
                    total_truncated = true;
                    continue;
                }
                total_bytes = total_bytes.saturating_add(text.len());
                total_truncated |= truncated;
                files.push(LiveDiffFile {
                    path: path.clone(),
                    patch: text,
                    truncated,
                });
            }
            Err(e) => errors.push(CollectionError {
                code: e.code.to_string(),
                detail: e.detail,
            }),
        }
    }

    LiveCommittedDiff {
        files,
        truncated: total_truncated,
        files_omitted,
        errors,
    }
}

/// Largest byte index `≤ max` that lands on a UTF-8 char boundary, so a
/// truncated patch stays valid UTF-8. (`str::floor_char_boundary` is still
/// unstable, so we open-code it.)
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Wall-clock milliseconds since the Unix epoch (saturating).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Heuristic version stamped into every `ReviewPackage` so re-runs and
/// policy changes are traceable (PLAN §C.11.c, §22). Bump whenever the
/// risk/secret heuristics change their classification behavior.
pub const RISK_HEURISTIC_VERSION: u32 = 1;

/// Derived trust state of a done attempt (PLAN §C.11.d). Pure function of
/// the reported checks + current contract + changed-file scope; NEVER an
/// agent-declared field, NEVER a numeric score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceState {
    /// Project could not be resolved (PLAN §C.12).
    ContractUnavailable,
    /// Reported `contract_version` ≠ current (drift, PLAN §C.11.d).
    ContractChanged,
    /// No checks defined (absent/empty profile).
    NoValidation,
    /// Some required checks missing, or conditional scope unknown.
    Partial,
    /// All required checks exit 0.
    Passed,
    /// Any required check exited non-zero.
    Failed,
}

/// Heuristic risk labels over the changed set (PLAN §C.11.c). Versioned via
/// `ReviewPackage.risk_heuristic_version`; never blocks `done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskFlag {
    NewDependency,
    Migration,
    Auth,
    PublicContract,
    LargeFile,
    PossibleSecret,
}

/// Confidence of a secret-pattern match. Coarse buckets, not a score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretConfidence {
    Low,
    Medium,
    High,
}

/// A redacted possible-secret hit (PLAN §C.11.c). **The matched text is
/// NEVER stored** — only structural metadata, so the package can be
/// persisted and rendered safely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretMatch {
    /// Heuristic category (e.g. "aws_key", "private_key", "generic_token").
    pub kind: String,
    /// Repo-relative POSIX path of the file the hit was found in.
    pub file: String,
    /// 1-based line number within that file's added lines.
    pub line: u32,
    pub confidence: SecretConfidence,
}

/// Lifecycle of a done attempt (PLAN §F.18).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageStatus {
    /// Latest, awaiting a human decision.
    Pending,
    /// A newer done attempt replaced this one (kept, not deleted).
    Superseded,
    /// review_decision verdict = aprovado.
    Aprovado,
    /// review_decision verdict = pedir_alteracoes.
    AlteracoesSolicitadas,
}

/// Echoed agent-reported check (untrusted) persisted on the package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedCheck {
    pub id: String,
    pub exit: i32,
    #[serde(default)]
    pub log_excerpt: String,
    /// Display-only label; the app NEVER fetches it (PLAN §E.14).
    #[serde(default)]
    pub log_path: Option<String>,
}

/// Intent group: a labeled bucket of changed files the UI renders
/// (PLAN §E.14). Agent-supplied, untrusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentGroup {
    pub label: String,
    #[serde(default)]
    pub files: Vec<String>,
}

/// Kind of change applied to a file in the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// App-derived (trustworthy) record of one changed file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub change: FileChange,
    /// Origin path for renames; None otherwise.
    #[serde(default)]
    pub renamed_from: Option<String>,
    #[serde(default)]
    pub lines_added: u32,
    #[serde(default)]
    pub lines_deleted: u32,
    /// True for binary/oversized files skipped by line/secret heuristics.
    #[serde(default)]
    pub binary: bool,
}

/// Structured, non-fatal git/heuristic failure (PLAN §C.12, §22). Recorded,
/// never aborts `done`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionError {
    /// Stable machine code: "missing_worktree", "base_unresolved",
    /// "diff_unavailable", "branch_unavailable", "git_timeout", etc.
    pub code: String,
    /// English detail for logs/diagnostics.
    pub detail: String,
}

/// One file's redacted unified-diff text within a `CappedPatch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CappedPatchFile {
    pub path: String,
    /// Unified-diff text, secret-redacted, capped at 64 KiB.
    pub patch: String,
    #[serde(default)]
    pub truncated: bool,
}

/// Capped, secret-redacted snapshot of staged+unstaged+untracked work
/// (PLAN §C.11.e). Dedicated persisted-patch caps, distinct from the
/// agent-JSON caps in §B.7: total ≤ 512 KiB, per-file ≤ 64 KiB,
/// ≤ 200 files, binaries excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CappedPatch {
    pub files: Vec<CappedPatchFile>,
    /// True if any per-file or total cap clipped content.
    #[serde(default)]
    pub truncated: bool,
    /// Files dropped entirely by the ≤200-file / total-size cap.
    #[serde(default)]
    pub files_omitted: u32,
}

/// One persisted review package per *done attempt* (PLAN §C.9, §F.18).
/// Keyed by (task_id, attempt). Lives entirely in src-tauri — the CLI
/// never reads this; the webview does via `get_review_package`.
///
/// `serde(default)` on every additive/optional field so a package written
/// by an older build still deserializes; new builds re-derive missing data
/// on the next `done`.
///
/// `#[derive(... Eq ...)]` is safe here because no float fields exist; if a
/// float is ever added, drop `Eq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPackage {
    // ── identity / lifecycle ──────────────────────────────────────────
    pub task_id: String,
    /// Monotonic attempt number; a new `done` supersedes prior attempts.
    pub attempt: u32,
    /// Idempotency key of the done attempt that produced this package
    /// (PLAN §C.9). Part of done-attempt identity.
    pub idempotency_key: String,
    /// Lifecycle of this attempt (PLAN §F.18).
    pub status: PackageStatus,

    // ── reported (agent-supplied, untrusted) ─────────────────────────
    /// Echoed reported checks (id/exit/log_excerpt/optional log_path).
    #[serde(default)]
    pub checks: Vec<ReportedCheck>,
    /// Intent groups: file→label buckets the UI renders (PLAN §E.14).
    #[serde(default)]
    pub groups: Vec<IntentGroup>,
    #[serde(default)]
    pub open_questions: Vec<String>,
    /// Human summary (the positional `done` summary).
    #[serde(default)]
    pub summary: String,

    // ── snapshot (app-derived, trustworthy) ──────────────────────────
    #[serde(default)]
    pub changed_files: Vec<ChangedFile>,
    #[serde(default)]
    pub files_added: u32,
    #[serde(default)]
    pub files_modified: u32,
    #[serde(default)]
    pub files_deleted: u32,
    #[serde(default)]
    pub risks: Vec<RiskFlag>,
    #[serde(default)]
    pub secret_matches: Vec<SecretMatch>,
    pub evidence_state: EvidenceState,
    /// Overlay (PLAN §C.11.d): true when any of {auth, migration,
    /// public_contract, possible_secret} fired. Drives the UI "focused
    /// human review" banner regardless of evidence_state.
    pub needs_focused_human_review: bool,
    /// True when the changed-file set was unavailable/truncated so
    /// `required_if_changed` checks could not be evaluated (PLAN §C.11.d).
    /// When set, evidence_state can never be `Passed`.
    #[serde(default)]
    pub validation_scope_unknown: bool,

    // ── git provenance ───────────────────────────────────────────────
    #[serde(default)]
    pub base_sha: Option<String>,
    #[serde(default)]
    pub head_sha: Option<String>,
    /// Hash of `git status --porcelain=v1 -z` (PLAN §C.11.b) — used to
    /// detect a moved worktree at read time (PLAN §D.13).
    #[serde(default)]
    pub worktree_fingerprint: Option<String>,

    // ── versions / drift ─────────────────────────────────────────────
    /// Current contract hash at done time (PLAN §A.2).
    #[serde(default)]
    pub contract_version: Option<String>,
    /// Contract hash the agent *reported* against (drift detection).
    #[serde(default)]
    pub reported_contract_version: Option<String>,
    pub risk_heuristic_version: u32,

    // ── timestamps ───────────────────────────────────────────────────
    pub created_at_ms: u64,
    #[serde(default)]
    pub collection_duration_ms: u64,

    // ── observability / partial-collection markers ───────────────────
    #[serde(default)]
    pub collection_errors: Vec<CollectionError>,
    #[serde(default)]
    pub truncated: bool,

    // ── capped, redacted uncommitted patch (PLAN §C.11.e, §D.13) ─────
    #[serde(default)]
    pub uncommitted_patch: Option<CappedPatch>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(v: impl Serialize) -> String {
        serde_json::to_string(&v).unwrap()
    }

    #[test]
    fn enum_wire_forms_are_snake_case() {
        assert_eq!(
            wire(EvidenceState::ContractUnavailable),
            "\"contract_unavailable\""
        );
        assert_eq!(wire(EvidenceState::NoValidation), "\"no_validation\"");
        assert_eq!(wire(RiskFlag::PossibleSecret), "\"possible_secret\"");
        assert_eq!(wire(RiskFlag::PublicContract), "\"public_contract\"");
        assert_eq!(
            wire(PackageStatus::AlteracoesSolicitadas),
            "\"alteracoes_solicitadas\""
        );
        assert_eq!(wire(FileChange::Renamed), "\"renamed\"");
        assert_eq!(wire(SecretConfidence::High), "\"high\"");
    }

    #[test]
    fn review_package_roundtrips() {
        let pkg = ReviewPackage {
            task_id: "T-1".into(),
            attempt: 1,
            idempotency_key: "k1".into(),
            status: PackageStatus::Pending,
            checks: vec![ReportedCheck {
                id: "clippy".into(),
                exit: 0,
                log_excerpt: "ok".into(),
                log_path: None,
            }],
            groups: vec![],
            open_questions: vec![],
            summary: "did the thing".into(),
            changed_files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                change: FileChange::Modified,
                renamed_from: None,
                lines_added: 3,
                lines_deleted: 1,
                binary: false,
            }],
            files_added: 0,
            files_modified: 1,
            files_deleted: 0,
            risks: vec![RiskFlag::Auth],
            secret_matches: vec![],
            evidence_state: EvidenceState::Passed,
            needs_focused_human_review: true,
            validation_scope_unknown: false,
            base_sha: Some("abc".into()),
            head_sha: Some("def".into()),
            worktree_fingerprint: None,
            contract_version: Some("sha256:00".into()),
            reported_contract_version: Some("sha256:00".into()),
            risk_heuristic_version: RISK_HEURISTIC_VERSION,
            created_at_ms: 123,
            collection_duration_ms: 4,
            collection_errors: vec![],
            truncated: false,
            uncommitted_patch: None,
        };
        let json = serde_json::to_string(&pkg).unwrap();
        let back: ReviewPackage = serde_json::from_str(&json).unwrap();
        assert_eq!(pkg, back);
    }

    /// Slice 5 additivity proof: a stored-format task package carries NO owner
    /// discriminator (we deliberately did NOT add a `ReviewOwner` enum), so an
    /// older serialized payload deserializes unchanged. Only the required
    /// fields are present; every additive field falls back via `serde(default)`.
    #[test]
    fn old_task_package_still_deserializes() {
        let stored = r#"{
            "task_id": "T-7",
            "attempt": 3,
            "idempotency_key": "k-old",
            "status": "pending",
            "evidence_state": "passed",
            "needs_focused_human_review": false,
            "risk_heuristic_version": 1,
            "created_at_ms": 999
        }"#;
        let pkg: ReviewPackage = serde_json::from_str(stored).unwrap();
        assert_eq!(pkg.task_id, "T-7");
        assert_eq!(pkg.attempt, 3);
        assert_eq!(pkg.status, PackageStatus::Pending);
        assert_eq!(pkg.evidence_state, EvidenceState::Passed);
        // Additive fields defaulted.
        assert!(pkg.changed_files.is_empty());
        assert!(pkg.uncommitted_patch.is_none());
        assert!(!pkg.truncated);
    }
}
