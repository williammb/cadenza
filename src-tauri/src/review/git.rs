//! Hardened, read-only git runner for the review engine (PLAN §C.11).
//!
//! This is deliberately **distinct** from the interactive `crate::git`
//! module, which has no caps, no timeout, and inherits the caller's
//! environment. Every invocation here:
//!
//! - uses a fixed argv (no shell, no interpolation),
//! - clears the inherited environment and re-adds only a minimal,
//!   deterministic set (`PATH`, `GIT_OPTIONAL_LOCKS=0`,
//!   `GIT_TERMINAL_PROMPT=0`, `GIT_CONFIG_NOSYSTEM=1`, `LC_ALL=C`),
//! - injects `--no-pager` globally and `--no-ext-diff`/`--no-textconv`
//!   per diff-style subcommand so external filters and pagers can never
//!   run or block,
//! - enforces a per-command wall-clock timeout (kills the child on
//!   expiry) and an output byte cap (stops reading and marks the result
//!   truncated rather than buffering unbounded git output),
//! - **never panics** and never returns the process's raw error to a
//!   caller as a failure they must handle — every failure mode is folded
//!   into a structured [`GitError`] the engine records as a
//!   `CollectionError`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Default per-command wall-clock timeout. Read-only plumbing commands in
/// a local worktree finish in milliseconds; 10 s is a generous ceiling
/// that still bounds a hung filter or a pathological repo.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default stdout byte cap per command. Diffs are collected per-file with
/// their own caps in `patch.rs`; this protects the name-status/porcelain
/// metadata reads from an adversarially huge tree. 8 MiB.
const DEFAULT_BYTE_CAP: usize = 8 * 1024 * 1024;

/// Structured, non-panicking failure of a single hardened git call.
/// Carries a stable machine `code` (folded into `CollectionError.code`)
/// and an English `detail` for logs.
#[derive(Debug, Clone)]
pub(crate) struct GitError {
    /// Stable code: `git_spawn`, `git_timeout`, `git_nonzero`, `git_io`.
    pub code: &'static str,
    /// English detail (subcommand + reason); never includes secrets.
    pub detail: String,
}

impl GitError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// Outcome of a hardened git call: the captured (possibly capped) stdout
/// bytes and whether the byte cap clipped them.
#[derive(Debug, Clone)]
pub(crate) struct GitOutput {
    pub stdout: Vec<u8>,
    pub truncated: bool,
}

/// A hardened git invocation context bound to one worktree directory.
pub(crate) struct GitCtx {
    dir: PathBuf,
    timeout: Duration,
    byte_cap: usize,
}

