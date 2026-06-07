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

/// A `git` command with the Windows console window suppressed (see
/// `spawn::CREATE_NO_WINDOW`). Cadenza is a windowless GUI process, so a plain
/// `git.exe` spawn would pop a console window for a fraction of a second;
/// routing every non-interactive git spawn through here avoids that.
fn git_command() -> Command {
    // `mut` is only used on Windows, where `creation_flags` borrows mutably.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = Command::new("git");
    #[cfg(windows)]
    cmd.creation_flags(crate::spawn::CREATE_NO_WINDOW);
    cmd
}

/// Run `git -C <dir> <args...>` and return trimmed stdout on success.
/// A non-zero exit becomes an error carrying git's stderr (or stdout when
/// stderr is empty), so callers can show the user why git refused.
async fn run_git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = git_command()
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
    let output = git_command()
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
    let output = git_command()
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

/// Resolve `rev` (e.g. `"HEAD"`, a branch, or a tag) to its full commit
/// sha in `dir` (a repo or worktree). Thin wrapper over `git rev-parse`,
/// used to capture a worktree's base sha at creation time.
pub async fn rev_parse(dir: &Path, rev: &str) -> Result<String> {
    run_git(dir, &["rev-parse", rev]).await
}

/// Best-effort `git worktree prune` in `repo` — clears stale worktree
/// administrative entries after a partial/failed `add_worktree` whose
/// directory was removed. Errors are returned so callers can log them, but
/// the cleanup path treats this as advisory.
pub async fn worktree_prune(repo: &Path) -> Result<()> {
    run_git(repo, &["worktree", "prune"]).await.map(|_| ())
}

/// `git -C <wt> status --porcelain` in a worktree. Returns the affected-path
/// lines — tracked changes AND untracked files. An empty vec means clean.
/// Used by `jira_discard` to refuse a dirty worktree unless `force=true`.
/// `--porcelain` (unlike `diff --quiet`) reports untracked files too, so a
/// brand-new file the agent left behind still counts as dirty.
pub async fn worktree_dirty_files(wt: &Path) -> Result<Vec<String>> {
    let out = run_git(wt, &["status", "--porcelain"]).await?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// `git -C <repo> worktree remove <path>` (no `--force` by default; pass
/// `force=true` for `--force`). Git itself refuses to remove a dirty worktree
/// without `--force`, so this errors on a dirty tree when `!force`. The caller
/// is expected to run [`worktree_prune`] afterwards to clear any stale admin
/// entry.
pub async fn remove_worktree(repo: &Path, path: &Path, force: bool) -> Result<()> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("worktree path is not valid UTF-8: {}", path.display()))?;
    let mut args = vec!["worktree", "remove", path_str];
    if force {
        args.push("--force");
    }
    run_git(repo, &args).await?;
    Ok(())
}

// ─── checkpoints / rollback (feature #6) ───────────────────────────
//
// A checkpoint captures the FULL working-tree state (tracked + untracked,
// minus .gitignored) of a repo/worktree into a commit object anchored under
// `refs/cadenza/checkpoints/...`, WITHOUT touching HEAD, the index, or the
// working tree. Restoring rewinds the working tree back to a snapshot. These
// are the primitives behind "revert this run" — non-destructive by
// construction (anchored refs, no force-reset of a branch).

