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

/// Local branch names in `repo`, sorted by git's default order. Used to
/// populate the origin/destination pickers in the task modal. An empty
/// repo (no commits yet) yields an empty list rather than an error.
pub async fn list_branches(repo: &Path) -> Result<Vec<String>> {
    let out = run_git(repo, &["branch", "--format=%(refname:short)"]).await?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// The upstream ref for `branch` (e.g. `origin/main`), or `None` when the
/// branch has no configured upstream. `rev-parse @{upstream}` exits
/// non-zero without a tracked upstream, so this can't reuse `run_git`.
async fn upstream_of(repo: &Path, branch: &str) -> Result<Option<String>> {
    let spec = format!("{branch}@{{upstream}}");
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--abbrev-ref", "--verify", "--quiet", &spec])
        .output()
        .await
        .context("failed to run git (is git installed and on PATH?)")?;
    if !output.status.success() {
        return Ok(None);
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(if name.is_empty() { None } else { Some(name) })
}

/// Update the local `branch` from its remote, fast-forward only, blocking
/// on any real failure (non-ff, network, conflict). A branch with **no**
/// upstream is a no-op (returns `Ok`): a local-only repo must not fail the
/// agent start. When `branch` is the one checked out in `dir` we use
/// `git pull --ff-only` (git refuses to update a checked-out branch via a
/// fetch refspec); otherwise `git fetch <remote> <branch>:<branch>`, whose
/// refspec is itself ff-only and errors when the update isn't a
/// fast-forward.
pub async fn pull_branch(dir: &Path, branch: &str) -> Result<()> {
    let Some(upstream) = upstream_of(dir, branch).await? else {
        return Ok(());
    };
    // `origin/main` → remote = `origin`, remote-side branch = `main`. The
    // upstream branch name can differ from the local one (e.g. local `main`
    // tracking `origin/trunk`), so split the ref instead of assuming they
    // match. Default to `origin`/the local name if the split is unexpected
    // (e.g. a remote name containing no slash).
    let (remote, remote_branch) = upstream.split_once('/').unwrap_or(("origin", branch));
    let checked_out = current_branch(dir).await? == branch;
    if checked_out {
        run_git(dir, &["pull", "--ff-only"]).await?;
    } else {
        let refspec = format!("{remote_branch}:{branch}");
        run_git(dir, &["fetch", remote, &refspec]).await?;
    }
    Ok(())
}

/// Add a worktree at `path`. When `create_branch` is true the branch is
/// created (`-b`), based on `start_point` when given (e.g. the origin
/// branch) and otherwise the current HEAD. When false the existing
/// `branch` is checked out into the new worktree (`start_point` ignored).
pub async fn add_worktree(
    repo: &Path,
    path: &Path,
    branch: &str,
    create_branch: bool,
    start_point: Option<&str>,
) -> Result<()> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("worktree path is not valid UTF-8: {}", path.display()))?;
    if create_branch {
        let mut args = vec!["worktree", "add", "-b", branch, path_str];
        if let Some(sp) = start_point {
            args.push(sp);
        }
        run_git(repo, &args).await?;
    } else {
        run_git(repo, &["worktree", "add", path_str, branch]).await?;
    }
    Ok(())
}