impl GitCtx {
    /// Build a context for `dir` with default timeout (10 s) and byte cap.
    pub(crate) fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            timeout: DEFAULT_TIMEOUT,
            byte_cap: DEFAULT_BYTE_CAP,
        }
    }

    /// Run `git -C <dir> --no-pager <args>` with the hardened environment.
    ///
    /// Returns captured stdout (capped at `byte_cap`) on a zero exit, or a
    /// structured [`GitError`] for any failure (spawn, timeout, non-zero
    /// exit, or I/O). The caller folds errors into `collection_errors`;
    /// this never panics.
    ///
    /// `--no-pager` is injected before the subcommand on every call so a
    /// configured pager cannot block on a non-tty. Callers that run a
    /// diff-style subcommand additionally pass `--no-ext-diff`/`--no-textconv`
    /// (see [`Self::run_diff`]).
    pub(crate) async fn run(&self, args: &[&str]) -> Result<GitOutput, GitError> {
        // The I/O core runs on a spawned task so the (large, debug-build)
        // process+read+wait future never accumulates on the caller's stack.
        // `run` is called in tight loops from `resolve_base`/`collect`; if
        // each call's full future were embedded inline the composite future
        // overflows small Windows test-thread stacks. The owned args/dir
        // make the task `'static`.
        let dir = self.dir.clone();
        let timeout = self.timeout;
        let byte_cap = self.byte_cap;
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let label = owned.join(" ");

        let handle = tokio::spawn(async move {
            let mut cmd = Command::new("git");
            cmd.arg("-C").arg(&dir);
            cmd.arg("--no-pager");
            cmd.args(&owned);
            harden_env(&mut cmd);
            cmd.stdin(Stdio::null());
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            #[cfg(windows)]
            {
                // CREATE_NO_WINDOW: never flash a console for the child git.
                // `tokio::process::Command` re-exposes `creation_flags`.
                cmd.creation_flags(0x0800_0000);
            }

            let mut child = cmd
                .spawn()
                .map_err(|e| GitError::new("git_spawn", format!("spawn git {label}: {e}")))?;

            let mut stdout = child.stdout.take().ok_or_else(|| {
                GitError::new("git_io", format!("no stdout pipe for git {label}"))
            })?;

            let read_fut = async {
                let mut buf = Vec::with_capacity(8 * 1024);
                let mut chunk = vec![0u8; 64 * 1024];
                let mut truncated = false;
                loop {
                    let n = stdout
                        .read(&mut chunk)
                        .await
                        .map_err(|e| GitError::new("git_io", format!("read git {label}: {e}")))?;
                    if n == 0 {
                        break;
                    }
                    if buf.len() < byte_cap {
                        let room = byte_cap - buf.len();
                        let take = room.min(n);
                        buf.extend_from_slice(&chunk[..take]);
                        if take < n {
                            truncated = true;
                        }
                    } else {
                        truncated = true;
                    }
                }
                Ok::<(Vec<u8>, bool), GitError>((buf, truncated))
            };

            let (read_res, status_res) = tokio::join!(read_fut, child.wait());
            let (stdout_bytes, truncated) = read_res?;
            let status = status_res
                .map_err(|e| GitError::new("git_io", format!("wait git {label}: {e}")))?;
            Ok::<(Vec<u8>, bool, std::process::ExitStatus), GitError>((
                stdout_bytes,
                truncated,
                status,
            ))
        });

        let joined = match tokio::time::timeout(timeout, handle).await {
            Err(_) => {
                return Err(GitError::new(
                    "git_timeout",
                    format!("git {} timed out after {:?}", args.join(" "), self.timeout),
                ));
            }
            Ok(Err(join_err)) => {
                return Err(GitError::new(
                    "git_io",
                    format!("git {} task failed: {join_err}", args.join(" ")),
                ));
            }
            Ok(Ok(inner)) => inner,
        };

        match joined {
            Err(e) => Err(e),
            Ok((stdout_bytes, truncated, status)) => {
                if status.success() {
                    Ok(GitOutput {
                        stdout: stdout_bytes,
                        truncated,
                    })
                } else {
                    Err(GitError::new(
                        "git_nonzero",
                        format!(
                            "git {} exited with {}",
                            args.join(" "),
                            status
                                .code()
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "signal".into())
                        ),
                    ))
                }
            }
        }
    }

    /// Run a diff-style subcommand with `--no-ext-diff --no-textconv`
    /// injected right after the subcommand name. `args[0]` must be the
    /// subcommand (e.g. `"diff"`); the hardening flags are inserted after
    /// it so they apply to the diff machinery.
    pub(crate) async fn run_diff(&self, args: &[&str]) -> Result<GitOutput, GitError> {
        let mut full: Vec<&str> = Vec::with_capacity(args.len() + 2);
        if let Some((first, rest)) = args.split_first() {
            full.push(first);
            full.push("--no-ext-diff");
            full.push("--no-textconv");
            full.extend_from_slice(rest);
        }
        self.run(&full).await
    }

    /// Run a command and decode trimmed UTF-8 stdout, mapping any failure
    /// to `None`. Convenience for single-line plumbing reads (rev-parse,
    /// symbolic-ref) where a missing value and an error are both "absent".
    pub(crate) async fn line(&self, args: &[&str]) -> Option<String> {
        let out = self.run(args).await.ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// Apply the sanitized environment to `cmd`: clear everything inherited,
/// then re-add a minimal deterministic set. `PATH` is preserved so git
/// (and its required helpers) remain locatable. A free function so the
/// spawned I/O task can call it without borrowing `&self`.
fn harden_env(cmd: &mut Command) {
    cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    // Windows git can need these to resolve the install / home.
    #[cfg(windows)]
    {
        for key in [
            "SystemRoot",
            "SystemDrive",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
        ] {
            if let Some(v) = std::env::var_os(key) {
                cmd.env(key, v);
            }
        }
    }
    cmd.env("GIT_OPTIONAL_LOCKS", "0"); // never take the index lock for reads
    cmd.env("GIT_TERMINAL_PROMPT", "0"); // never prompt for credentials
    cmd.env("GIT_CONFIG_NOSYSTEM", "1"); // ignore /etc/gitconfig
    cmd.env("GIT_PAGER", "cat"); // belt-and-suspenders with --no-pager
    cmd.env("LC_ALL", "C"); // stable, parseable English output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

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
        run(&["branch", "-M", "main"]);
        dir
    }

    #[tokio::test]
    async fn run_returns_stdout_on_success() {
        let repo = init_repo();
        let ctx = GitCtx::new(repo.path());
        let head = ctx.line(&["rev-parse", "HEAD"]).await;
        assert!(head.is_some());
        assert_eq!(head.unwrap().len(), 40);
    }

    #[tokio::test]
    async fn run_maps_nonzero_to_structured_error() {
        let repo = init_repo();
        let ctx = GitCtx::new(repo.path());
        let err = ctx.run(&["rev-parse", "does-not-exist"]).await.unwrap_err();
        assert_eq!(err.code, "git_nonzero");
    }

    #[tokio::test]
    async fn byte_cap_marks_truncated() {
        let repo = init_repo();
        // Tiny cap forces truncation on any non-empty output.
        let ctx = GitCtx {
            dir: repo.path().to_path_buf(),
            timeout: DEFAULT_TIMEOUT,
            byte_cap: 4,
        };
        let out = ctx.run(&["rev-parse", "HEAD"]).await.unwrap();
        assert!(out.truncated);
        assert_eq!(out.stdout.len(), 4);
    }
}
