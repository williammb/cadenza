//! Aggregate review owned by a Jira issue (Slice 5).
//!
//! This is a **parallel** type to [`crate::review::ReviewPackage`], NOT a
//! variant of it. The per-task package is keyed by `(task_id, attempt)` with a
//! `NOT NULL` FK to `tasks(id)`; an issue-owned aggregate is keyed by
//! `(jira_site, jira_issue_id, attempt)` and owns the SHARED per-issue
//! worktree's committed branch diff. Keeping the types separate means existing
//! task packages, their table, sidecars, and trait methods are byte-for-byte
//! unchanged — old serialized data deserializes exactly as before (the
//! "default Task owner" is the untouched [`ReviewPackage`]).
//!
//! The aggregate is **branch-diff-only** (no per-task evidence/checks/risks)
//! and **state-neutral**: building and persisting it NEVER moves any subtask
//! through an estado, never appends a task log, and never calls any `done`
//! path. Every git invocation is the hardened, read-only [`GitCtx`]
//! (`GIT_OPTIONAL_LOCKS=0`), so even the git layer cannot mutate the worktree.

use serde::{Deserialize, Serialize};
use std::path::Path;

use super::git::GitCtx;
use crate::review::{CappedPatch, ChangedFile, CollectionError, FileChange};

/// Lifecycle of one aggregate review attempt. A parallel snake_case enum to
/// [`crate::review::PackageStatus`] with the SAME wire string values, so the
/// `(pending|superseded|aprovado|alteracoes_solicitadas)` CHECK constraint is
/// shared and a future decision verb maps cleanly. Decisions are out of
/// Slice-5 scope; the builder always emits `Pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssuePackageStatus {
    Pending,
    Superseded,
    Aprovado,
    AlteracoesSolicitadas,
}

impl IssuePackageStatus {
    /// Wire/column string. Mirrors the per-task `package_status_as_str`.
    pub fn as_str(self) -> &'static str {
        match self {
            IssuePackageStatus::Pending => "pending",
            IssuePackageStatus::Superseded => "superseded",
            IssuePackageStatus::Aprovado => "aprovado",
            IssuePackageStatus::AlteracoesSolicitadas => "alteracoes_solicitadas",
        }
    }
}

/// One aggregate (issue-owned) review attempt. Keyed by
/// `(jira_site, jira_issue_id, attempt)`. Branch-diff-only payload.
///
/// `#[serde(default)]` on every additive field so an aggregate written by an
/// older build still deserializes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueReviewPackage {
    // ── identity / lifecycle ─────────────────────────────────────────
    pub jira_site: String,
    pub jira_issue_id: String,
    pub attempt: u32,
    /// Aggregate dedup key (deterministic content key; see the command core).
    pub idempotency_key: String,
    pub status: IssuePackageStatus,

    // ── git provenance (the diff) ────────────────────────────────────
    pub branch_name: String,
    pub base_sha: String,
    #[serde(default)]
    pub head_sha: Option<String>,

    // ── branch diff (the only payload) ───────────────────────────────
    #[serde(default)]
    pub changed_files: Vec<ChangedFile>,
    #[serde(default)]
    pub files_added: u32,
    #[serde(default)]
    pub files_modified: u32,
    #[serde(default)]
    pub files_deleted: u32,
    #[serde(default)]
    pub diff: Option<CappedPatch>,
    #[serde(default)]
    pub truncated: bool,

    // ── observability ────────────────────────────────────────────────
    #[serde(default)]
    pub collection_errors: Vec<CollectionError>,
    pub created_at_ms: u64,
    #[serde(default)]
    pub collection_duration_ms: u64,
}

/// Typed build error for the aggregate review. Unlike the per-task
/// `build_package` (which never errs and folds everything into
/// `collection_errors`), these are precondition/irrecoverable failures the
/// command must surface to the caller.
#[derive(Debug, Clone)]
pub enum IssueReviewError {
    /// The record exists but is not a Ready worktree (state != Ready, or
    /// branch_name/worktree_path/base_sha missing, or the worktree path does
    /// not exist on disk).
    NotReady,
    /// `read_jira_issue` returned `None`.
    NotFound,
    /// The committed diff failed irrecoverably (both metadata reads failed, or
    /// the store read errored).
    DiffFailed(String),
}

impl IssueReviewError {
    /// Stable machine code for the IPC/CLI surface.
    pub fn code(&self) -> &'static str {
        match self {
            IssueReviewError::NotReady => "jira_worktree_not_ready",
            IssueReviewError::NotFound => "jira_not_found",
            IssueReviewError::DiffFailed(_) => "jira_review_failed",
        }
    }
}

