//! Async git helpers for per-task worktrees and branch switching.
//!
//! These run git **non-interactively** and capture stdout/stderr via
//! `tokio::process` — distinct from `spawn.rs`, whose PTY path is for the
//! interactive agent. On failure the returned error embeds git's stderr so
//! the UI can surface a useful message. On Windows `git.exe` is resolved
//! from PATH; no batch wrapper is needed (git ships a real executable).

use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use tokio::process::Command;

/// Run `git -C <dir> <args...>` and return trimmed stdout on success.
/// A non-zero exit becomes an error carrying git's stderr (or stdout when
/// stderr is empty), so callers can show the user why git refused.
async fn run_git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to run git (is git installed and on PATH?): git {}",
                args.join(" ")
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        bail!("git {} failed: {}", args.join(" "), detail);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Short name of the branch currently checked out in `repo`, or an empty
/// string when `repo` is in detached-HEAD state. `--show-current` returns
/// "" while detached, where `rev-parse --abbrev-ref HEAD` would return the
/// literal "HEAD" — a bogus default that breaks `git worktree add -b HEAD`.
pub async fn current_branch(repo: &Path) -> Result<String> {
    run_git(repo, &["branch", "--show-current"]).await
}

/// Whether a local branch ref `refs/heads/<branch>` exists in `repo`.
/// Uses `--quiet`, which exits non-zero (without an error message) when
/// the ref is absent, so this can't reuse `run_git`'s bail-on-failure.
pub async fn branch_exists(repo: &Path, branch: &str) -> Result<bool> {
    let refname = format!("refs/heads/{branch}");
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", &refname])
        .output()
        .await
        .context("failed to run git (is git installed and on PATH?)")?;
    Ok(output.status.success())
}

/// Add a worktree at `path`. When `create_branch` is true the branch is
/// created off the current HEAD (`-b`); otherwise the existing `branch`
/// is checked out into the new worktree.
pub async fn add_worktree(
    repo: &Path,
    path: &Path,
    branch: &str,
    create_branch: bool,
) -> Result<()> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("worktree path is not valid UTF-8: {}", path.display()))?;
    if create_branch {
        run_git(repo, &["worktree", "add", "-b", branch, path_str]).await?;
    } else {
        run_git(repo, &["worktree", "add", path_str, branch]).await?;
    }
    Ok(())
}

/// Remove the worktree at `path`. Git refuses (with a clear message that
/// propagates through the error) when the worktree has uncommitted
/// changes — we deliberately do not pass `--force`.
pub async fn remove_worktree(repo: &Path, path: &Path) -> Result<()> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("worktree path is not valid UTF-8: {}", path.display()))?;
    run_git(repo, &["worktree", "remove", path_str]).await?;
    Ok(())
}

/// Switch `dir` (a repo or a worktree) to `branch`. When `create` is true
/// the branch is created off the current HEAD (`-c`).
pub async fn switch_branch(dir: &Path, branch: &str, create: bool) -> Result<()> {
    if create {
        run_git(dir, &["switch", "-c", branch]).await?;
    } else {
        run_git(dir, &["switch", branch]).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    /// Create a throwaway repo with one empty commit on branch `main`.
    fn init_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let status = StdCommand::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["commit", "--allow-empty", "-m", "init"]);
        // Normalize the branch name regardless of the host's init.defaultBranch.
        run(&["branch", "-M", "main"]);
        dir
    }

    #[tokio::test]
    async fn current_branch_reports_default() {
        let repo = init_repo();
        assert_eq!(current_branch(repo.path()).await.unwrap(), "main");
    }

    #[tokio::test]
    async fn branch_exists_true_and_false() {
        let repo = init_repo();
        assert!(branch_exists(repo.path(), "main").await.unwrap());
        assert!(!branch_exists(repo.path(), "does-not-exist").await.unwrap());
    }

    #[tokio::test]
    async fn add_switch_and_remove_worktree() {
        let repo = init_repo();
        // Worktree lives in its own temp dir so it never collides with the repo.
        let holder = TempDir::new().unwrap();
        let wt = holder.path().join("wt-feature");

        assert!(!branch_exists(repo.path(), "feature").await.unwrap());
        add_worktree(repo.path(), &wt, "feature", true)
            .await
            .unwrap();
        assert!(wt.exists());
        assert!(branch_exists(repo.path(), "feature").await.unwrap());
        assert_eq!(current_branch(&wt).await.unwrap(), "feature");

        switch_branch(&wt, "other", true).await.unwrap();
        assert_eq!(current_branch(&wt).await.unwrap(), "other");

        remove_worktree(repo.path(), &wt).await.unwrap();
        assert!(!wt.exists());
    }

    #[tokio::test]
    async fn add_worktree_for_existing_branch() {
        let repo = init_repo();
        // Pre-create the branch in the main repo.
        let status = StdCommand::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["branch", "existing"])
            .status()
            .unwrap();
        assert!(status.success());

        let holder = TempDir::new().unwrap();
        let wt = holder.path().join("wt-existing");
        add_worktree(repo.path(), &wt, "existing", false)
            .await
            .unwrap();
        assert_eq!(current_branch(&wt).await.unwrap(), "existing");
    }
}