/// Switch `dir` (a repo or a worktree) to `branch`. When `create` is true
/// the branch is created (`-c`), based on `start_point` when given (e.g.
/// the origin branch) and otherwise the current HEAD.
pub async fn switch_branch(
    dir: &Path,
    branch: &str,
    create: bool,
    start_point: Option<&str>,
) -> Result<()> {
    if create {
        let mut args = vec!["switch", "-c", branch];
        if let Some(sp) = start_point {
            args.push(sp);
        }
        run_git(dir, &args).await?;
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
    async fn add_and_switch_worktree() {
        let repo = init_repo();
        // Worktree lives in its own temp dir so it never collides with the repo.
        let holder = TempDir::new().unwrap();
        let wt = holder.path().join("wt-feature");

        assert!(!branch_exists(repo.path(), "feature").await.unwrap());
        add_worktree(repo.path(), &wt, "feature", true, None)
            .await
            .unwrap();
        assert!(wt.exists());
        assert!(branch_exists(repo.path(), "feature").await.unwrap());
        assert_eq!(current_branch(&wt).await.unwrap(), "feature");

        switch_branch(&wt, "other", true, None).await.unwrap();
        assert_eq!(current_branch(&wt).await.unwrap(), "other");
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
        add_worktree(repo.path(), &wt, "existing", false, None)
            .await
            .unwrap();
        assert_eq!(current_branch(&wt).await.unwrap(), "existing");
    }

    #[tokio::test]
    async fn list_branches_reports_locals() {
        let repo = init_repo();
        StdCommand::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["branch", "feature"])
            .status()
            .unwrap();
        let branches = list_branches(repo.path()).await.unwrap();
        assert!(branches.contains(&"main".to_string()));
        assert!(branches.contains(&"feature".to_string()));
    }

    #[tokio::test]
    async fn add_worktree_with_start_point_branches_off_it() {
        let repo = init_repo();
        // A second commit on `main` so `base` (created from the first commit)
        // is provably distinct from current HEAD.
        StdCommand::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["branch", "base"])
            .status()
            .unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["commit", "--allow-empty", "-m", "second"])
            .status()
            .unwrap();

        let holder = TempDir::new().unwrap();
        let wt = holder.path().join("wt-derived");
        // Create `derived` off `base` (not current HEAD).
        add_worktree(repo.path(), &wt, "derived", true, Some("base"))
            .await
            .unwrap();
        assert_eq!(current_branch(&wt).await.unwrap(), "derived");
        // `derived` points at `base`, one commit behind `main`'s tip.
        let base_rev = run_git(repo.path(), &["rev-parse", "base"]).await.unwrap();
        let derived_rev = run_git(&wt, &["rev-parse", "HEAD"]).await.unwrap();
        assert_eq!(base_rev, derived_rev);
    }

    #[tokio::test]
    async fn pull_branch_no_upstream_is_noop() {
        let repo = init_repo();
        // No remote configured → nothing to pull, must not error.
        pull_branch(repo.path(), "main").await.unwrap();
    }

    #[tokio::test]
    async fn pull_branch_fast_forwards_from_remote() {
        // A bare "remote" with one commit; a clone tracking it; the remote
        // advances; pull_branch fast-forwards the clone's checked-out main.
        let remote = TempDir::new().unwrap();
        let run = |dir: &Path, args: &[&str]| {
            let status = StdCommand::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(remote.path(), &["init", "--bare", "-b", "main"]);

        // Seed the remote via a throwaway working clone.
        let seed = TempDir::new().unwrap();
        let remote_url = remote.path().to_str().unwrap();
        run(seed.path(), &["clone", remote_url, "."]);
        run(seed.path(), &["config", "user.email", "t@e.com"]);
        run(seed.path(), &["config", "user.name", "T"]);
        run(seed.path(), &["commit", "--allow-empty", "-m", "c1"]);
        run(seed.path(), &["push", "origin", "main"]);

        // The clone under test, tracking the remote's main.
        let clone = TempDir::new().unwrap();
        run(clone.path(), &["clone", remote_url, "."]);
        let before = run_git(clone.path(), &["rev-parse", "HEAD"]).await.unwrap();

        // Advance the remote.
        run(seed.path(), &["commit", "--allow-empty", "-m", "c2"]);
        run(seed.path(), &["push", "origin", "main"]);

        // main is checked out in the clone → ff via `pull --ff-only`.
        pull_branch(clone.path(), "main").await.unwrap();
        let after = run_git(clone.path(), &["rev-parse", "HEAD"]).await.unwrap();
        assert_ne!(before, after, "pull should have advanced HEAD");
    }
}
