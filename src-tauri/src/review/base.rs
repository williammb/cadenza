//! Branch-ref and base-commit resolution for the review engine
//! (PLAN §C.11.a).
//!
//! A *missing worktree* is the only thing that skips git entirely (handled
//! by the orchestrator). A worktree with a missing/unknown branch is still
//! diffed: we fall back to `HEAD` and record `branch_unavailable`.

use super::git::GitCtx;

/// Resolved branch + base for the committed-diff portion of collection.
#[derive(Debug, Clone)]
pub(crate) struct BaseResolution {
    /// The ref we treat as the branch tip: the task branch when set,
    /// otherwise `HEAD`.
    pub branch_ref: String,
    /// True when the task had no recorded branch and we fell back to HEAD.
    pub branch_unavailable: bool,
    /// Resolved base commit (merge-base or single-commit HEAD). `None`
    /// when `base_unresolved` is set.
    pub base_sha: Option<String>,
    /// Resolved head commit (`branch_ref`'s tip). `None` when unreadable.
    pub head_sha: Option<String>,
    /// Reason the committed-diff portion must be skipped, when set
    /// (e.g. `"no_base"`). When `Some`, callers treat the changed-file
    /// scope as untrustworthy for conditional checks.
    pub base_unresolved: Option<String>,
}

impl BaseResolution {
    /// Whether the committed `base..head` diff can be computed.
    pub(crate) fn committed_available(&self) -> bool {
        self.base_unresolved.is_none() && self.base_sha.is_some() && self.head_sha.is_some()
    }
}

/// Resolve the repo's default branch (PLAN §C.11.a) in priority order:
/// 1. `project_default` (explicit per-project setting),
/// 2. `refs/remotes/origin/HEAD` (the remote's default),
/// 3. local `init.defaultBranch`,
/// 4. current `HEAD`'s short name.
///
/// Returns a short branch name (e.g. `"main"`) or `None` when nothing
/// resolves (e.g. detached HEAD with no remote default).
pub(crate) async fn resolve_default_branch(
    git: &GitCtx,
    project_default: Option<&str>,
) -> Option<String> {
    if let Some(d) = project_default {
        let d = d.trim();
        if !d.is_empty() {
            return Some(d.to_string());
        }
    }
    // refs/remotes/origin/HEAD → "origin/main"; strip the remote prefix.
    if let Some(sym) = git
        .line(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .await
    {
        if let Some((_, branch)) = sym.split_once('/') {
            if !branch.is_empty() {
                return Some(branch.to_string());
            }
        } else if !sym.is_empty() {
            return Some(sym);
        }
    }
    if let Some(d) = git.line(&["config", "--get", "init.defaultBranch"]).await {
        if !d.is_empty() {
            return Some(d);
        }
    }
    // Current HEAD short name (empty/"HEAD" when detached → treat as None).
    if let Some(name) = git.line(&["rev-parse", "--abbrev-ref", "HEAD"]).await {
        if name != "HEAD" && !name.is_empty() {
            return Some(name);
        }
    }
    None
}

/// Resolve the branch ref and base commit for committed-diff collection
/// (PLAN §C.11.a).
///
/// - `branch_ref` = `task_branch` when set, else `HEAD` (+ `branch_unavailable`).
/// - base order: `merge-base(branch_ref, project default)` →
///   `merge-base(branch_ref, repo default)` → `HEAD` (single-commit repo) →
///   `base_unresolved`.
///
/// `default_branch` is the project's configured default; `repo_default`
/// is resolved separately (so the two merge-base attempts can differ).
pub(crate) async fn resolve_base(
    git: &GitCtx,
    task_branch: Option<&str>,
    project_default: Option<&str>,
) -> BaseResolution {
    let (branch_ref, branch_unavailable) = match task_branch.map(str::trim) {
        Some(b) if !b.is_empty() => (b.to_string(), false),
        _ => ("HEAD".to_string(), true),
    };

    let head_sha = git.line(&["rev-parse", &branch_ref]).await;

    // Candidate default branches, de-duplicated in priority order.
    let mut candidates: Vec<String> = Vec::new();
    if let Some(d) = project_default.map(str::trim) {
        if !d.is_empty() {
            candidates.push(d.to_string());
        }
    }
    if let Some(repo_default) = resolve_default_branch(git, None).await {
        if !candidates.contains(&repo_default) {
            candidates.push(repo_default);
        }
    }

    // Try merge-base against each candidate default. A candidate equal to
    // branch_ref yields branch_ref itself (diff would be empty) — still a
    // valid, trustworthy base, so we accept it.
    let mut base_sha: Option<String> = None;
    for cand in &candidates {
        if let Some(mb) = git.line(&["merge-base", &branch_ref, cand]).await {
            base_sha = Some(mb);
            break;
        }
    }

    // Single-commit / no-default fallback: if HEAD has no parent, the base
    // IS head (whole history is the change set). `rev-parse HEAD~1` failing
    // signals a root commit.
    let mut base_unresolved: Option<String> = None;
    if base_sha.is_none() {
        let has_parent = git
            .line(&["rev-parse", "--verify", "HEAD~1"])
            .await
            .is_some();
        if !has_parent {
            // Single commit: base = head; the committed diff is empty but
            // the scope is trustworthy.
            base_sha = head_sha.clone();
        } else {
            base_unresolved = Some("no_base".to_string());
        }
    }

    BaseResolution {
        branch_ref,
        branch_unavailable,
        base_sha,
        head_sha,
        base_unresolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn init_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        run(dir.path(), &["init"]);
        run(dir.path(), &["config", "user.email", "t@e.com"]);
        run(dir.path(), &["config", "user.name", "T"]);
        run(dir.path(), &["commit", "--allow-empty", "-m", "init"]);
        run(dir.path(), &["branch", "-M", "main"]);
        dir
    }

    #[tokio::test]
    async fn single_commit_base_equals_head() {
        let repo = init_repo();
        let git = GitCtx::new(repo.path());
        let res = resolve_base(&git, Some("main"), None).await;
        assert!(res.base_unresolved.is_none());
        assert_eq!(res.base_sha, res.head_sha);
        assert!(!res.branch_unavailable);
    }

    #[tokio::test]
    async fn missing_branch_falls_back_to_head() {
        let repo = init_repo();
        let git = GitCtx::new(repo.path());
        let res = resolve_base(&git, None, None).await;
        assert_eq!(res.branch_ref, "HEAD");
        assert!(res.branch_unavailable);
    }

    #[tokio::test]
    async fn merge_base_resolves_against_default() {
        let repo = init_repo();
        // Second commit on main, then a feature branch off the first commit.
        run(repo.path(), &["branch", "feature"]);
        run(repo.path(), &["commit", "--allow-empty", "-m", "main2"]);
        run(repo.path(), &["checkout", "feature"]);
        run(repo.path(), &["commit", "--allow-empty", "-m", "feat1"]);
        let git = GitCtx::new(repo.path());
        let res = resolve_base(&git, Some("feature"), Some("main")).await;
        assert!(res.committed_available());
        // base = merge-base(feature, main) = the init commit.
        let init = git.line(&["rev-list", "--max-parents=0", "HEAD"]).await;
        assert_eq!(res.base_sha, init);
    }

    #[tokio::test]
    async fn default_branch_prefers_project_override() {
        let repo = init_repo();
        let git = GitCtx::new(repo.path());
        let d = resolve_default_branch(&git, Some("trunk")).await;
        assert_eq!(d.as_deref(), Some("trunk"));
    }
}