impl std::fmt::Display for IssueReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IssueReviewError::NotReady => write!(
                f,
                "jira_worktree_not_ready: the issue's shared worktree is not Ready"
            ),
            IssueReviewError::NotFound => {
                write!(f, "jira_not_found: jira issue record not found")
            }
            IssueReviewError::DiffFailed(d) => write!(f, "jira_review_failed: {d}"),
        }
    }
}

impl std::error::Error for IssueReviewError {}

/// Wall-clock milliseconds since the Unix epoch (saturating).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Pure git builder over an already-resolved Ready worktree. Produces the
/// committed branch diff (`base_sha..HEAD`), capped/truncated like the
/// existing packages. NEVER touches any estado/store. `attempt`/
/// `idempotency_key`/`status` are left blank (attempt=0, key="",
/// status=Pending) for the caller to stamp — mirroring `build_package`.
///
/// State-neutral: every git call is read-only (hardened `GitCtx`); no estado
/// flip, no log append, no `done`.
pub async fn build_issue_review_from_worktree(
    jira_site: &str,
    jira_issue_id: &str,
    ready: &crate::jira::worktree::WorktreeReady,
) -> Result<IssueReviewPackage, IssueReviewError> {
    let started = std::time::Instant::now();
    let created_at_ms = now_ms();

    let git = GitCtx::new(Path::new(&ready.worktree_path));
    let mut collection_errors: Vec<CollectionError> = Vec::new();
    let mut truncated = false;

    // Resolve HEAD (non-fatal: fold failure into collection_errors). Uses the
    // plain `run` — `run_diff` would inject `--no-ext-diff`/`--no-textconv`,
    // which `rev-parse` treats as (bogus) revs.
    let head_sha = match git.run(&["rev-parse", "HEAD"]).await {
        Ok(out) => {
            truncated |= out.truncated;
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        Err(e) => {
            collection_errors.push(CollectionError {
                code: e.code.to_string(),
                detail: format!("HEAD rev-parse failed: {}", e.detail),
            });
            None
        }
    };

    let range = format!("{}..HEAD", ready.base_sha);

    // Changed-file metadata (committed range only).
    let collected = super::collect::collect_committed_range(&git, &range).await;
    truncated |= collected.truncated;
    let metadata_failed = collected.both_failed;
    collection_errors.extend(collected.errors.iter().cloned());
    let changed_files = collected.files;

    // Diff body.
    let mut body_failed = false;
    let diff = match git.run_diff(&["diff", &range]).await {
        Ok(out) => {
            truncated |= out.truncated;
            let text = String::from_utf8_lossy(&out.stdout);
            Some(super::patch::cap_unified_diff(&text, &[]))
        }
        Err(e) => {
            body_failed = true;
            collection_errors.push(CollectionError {
                code: "diff_unavailable".into(),
                detail: format!("committed diff body failed: {} ({})", e.detail, e.code),
            });
            None
        }
    };

    // If BOTH the metadata and the body failed, the diff is irrecoverable.
    if metadata_failed && body_failed {
        return Err(IssueReviewError::DiffFailed(format!(
            "committed diff {range} failed: both metadata and body reads errored"
        )));
    }

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

    Ok(IssueReviewPackage {
        jira_site: jira_site.to_string(),
        jira_issue_id: jira_issue_id.to_string(),
        attempt: 0,
        idempotency_key: String::new(),
        status: IssuePackageStatus::Pending,
        branch_name: ready.branch_name.clone(),
        base_sha: ready.base_sha.clone(),
        head_sha,
        changed_files,
        files_added,
        files_modified,
        files_deleted,
        diff,
        truncated,
        collection_errors,
        created_at_ms,
        collection_duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// Record-driven entry point used by the command. Resolves the record,
/// enforces the Ready guard (the SAME logic as `ensure_issue_worktree`'s
/// `ready_if_valid`), then delegates to the pure builder. Does NOT persist and
/// does NOT change any estado.
pub async fn build_issue_review(
    repo: &dyn crate::store::Repository,
    jira_site: &str,
    jira_issue_id: &str,
) -> Result<IssueReviewPackage, IssueReviewError> {
    let record = repo
        .read_jira_issue(jira_site, jira_issue_id)
        .await
        .map_err(|e| IssueReviewError::DiffFailed(e.to_string()))?
        .ok_or(IssueReviewError::NotFound)?;

    let ready = crate::jira::worktree::ready_if_valid(&record).ok_or(IssueReviewError::NotReady)?;

    build_issue_review_from_worktree(jira_site, jira_issue_id, &ready).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jira::worktree::WorktreeReady;
    use crate::store::Repository;
    use std::path::Path;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    fn run(dir: &Path, args: &[&str]) {
        let status = StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn out(dir: &Path, args: &[&str]) -> String {
        let o = StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    fn init_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        run(dir.path(), &["init"]);
        run(dir.path(), &["config", "user.email", "t@e.com"]);
        run(dir.path(), &["config", "user.name", "T"]);
        run(dir.path(), &["commit", "--allow-empty", "-m", "init"]);
        run(dir.path(), &["branch", "-M", "main"]);
        dir
    }

    fn ready_for(repo: &Path, base_sha: &str) -> WorktreeReady {
        WorktreeReady {
            branch_name: "jira/10001-x".into(),
            worktree_path: repo.to_string_lossy().into_owned(),
            base_sha: base_sha.into(),
        }
    }

    #[tokio::test]
    async fn build_issue_review_returns_branch_diff() {
        let repo = init_repo();
        // base = A
        std::fs::write(repo.path().join("keep.rs"), "one\n").unwrap();
        run(repo.path(), &["add", "."]);
        run(repo.path(), &["commit", "-m", "A"]);
        let base = out(repo.path(), &["rev-parse", "HEAD"]);

        // B: add a new file + modify keep.rs
        std::fs::write(repo.path().join("added.rs"), "new\n").unwrap();
        std::fs::write(repo.path().join("keep.rs"), "one\ntwo\n").unwrap();
        run(repo.path(), &["add", "."]);
        run(repo.path(), &["commit", "-m", "B"]);
        // C: delete keep.rs
        run(repo.path(), &["rm", "keep.rs"]);
        run(repo.path(), &["commit", "-m", "C"]);

        let ready = ready_for(repo.path(), &base);
        let pkg = build_issue_review_from_worktree("site", "10001", &ready)
            .await
            .unwrap();

        let paths: Vec<&str> = pkg.changed_files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"added.rs"), "got {paths:?}");
        // keep.rs added then deleted across the range => net Deleted.
        assert!(paths.contains(&"keep.rs"), "got {paths:?}");
        assert_eq!(pkg.files_added, 1);
        assert_eq!(pkg.files_deleted, 1);
        assert_eq!(pkg.base_sha, base);
        assert_eq!(
            pkg.head_sha.as_deref(),
            Some(out(repo.path(), &["rev-parse", "HEAD"]).as_str())
        );
        let diff = pkg.diff.expect("diff present");
        let dp: Vec<&str> = diff.files.iter().map(|f| f.path.as_str()).collect();
        assert!(dp.contains(&"added.rs"), "diff files {dp:?}");
        assert!(diff.files.iter().any(|f| f.patch.contains("+new")));
        assert!(
            pkg.collection_errors.is_empty(),
            "{:?}",
            pkg.collection_errors
        );
        // Identity left blank for the caller to stamp.
        assert_eq!(pkg.attempt, 0);
        assert!(pkg.idempotency_key.is_empty());
        assert_eq!(pkg.status, IssuePackageStatus::Pending);
    }

    #[tokio::test]
    async fn build_issue_review_empty_when_no_commits_past_base() {
        let repo = init_repo();
        std::fs::write(repo.path().join("x.rs"), "x\n").unwrap();
        run(repo.path(), &["add", "."]);
        run(repo.path(), &["commit", "-m", "A"]);
        let base = out(repo.path(), &["rev-parse", "HEAD"]);

        let ready = ready_for(repo.path(), &base);
        let pkg = build_issue_review_from_worktree("site", "10001", &ready)
            .await
            .unwrap();
        assert!(pkg.changed_files.is_empty());
        assert_eq!(pkg.files_added, 0);
        // Empty diff body => an empty CappedPatch (no files), no error.
        assert!(pkg.diff.map(|d| d.files.is_empty()).unwrap_or(true));
        assert!(pkg.collection_errors.is_empty());
    }

    #[tokio::test]
    async fn build_issue_review_missing_record_is_not_found() {
        let repo = init_repo();
        let home = TempDir::new().unwrap();
        let _ = &repo;
        let store = crate::store::FileRepository::new(home.path()).unwrap();
        let err = build_issue_review(&store, "site", "404").await.unwrap_err();
        assert!(matches!(err, IssueReviewError::NotFound));
        assert_eq!(err.code(), "jira_not_found");
    }

    #[tokio::test]
    async fn build_issue_review_not_ready_record_is_typed_error() {
        let home = TempDir::new().unwrap();
        let store = crate::store::FileRepository::new(home.path()).unwrap();
        let now = now_ms() as i64;
        // Record present but worktree_state not Ready (base_sha missing too).
        store
            .upsert_jira_issue(&cadenza_proto::JiraIssueRecord {
                jira_site: "site".into(),
                jira_issue_id: "10001".into(),
                jira_key: "PROJ-1".into(),
                project_id: None,
                analysis_run_id: None,
                secret_hash: None,
                secret_expiry_ms: None,
                secret_status: None,
                raw_adf: None,
                branch_name: Some("jira/10001-x".into()),
                worktree_path: None,
                base_sha: None,
                worktree_state: Some("creating".into()),
                created_at_ms: now,
                updated_at_ms: now,
            })
            .await
            .unwrap();
        let err = build_issue_review(&store, "site", "10001")
            .await
            .unwrap_err();
        assert!(matches!(err, IssueReviewError::NotReady));
        assert_eq!(err.code(), "jira_worktree_not_ready");
    }

    #[tokio::test]
    async fn build_issue_review_does_not_change_estado() {
        use crate::commands::AppState;
        use crate::config::Config;
        use crate::store::{Estado, FileRepository, Task};
        use std::sync::Arc;

        // A real repo with a commit past base, used as the issue worktree.
        let repo = init_repo();
        std::fs::write(repo.path().join("base.rs"), "b\n").unwrap();
        run(repo.path(), &["add", "."]);
        run(repo.path(), &["commit", "-m", "A"]);
        let base = out(repo.path(), &["rev-parse", "HEAD"]);
        std::fs::write(repo.path().join("work.rs"), "w\n").unwrap();
        run(repo.path(), &["add", "."]);
        run(repo.path(), &["commit", "-m", "B"]);

        let home = TempDir::new().unwrap();
        let repo_record = FileRepository::new(home.path()).unwrap();
        let state =
            AppState::for_test(home.path(), Arc::new(repo_record), Config::default()).unwrap();

        let site = "https://x.atlassian.net";
        let issue = "10001";
        let now = now_ms() as i64;
        // Ready jira record pointing at the real worktree.
        state
            .repo
            .upsert_jira_issue(&cadenza_proto::JiraIssueRecord {
                jira_site: site.into(),
                jira_issue_id: issue.into(),
                jira_key: "PROJ-1".into(),
                project_id: None,
                analysis_run_id: None,
                secret_hash: None,
                secret_expiry_ms: None,
                secret_status: None,
                raw_adf: None,
                branch_name: Some("jira/10001-x".into()),
                worktree_path: Some(repo.path().to_string_lossy().into_owned()),
                base_sha: Some(base.clone()),
                worktree_state: Some("ready".into()),
                created_at_ms: now,
                updated_at_ms: now,
            })
            .await
            .unwrap();

        // A subtask bound to the issue, sitting in `a_fazer`.
        let task = Task {
            id: "T-sub".into(),
            titulo: "sub".into(),
            estado: Estado::AFazer,
            responsavel: "humano".into(),
            body: "# T-sub\n".into(),
            worktree_path: None,
            branch: None,
            blocked_by: Vec::new(),
            jira_site: Some(site.into()),
            jira_issue_id: Some(issue.into()),
            jira_key_display: None,
        };
        state.repo.create_task(&task).await.unwrap();

        // Build + persist the aggregate review.
        let pkg = crate::commands::jira_review_core(&state, site, issue)
            .await
            .unwrap();
        assert_eq!(pkg.attempt, 1);
        assert!(!pkg.idempotency_key.is_empty());
        assert!(pkg.changed_files.iter().any(|f| f.path == "work.rs"));

        // STATE-NEUTRAL: the subtask estado is untouched.
        let back = state.repo.read_task("T-sub").await.unwrap();
        assert_eq!(back.estado, Estado::AFazer);

        // Re-running on the same branch state dedups to the same attempt.
        let again = crate::commands::jira_review_core(&state, site, issue)
            .await
            .unwrap();
        assert_eq!(again.attempt, 1);
        assert_eq!(
            state.repo.read_task("T-sub").await.unwrap().estado,
            Estado::AFazer
        );
    }
}