/// Run `git -C <dir> <args...>` with a custom `GIT_INDEX_FILE`, so a
/// checkpoint can stage into a throwaway index without disturbing the real
/// one. Same success/stderr handling as [`run_git`].
async fn run_git_with_index(dir: &Path, index: &Path, args: &[&str]) -> Result<String> {
    let output = git_command()
        .arg("-C")
        .arg(dir)
        .env("GIT_INDEX_FILE", index)
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to run git: git {}", args.join(" ")))?;
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

/// Snapshot the current working-tree state of `dir` into a commit anchored at
/// `refname` (e.g. `refs/cadenza/checkpoints/T-42/<uuid>`). Captures tracked
/// AND untracked files (respecting `.gitignore`), via a throwaway index so the
/// real index, HEAD, and working tree are left untouched. The anchored ref
/// keeps the commit from being garbage-collected. Returns the commit sha.
pub async fn create_checkpoint(dir: &Path, refname: &str) -> Result<String> {
    let index = std::env::temp_dir().join(format!("cadenza-cp-{}.index", uuid::Uuid::new_v4()));
    // Seed the throwaway index from HEAD, then overlay the working tree
    // (`add -A` records modifications, additions, and deletions). The result
    // is a tree that mirrors the current working state.
    run_git_with_index(dir, &index, &["read-tree", "HEAD"]).await?;
    let add_res = run_git_with_index(dir, &index, &["add", "-A"]).await;
    let tree_res = match add_res {
        Ok(_) => run_git_with_index(dir, &index, &["write-tree"]).await,
        Err(e) => Err(e),
    };
    // Always clean up the throwaway index, success or failure.
    let _ = std::fs::remove_file(&index);
    let tree = tree_res?;
    let head = rev_parse(dir, "HEAD").await?;
    let commit = run_git(
        dir,
        &[
            "commit-tree",
            &tree,
            "-p",
            &head,
            "-m",
            "cadenza checkpoint",
        ],
    )
    .await?;
    run_git(dir, &["update-ref", refname, &commit]).await?;
    Ok(commit)
}

/// Snapshot the CURRENT real index of `dir` (staged content) into a commit
/// anchored at `refname`, so staged-but-uncommitted blobs stay reachable
/// across a [`restore_checkpoint`] (which overwrites the index). Best-effort
/// companion to [`create_checkpoint`] (which captures the working tree but
/// records working-tree blobs, not the staged ones). Returns the commit sha.
pub async fn checkpoint_index(dir: &Path, refname: &str) -> Result<String> {
    // `write-tree` operates on the real index; it writes whatever is staged.
    let tree = run_git(dir, &["write-tree"]).await?;
    let head = rev_parse(dir, "HEAD").await?;
    let commit = run_git(
        dir,
        &[
            "commit-tree",
            &tree,
            "-p",
            &head,
            "-m",
            "cadenza pre-revert index",
        ],
    )
    .await?;
    run_git(dir, &["update-ref", refname, &commit]).await?;
    Ok(commit)
}

/// Whether `dir` is a LINKED worktree (`git worktree add`) rather than the
/// main repository. In a linked worktree the per-worktree git dir differs
/// from the common dir; in the main repo they are identical. Drives how
/// aggressively [`restore_checkpoint`] cleans (a disposable worktree can be
/// rewound harder than a human's main repo).
pub async fn is_linked_worktree(dir: &Path) -> Result<bool> {
    let git_dir = run_git(dir, &["rev-parse", "--git-dir"]).await?;
    let common = run_git(dir, &["rev-parse", "--git-common-dir"]).await?;
    Ok(git_dir != common)
}

/// Rewind the working tree of `dir` to the snapshot in `commit` (a checkpoint
/// created by [`create_checkpoint`]). Tracked files are restored to the
/// snapshot, files deleted since are brought back, and files added since are
/// removed. NON-destructive to history: HEAD and the branch ref are NOT moved;
/// the restored state shows up as ordinary uncommitted working changes.
///
/// `remove_nested` controls `git clean`: `true` (`clean -ff -d`, for a
/// disposable worktree) also removes nested git repos added since the
/// snapshot; `false` (`clean -f -d`, for the human's main repo) leaves nested
/// repos in place. Either way `.gitignore`d files are kept (no `-x`).
///
/// Returns the list of UNTRACKED paths still present after the rewind (`??`
/// status lines) — empty means a complete rewind; non-empty signals a PARTIAL
/// one (e.g. a nested repo `clean` refused to remove) the caller should warn
/// about.
///
/// CAUTION: this overwrites the working tree and deletes added-since untracked
/// files. Callers MUST snapshot the current state first (see the revert
/// command) so the rewind is itself reversible.
pub async fn restore_checkpoint(
    dir: &Path,
    commit: &str,
    remove_nested: bool,
) -> Result<Vec<String>> {
    let tree = rev_parse(dir, &format!("{commit}^{{tree}}")).await?;
    // index ← snapshot tree (working tree untouched yet)
    run_git(dir, &["read-tree", &tree]).await?;
    // working tree ← index (overwrites modified/restores deleted snapshot files)
    run_git(dir, &["checkout-index", "-a", "-f"]).await?;
    // drop files that exist now but aren't in the snapshot (added since).
    // -d removes now-empty dirs; .gitignored files are kept (no -x). A second
    // force (-ff) is needed to remove nested git repos, which we only do in a
    // disposable worktree — never in the user's main repo.
    let clean_args: &[&str] = if remove_nested {
        &["clean", "-ff", "-d"]
    } else {
        &["clean", "-f", "-d"]
    };
    run_git(dir, clean_args).await?;
    // Detect leftovers WHILE the index still equals the snapshot tree: a clean
    // working tree shows no status, so any `??` here is a path that couldn't be
    // removed (e.g. a nested git repo) — a PARTIAL rewind. Must run before the
    // `read-tree HEAD` below, which would otherwise flag legitimately-restored
    // snapshot-untracked files (not in HEAD) as false `??`.
    let leftovers = run_git(dir, &["status", "--porcelain"])
        .await?
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("??"))
        .map(str::to_string)
        .collect();
    // index ← HEAD so the rewound state reads as unstaged working changes,
    // not a giant staged diff. Working tree is left as the snapshot.
    run_git(dir, &["read-tree", "HEAD"]).await?;
    Ok(leftovers)
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

    /// Init a repo and add a clean worktree off `main`; returns (repo, holder,
    /// worktree path). The holder TempDir keeps the worktree dir alive.
    async fn repo_with_worktree() -> (TempDir, TempDir, std::path::PathBuf) {
        let repo = init_repo();
        let holder = TempDir::new().unwrap();
        let wt = holder.path().join("wt");
        add_worktree(repo.path(), &wt, "feature", true, None)
            .await
            .unwrap();
        (repo, holder, wt)
    }

    #[tokio::test]
    async fn worktree_dirty_files_empty_on_clean() {
        let (_repo, _holder, wt) = repo_with_worktree().await;
        let dirty = worktree_dirty_files(&wt).await.unwrap();
        assert!(dirty.is_empty(), "expected clean worktree, got: {dirty:?}");
    }

    #[tokio::test]
    async fn worktree_dirty_files_reports_untracked() {
        let (_repo, _holder, wt) = repo_with_worktree().await;
        // An untracked file is invisible to `diff --quiet` but `--porcelain`
        // must catch it (this is the whole reason we use status --porcelain).
        std::fs::write(wt.join("scratch.txt"), b"hello").unwrap();
        let dirty = worktree_dirty_files(&wt).await.unwrap();
        assert!(
            dirty.iter().any(|l| l.contains("scratch.txt")),
            "expected untracked file reported, got: {dirty:?}"
        );
    }

    #[tokio::test]
    async fn remove_worktree_refuses_dirty_without_force() {
        let (repo, _holder, wt) = repo_with_worktree().await;
        std::fs::write(wt.join("scratch.txt"), b"hi").unwrap();
        let res = remove_worktree(repo.path(), &wt, false).await;
        assert!(res.is_err(), "remove should refuse a dirty worktree");
        assert!(wt.exists(), "worktree dir must survive a refused remove");
    }

    #[tokio::test]
    async fn remove_worktree_force_succeeds() {
        let (repo, _holder, wt) = repo_with_worktree().await;
        std::fs::write(wt.join("scratch.txt"), b"hi").unwrap();
        remove_worktree(repo.path(), &wt, true).await.unwrap();
        assert!(!wt.exists(), "worktree dir must be gone after force remove");
    }

    /// Stage + commit a file in `dir` (test helper).
    fn commit_file(dir: &Path, name: &str, content: &str, msg: &str) {
        std::fs::write(dir.join(name), content).unwrap();
        let run = |args: &[&str]| {
            let ok = StdCommand::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["add", name]);
        run(&["commit", "-m", msg]);
    }

    #[tokio::test]
    async fn checkpoint_captures_and_restores_working_tree() {
        let repo = init_repo();
        let dir = repo.path();
        commit_file(dir, "a.txt", "1", "add a");

        // Working state to snapshot: a.txt modified + an untracked b.txt.
        std::fs::write(dir.join("a.txt"), "2").unwrap();
        std::fs::write(dir.join("b.txt"), "new").unwrap();

        let refname = "refs/cadenza/checkpoints/T-1/cp1";
        let commit = create_checkpoint(dir, refname).await.unwrap();
        assert!(!commit.is_empty());

        // create_checkpoint must NOT touch the working tree or HEAD.
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "2");
        assert_eq!(current_branch(dir).await.unwrap(), "main");

        // Diverge: change a.txt again, add c.txt, delete b.txt.
        std::fs::write(dir.join("a.txt"), "3").unwrap();
        std::fs::write(dir.join("c.txt"), "later").unwrap();
        std::fs::remove_file(dir.join("b.txt")).unwrap();

        let leftovers = restore_checkpoint(dir, &commit, true).await.unwrap();
        assert!(
            leftovers.is_empty(),
            "expected complete rewind, got {leftovers:?}"
        );

        // Modified-since file rewound, deleted-since file restored, added-since
        // file removed.
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "2",
            "modified file rewound to snapshot"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("b.txt")).unwrap(),
            "new",
            "deleted-since file restored from snapshot"
        );
        assert!(
            !dir.join("c.txt").exists(),
            "file added after the snapshot must be removed"
        );
        // The branch ref was NOT moved (non-destructive to history).
        assert_eq!(current_branch(dir).await.unwrap(), "main");
    }

    #[tokio::test]
    async fn restore_is_noop_when_nothing_changed() {
        let repo = init_repo();
        let dir = repo.path();
        commit_file(dir, "a.txt", "1", "add a");
        std::fs::write(dir.join("a.txt"), "2").unwrap();

        let commit = create_checkpoint(dir, "refs/cadenza/checkpoints/T-1/cp")
            .await
            .unwrap();
        // Restore immediately — working tree already matches the snapshot.
        restore_checkpoint(dir, &commit, false).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "2");
    }

    #[tokio::test]
    async fn is_linked_worktree_distinguishes_main_and_worktree() {
        let (repo, _holder, wt) = repo_with_worktree().await;
        assert!(
            !is_linked_worktree(repo.path()).await.unwrap(),
            "main repo is not a linked worktree"
        );
        assert!(
            is_linked_worktree(&wt).await.unwrap(),
            "added worktree is a linked worktree"
        );
    }

    #[tokio::test]
    async fn restore_keeps_nested_repo_in_main_mode_and_reports_it() {
        let repo = init_repo();
        let dir = repo.path();
        commit_file(dir, "a.txt", "1", "add a");
        let commit = create_checkpoint(dir, "refs/cadenza/checkpoints/T-1/cp")
            .await
            .unwrap();

        // A nested git repo appears after the snapshot.
        std::fs::create_dir(dir.join("nested")).unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(dir.join("nested"))
            .args(["init"])
            .status()
            .unwrap();
        std::fs::write(dir.join("nested").join("f.txt"), "x").unwrap();

        // Main-repo mode (remove_nested=false): nested repo SURVIVES and is
        // reported as a leftover (partial rewind), never silently nuked.
        let leftovers = restore_checkpoint(dir, &commit, false).await.unwrap();
        assert!(dir.join("nested").exists(), "nested repo kept in main mode");
        assert!(
            leftovers.iter().any(|l| l.contains("nested")),
            "leftover reported, got {leftovers:?}"
        );

        // Worktree mode (remove_nested=true): nested repo IS removed, no leftover.
        let leftovers2 = restore_checkpoint(dir, &commit, true).await.unwrap();
        assert!(
            !dir.join("nested").exists(),
            "nested repo removed with -ff in worktree mode"
        );
        assert!(leftovers2.is_empty(), "no leftovers, got {leftovers2:?}");
    }

    #[tokio::test]
    async fn checkpoint_index_preserves_staged_blob() {
        let repo = init_repo();
        let dir = repo.path();
        commit_file(dir, "a.txt", "1", "add a");
        // Stage content distinct from HEAD, then snapshot the index.
        std::fs::write(dir.join("a.txt"), "staged").unwrap();
        StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(["add", "a.txt"])
            .status()
            .unwrap();
        let commit = checkpoint_index(dir, "refs/cadenza/checkpoints/T-1/idx")
            .await
            .unwrap();
        // The snapshot commit's tree carries the STAGED blob, so it stays
        // reachable even after a later restore clobbers the real index.
        let show = run_git(dir, &["show", &format!("{commit}:a.txt")])
            .await
            .unwrap();
        assert_eq!(show, "staged");
    }
}
