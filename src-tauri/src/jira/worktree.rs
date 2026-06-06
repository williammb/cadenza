//! Lazy shared per-issue worktree lifecycle + one-executor-per-issue guard
//! (Slice 4).
//!
//! A Jira issue owns ONE shared git worktree (branch + directory + base
//! sha), recorded on its [`JiraIssueRecord`]. The worktree is created
//! lazily the first time an accepted proposta carrying that issue's
//! identity materializes into a Task, and reused by every later subtask of
//! the same issue.
//!
//! Concurrency model (the point of this slice):
//! - A per-issue in-process guard (`AppState::jira_worktree_locks`, a
//!   registry of `tokio::sync::Mutex` keyed by `(site, issue)`) serializes
//!   creation so two concurrent ensure calls converge on one worktree.
//! - The persisted [`WorktreeState`] on the record is the durable
//!   reservation: `Reserved -> Creating -> Ready`, or `-> Failed` on error.
//!   Combined with double-checked locking, this makes
//!   [`ensure_issue_worktree`] idempotent and race-free.
//! - The store has no compare-and-swap; every transition is a
//!   read-modify-`upsert` performed ONLY while the per-issue guard is held,
//!   so the in-process mutex is what closes the TOCTOU window.
//!
//! This module does NOT spawn agents, fetch over HTTP, or build any UI —
//! those are other slices. It only reacts to the existing accept path and
//! exposes a pure busy-check helper for the executor guard.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use cadenza_proto::jira::WorktreeState;
use cadenza_proto::JiraIssueRecord;

use crate::commands::AppState;
use crate::worktrees::WorktreeInfo;

/// Per-issue creation-guard registry: `(jira_site, jira_issue_id)` →
/// the `tokio::sync::Mutex` serializing that issue's worktree creation.
pub type WorktreeLockRegistry = HashMap<(String, String), Arc<tokio::sync::Mutex<()>>>;

/// One-executor-per-issue registry slot. `Reserving` is an in-flight start
/// that passed the guard but has not yet created its session (the async
/// spawn window) — it counts as busy so a concurrent start is refused,
/// closing the check-then-act TOCTOU. `Live(session_id)` is a started
/// executor; it counts as busy only while its session is still in
/// `state.sessions` (a gone session is stale and reaped). A leaked
/// `Reserving` cannot wedge the issue: the start path holds an
/// `ExecutorReservation` RAII guard that removes it on any early
/// return/panic.
#[derive(Debug, Clone)]
pub enum ExecutorSlot {
    Reserving,
    Live(String),
}

/// Max length of a generated branch name. The `jira/<issue_id>-` prefix is
/// always preserved; only the slug is truncated to fit.
const MAX_BRANCH_LEN: usize = 60;

/// Result of a successful (or no-op) [`ensure_issue_worktree`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeReady {
    pub branch_name: String,
    pub worktree_path: String,
    pub base_sha: String,
}

/// Typed failure of [`ensure_issue_worktree`] / the executor guard. Carries
/// a stable machine code mapped to an `ErrorBody` where these reach the IPC
/// surface (future slices). For Slice 4's UI-only hooks, `Display` / a
/// `.map_err(to_str_err)` at the call site is sufficient.
#[derive(Debug, Clone)]
pub enum JiraWorktreeError {
    /// Another task for the same Jira issue already has a running agent.
    /// Maps to `ErrorBody.code = "jira_worktree_busy"` (retryable).
    Busy { session_id: String },
    /// Git/worktree creation failed. Maps to
    /// `ErrorBody.code = "jira_worktree_failed"` (retryable).
    CreateFailed(String),
    /// The issue record does not exist. Maps to the existing
    /// `"jira_not_found"` (exit 30).
    NotFound,
    /// Any other failure (config/lookup); maps to a generic error.
    Other(String),
}

impl std::fmt::Display for JiraWorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JiraWorktreeError::Busy { session_id } => write!(
                f,
                "jira_worktree_busy: another task for this Jira issue already has a running agent (session {session_id})"
            ),
            JiraWorktreeError::CreateFailed(d) => {
                write!(f, "jira_worktree_failed: {d}")
            }
            JiraWorktreeError::NotFound => write!(f, "jira_not_found: jira issue record not found"),
            JiraWorktreeError::Other(d) => write!(f, "{d}"),
        }
    }
}

impl std::error::Error for JiraWorktreeError {}

impl JiraWorktreeError {
    /// Stable machine code for the IPC/CLI surface (see
    /// `cadenza-cli/src/client.rs` exit-code table).
    pub fn code(&self) -> &'static str {
        match self {
            JiraWorktreeError::Busy { .. } => "jira_worktree_busy",
            JiraWorktreeError::CreateFailed(_) => "jira_worktree_failed",
            JiraWorktreeError::NotFound => "jira_not_found",
            JiraWorktreeError::Other(_) => "jira_worktree_failed",
        }
    }
}

/// Deterministic, sanitized, length-bounded branch name for a Jira issue's
/// shared worktree. ALWAYS embeds `jira_issue_id` so summary/key churn and
/// slug collisions can neither change nor collide the branch for a given
/// issue.
///
/// Rule: `jira/<issue_id>-<slug>`, where both the issue id and the slug are
/// lowercased, every char outside `[a-z0-9-]` is replaced by `-`, runs of
/// `-` are collapsed, and leading/trailing `-` trimmed. The whole string is
/// truncated to [`MAX_BRANCH_LEN`] by cutting the SLUG only — the
/// `jira/<issue_id>` prefix is never shortened, so the issue id always
/// survives truncation.
///
/// Chosen property for collision resistance: the branch is a pure function
/// of `(issue_id, summary)`. Same issue id + same slug => same branch (so a
/// second subtask of one issue reuses the branch); different issue ids =>
/// different branches even when their slugs are identical, because the id is
/// always present in the prefix.
pub fn jira_branch_name(jira_issue_id: &str, summary_or_key: &str) -> String {
    let id = sanitize_segment(jira_issue_id);
    let slug = sanitize_segment(summary_or_key);
    // The id is mandatory; fall back to a placeholder if it sanitized away
    // (e.g. caller passed an all-punctuation id) so the prefix is stable.
    let id = if id.is_empty() {
        "issue".to_string()
    } else {
        id
    };

    let prefix = format!("jira/{id}-");
    if slug.is_empty() {
        // No usable slug: trim the trailing '-' for a clean `jira/<id>`.
        return format!("jira/{id}");
    }
    // Reserve room for the prefix; truncate the slug only.
    let budget = MAX_BRANCH_LEN.saturating_sub(prefix.len());
    if budget == 0 {
        // Pathologically long id: keep the full prefix sans trailing '-'.
        return format!("jira/{id}");
    }
    let slug = truncate_on_char_boundary(&slug, budget);
    // Truncation can leave a trailing '-' if it cut mid-run.
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("jira/{id}")
    } else {
        format!("{prefix}{slug}")
    }
}

/// Lowercase, replace any char not in `[a-z0-9-]` with `-`, collapse
/// repeated `-`, and trim leading/trailing `-`.
fn sanitize_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for ch in s.chars() {
        let lc = ch.to_ascii_lowercase();
        let mapped = if lc.is_ascii_lowercase() || lc.is_ascii_digit() {
            lc
        } else {
            '-'
        };
        if mapped == '-' {
            if prev_dash {
                continue;
            }
            prev_dash = true;
        } else {
            prev_dash = false;
        }
        out.push(mapped);
    }
    out.trim_matches('-').to_string()
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 char.
/// (The sanitized slug is ASCII, so this is byte-exact in practice, but we
/// stay char-safe regardless.)
fn truncate_on_char_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Lazily ensure the shared worktree for `(jira_site, jira_issue_id)`
/// exists, associate `task_id` with it, and return its
/// [`WorktreeReady`]. Idempotent and race-free across concurrent callers
/// (see the module docs for the concurrency model).
pub async fn ensure_issue_worktree(
    state: &AppState,
    jira_site: &str,
    jira_issue_id: &str,
    project_id: &str,
    task_id: &str,
    summary_or_key: &str,
) -> Result<WorktreeReady, JiraWorktreeError> {
    // 1. Load the record (created by prior slices). Absent => not_found.
    let record = read_record(state, jira_site, jira_issue_id).await?;

    // 2. Fast path: already Ready with an on-disk worktree. Associate the
    //    task and return without taking the per-issue guard.
    if let Some(ready) = ready_if_valid(&record) {
        associate_task(state, task_id, &ready)?;
        return Ok(ready);
    }

    // 3. Acquire the per-issue creation guard. The registry map is a sync
    //    Mutex used only for the in-memory lookup; the per-key value is a
    //    `tokio::sync::Mutex` that IS held across the git `.await`.
    let guard_arc = {
        let mut registry = lock_registry(state);
        registry
            .entry((jira_site.to_string(), jira_issue_id.to_string()))
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _guard = guard_arc.lock().await;

    // 3b. Double-checked locking: re-read; a prior holder may have just
    //     finished creating the worktree while we waited for the guard.
    let record = read_record(state, jira_site, jira_issue_id).await?;
    if let Some(ready) = ready_if_valid(&record) {
        associate_task(state, task_id, &ready)?;
        return Ok(ready);
    }

    // 4. Recovery for a crash mid-create.
    if WorktreeState::parse(record.worktree_state.as_deref().unwrap_or(""))
        == Some(WorktreeState::Creating)
    {
        if let (Some(branch), Some(wt), Some(sha)) = (
            record.branch_name.as_deref(),
            record.worktree_path.as_deref(),
            record.base_sha.as_deref(),
        ) {
            if Path::new(wt).exists() {
                // Everything is present and the dir exists → promote.
                let ready = WorktreeReady {
                    branch_name: branch.to_string(),
                    worktree_path: wt.to_string(),
                    base_sha: sha.to_string(),
                };
                persist_state(state, &record, WorktreeState::Ready, None).await?;
                associate_task(state, task_id, &ready)?;
                return Ok(ready);
            }
        }
        // Partial create (path missing / fields unset) → recreate from
        // scratch below, cleaning any partial dir first.
    }

    // 5. Resolve repo path + default/origin branch from the project.
    let (repo, origin) = resolve_repo_and_origin(state, project_id).await?;

    // 6. Branch name: reuse the persisted one (later subtasks) else derive.
    let branch = match record.branch_name.as_deref() {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => jira_branch_name(jira_issue_id, summary_or_key),
    };
    let wt_path = crate::commands::suggested_worktree_path(&repo, &branch);
    let wt_str = wt_path.to_string_lossy().into_owned();

    // 7. Persist Reserved then Creating (with branch + path) BEFORE git, so
    //    a crash leaves a recoverable Creating marker. Both writes happen
    //    under the held guard.
    let mut rec = record.clone();
    rec.branch_name = Some(branch.clone());
    rec.worktree_path = Some(wt_str.clone());
    persist_state(state, &record, WorktreeState::Reserved, Some(&rec)).await?;
    persist_state(state, &rec, WorktreeState::Creating, None).await?;

    // 8. Best-effort clean a stale partial dir from a previous failed run.
    if wt_path.exists() {
        let _ = std::fs::remove_dir_all(&wt_path);
        let _ = crate::git::worktree_prune(&repo).await;
    }

    // 9. Create the worktree, then capture base sha.
    let create_branch = match crate::git::branch_exists(&repo, &branch).await {
        Ok(exists) => !exists,
        Err(e) => return Err(fail_and_cleanup(state, &rec, &repo, &wt_path, e.to_string()).await),
    };
    let start_point = if create_branch {
        Some(origin.as_str())
    } else {
        None
    };
    if let Err(e) =
        crate::git::add_worktree(&repo, &wt_path, &branch, create_branch, start_point).await
    {
        return Err(fail_and_cleanup(state, &rec, &repo, &wt_path, e.to_string()).await);
    }
    let base_sha = match crate::git::rev_parse(&wt_path, "HEAD").await {
        Ok(s) => s,
        Err(e) => {
            return Err(fail_and_cleanup(state, &rec, &repo, &wt_path, e.to_string()).await);
        }
    };

    // 10. Persist branch/path/base_sha + Ready.
    let mut done = rec.clone();
    done.branch_name = Some(branch.clone());
    done.worktree_path = Some(wt_str.clone());
    done.base_sha = Some(base_sha.clone());
    persist_state(state, &rec, WorktreeState::Ready, Some(&done)).await?;

    let ready = WorktreeReady {
        branch_name: branch.clone(),
        worktree_path: wt_str,
        base_sha,
    };

    // 11. Associate the task with the shared worktree.
    associate_task(state, task_id, &ready)?;
    Ok(ready)
}

/// Pure busy-check for the one-executor-per-issue guard. The issue is busy
/// iff a registry entry exists for `key` AND its recorded `session_id` is
/// still present in `sessions` (i.e. the executor is live). A registry
/// entry whose session has gone (killed/exited) is stale and treated as
/// not-busy — the caller is expected to clear it.
///
/// Generic over the session-map value so the guard logic is testable
/// without fabricating a live `TerminalSession`; the production caller
/// passes `&state.sessions`'s `HashMap<String, Arc<TerminalSession>>`.
pub fn issue_executor_busy<S>(
    active: &HashMap<(String, String), ExecutorSlot>,
    sessions: &HashMap<String, S>,
    key: &(String, String),
) -> bool {
    match active.get(key) {
        // An in-flight start owns the issue even before its session exists.
        Some(ExecutorSlot::Reserving) => true,
        Some(ExecutorSlot::Live(session_id)) => sessions.contains_key(session_id),
        None => false,
    }
}

// ─── internals ─────────────────────────────────────────────────────────

fn lock_registry(state: &AppState) -> std::sync::MutexGuard<'_, WorktreeLockRegistry> {
    // Poison-tolerant, mirroring `worktrees.rs` lock discipline.
    state
        .jira_worktree_locks
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

async fn read_record(
    state: &AppState,
    site: &str,
    issue: &str,
) -> Result<JiraIssueRecord, JiraWorktreeError> {
    state
        .repo
        .read_jira_issue(site, issue)
        .await
        .map_err(|e| JiraWorktreeError::Other(e.to_string()))?
        .ok_or(JiraWorktreeError::NotFound)
}

/// `Some(ready)` when the record is `Ready` with a path that exists on disk
/// and all three identity fields set; `None` otherwise.
pub(crate) fn ready_if_valid(record: &JiraIssueRecord) -> Option<WorktreeReady> {
    if WorktreeState::parse(record.worktree_state.as_deref()?) != Some(WorktreeState::Ready) {
        return None;
    }
    let branch = record.branch_name.clone()?;
    let wt = record.worktree_path.clone()?;
    let sha = record.base_sha.clone()?;
    if !Path::new(&wt).exists() {
        return None;
    }
    Some(WorktreeReady {
        branch_name: branch,
        worktree_path: wt,
        base_sha: sha,
    })
}

/// Resolve `Project.path` and the origin branch (`default_branch`, falling
/// back to the repo's current branch) from `project_id`.
async fn resolve_repo_and_origin(
    state: &AppState,
    project_id: &str,
) -> Result<(std::path::PathBuf, String), JiraWorktreeError> {
    let (repo, default_branch) = {
        let cfg = state
            .config
            .lock()
            .map_err(|e| JiraWorktreeError::Other(e.to_string()))?;
        let project = cfg
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .ok_or_else(|| {
                JiraWorktreeError::Other(format!("project '{project_id}' not found in config"))
            })?;
        (
            project.path.clone(),
            project
                .default_branch
                .clone()
                .filter(|b| !b.trim().is_empty()),
        )
    };
    let origin = match default_branch {
        Some(b) => b,
        None => crate::git::current_branch(&repo)
            .await
            .map_err(|e| JiraWorktreeError::CreateFailed(e.to_string()))?,
    };
    Ok((repo, origin))
}

/// Read-modify-write a state transition. When `full` is `Some`, persist
/// that record (with its branch/path/sha already set) stamped with `next`;
/// otherwise stamp `base` with `next`. Bumps `updated_at_ms`. Only ever
/// called while the per-issue guard is held.
async fn persist_state(
    state: &AppState,
    base: &JiraIssueRecord,
    next: WorktreeState,
    full: Option<&JiraIssueRecord>,
) -> Result<(), JiraWorktreeError> {
    let mut rec = full.cloned().unwrap_or_else(|| base.clone());
    rec.worktree_state = Some(next.as_str().to_string());
    rec.updated_at_ms = now_ms();
    state
        .repo
        .upsert_jira_issue(&rec)
        .await
        .map_err(|e| JiraWorktreeError::Other(e.to_string()))
}

/// Set `Failed`, best-effort remove the partial worktree dir + prune, and
/// return a `CreateFailed`.
async fn fail_and_cleanup(
    state: &AppState,
    rec: &JiraIssueRecord,
    repo: &Path,
    wt_path: &Path,
    detail: String,
) -> JiraWorktreeError {
    if let Err(e) = persist_state(state, rec, WorktreeState::Failed, None).await {
        tracing::warn!(error = %e, "persisting failed worktree state failed");
    }
    if wt_path.exists() {
        let _ = std::fs::remove_dir_all(wt_path);
    }
    let _ = crate::git::worktree_prune(repo).await;
    JiraWorktreeError::CreateFailed(detail)
}

fn associate_task(
    state: &AppState,
    task_id: &str,
    ready: &WorktreeReady,
) -> Result<(), JiraWorktreeError> {
    state
        .task_worktrees
        .set(
            task_id,
            WorktreeInfo {
                worktree_path: Some(ready.worktree_path.clone()),
                branch: Some(ready.branch_name.clone()),
                origin_branch: None,
                use_worktree: true,
            },
        )
        .map_err(|e| JiraWorktreeError::Other(e.to_string()))
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::store::FileRepository;
    use std::process::Command as StdCommand;
    use std::sync::Arc;
    use tempfile::TempDir;

    // ─── branch-name (pure) ────────────────────────────────────────────

    #[test]
    fn jira_branch_name_is_deterministic() {
        let a = jira_branch_name("10001", "Fix the login bug");
        let b = jira_branch_name("10001", "Fix the login bug");
        assert_eq!(a, b);
    }

    #[test]
    fn jira_branch_name_includes_issue_id() {
        assert!(jira_branch_name("10001", "").contains("10001"));
        assert!(jira_branch_name("10001", "!!!").contains("10001"));
        assert!(jira_branch_name("PROJ-42", "whatever").contains("proj-42"));
    }

    #[test]
    fn jira_branch_name_sanitizes_and_truncates() {
        let out = jira_branch_name("10001", "Add OAuth/SSO support — café ☕ login!!!");
        // Only [a-z0-9-/] (the single literal 'jira/' slash).
        let body = out.strip_prefix("jira/").unwrap();
        assert!(
            body.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "unexpected chars in {out}"
        );
        assert!(out.len() <= MAX_BRANCH_LEN, "too long: {out}");
        assert!(out.contains("10001"));
        // A very long summary still keeps the id after truncation.
        let long = jira_branch_name("10001", &"word ".repeat(50));
        assert!(long.len() <= MAX_BRANCH_LEN);
        assert!(long.contains("10001"));
        assert!(!long.ends_with('-'));
    }

    #[test]
    fn jira_branch_name_collision_resistant() {
        // Different issue ids with the SAME slug differ via the id.
        let a = jira_branch_name("10001", "same summary");
        let b = jira_branch_name("10002", "same summary");
        assert_ne!(a, b);
        // Same issue id intentionally yields the same branch (subtasks
        // reuse one shared worktree) — documented property.
        let c = jira_branch_name("10001", "same summary");
        assert_eq!(a, c);
    }

    // ─── ensure (integration over a temp repo + file store) ────────────

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

    /// (state home, repo dir, AppState) wired so `project_id = "p1"` points
    /// at `repo`. The state home is separate from the repo so sidecar files
    /// don't pollute the git tree.
    fn scaffold(repo: &Path) -> (TempDir, Arc<AppState>) {
        let home = TempDir::new().unwrap();
        let repo_record = FileRepository::new(home.path()).unwrap();
        let mut cfg = Config::default();
        cfg.projects.push(crate::config::Project {
            id: "p1".into(),
            name: "P1".into(),
            path: repo.to_path_buf(),
            agente: None,
            default_branch: Some("main".into()),
            color: None,
            quality: None,
        });
        let state = AppState::for_test(home.path(), Arc::new(repo_record), cfg).unwrap();
        (home, Arc::new(state))
    }

    async fn seed_record(state: &AppState, site: &str, issue: &str) {
        let now = now_ms();
        state
            .repo
            .upsert_jira_issue(&JiraIssueRecord {
                jira_site: site.into(),
                jira_issue_id: issue.into(),
                jira_key: format!("PROJ-{issue}"),
                project_id: Some("p1".into()),
                analysis_run_id: None,
                secret_hash: None,
                secret_expiry_ms: None,
                secret_status: None,
                raw_adf: None,
                branch_name: None,
                worktree_path: None,
                base_sha: None,
                worktree_state: None,
                created_at_ms: now,
                updated_at_ms: now,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn ensure_creates_branch_worktree_and_base_sha() {
        let repo = init_repo();
        let (_home, state) = scaffold(repo.path());
        let site = "https://x.atlassian.net";
        seed_record(&state, site, "10001").await;

        let ready = ensure_issue_worktree(&state, site, "10001", "p1", "T-1", "Login bug")
            .await
            .unwrap();

        assert!(crate::git::branch_exists(repo.path(), &ready.branch_name)
            .await
            .unwrap());
        assert!(Path::new(&ready.worktree_path).exists());
        let rec = state
            .repo
            .read_jira_issue(site, "10001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.worktree_state.as_deref(), Some("ready"));
        let head = crate::git::rev_parse(Path::new(&ready.worktree_path), "HEAD")
            .await
            .unwrap();
        assert_eq!(ready.base_sha, head);
        // Task is associated.
        let info = state.task_worktrees.get("T-1").unwrap();
        assert_eq!(
            info.worktree_path.as_deref(),
            Some(ready.worktree_path.as_str())
        );
        assert!(info.use_worktree);
    }

    #[tokio::test]
    async fn ensure_is_idempotent_second_call_is_noop() {
        let repo = init_repo();
        let (_home, state) = scaffold(repo.path());
        let site = "https://x.atlassian.net";
        seed_record(&state, site, "10001").await;

        let first = ensure_issue_worktree(&state, site, "10001", "p1", "T-1", "Login bug")
            .await
            .unwrap();
        let second = ensure_issue_worktree(&state, site, "10001", "p1", "T-2", "Login bug")
            .await
            .unwrap();
        assert_eq!(first, second);
        // Exactly one worktree on disk for this branch.
        let out = StdCommand::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let count = text.matches(&first.branch_name).count();
        assert_eq!(count, 1, "expected exactly one worktree:\n{text}");
        // Both tasks are associated to the same worktree.
        assert_eq!(
            state.task_worktrees.get("T-2").unwrap().worktree_path,
            Some(first.worktree_path.clone())
        );
    }

    #[tokio::test]
    async fn ensure_two_concurrent_calls_create_one_worktree() {
        let repo = init_repo();
        let (_home, state) = scaffold(repo.path());
        let site = "https://x.atlassian.net";
        seed_record(&state, site, "10001").await;

        let s1 = state.clone();
        let s2 = state.clone();
        let f1 = ensure_issue_worktree(&s1, site, "10001", "p1", "T-1", "Login bug");
        let f2 = ensure_issue_worktree(&s2, site, "10001", "p1", "T-2", "Login bug");
        let (r1, r2) = tokio::join!(f1, f2);
        let r1 = r1.unwrap();
        let r2 = r2.unwrap();
        assert_eq!(r1.worktree_path, r2.worktree_path);

        let out = StdCommand::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            text.matches(&r1.branch_name).count(),
            1,
            "exactly one worktree:\n{text}"
        );
        let rec = state
            .repo
            .read_jira_issue(site, "10001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.worktree_state.as_deref(), Some("ready"));
    }

    #[tokio::test]
    async fn ensure_recovers_creating_with_missing_path_retries() {
        let repo = init_repo();
        let (_home, state) = scaffold(repo.path());
        let site = "https://x.atlassian.net";
        seed_record(&state, site, "10001").await;
        // Pre-seed a Creating record whose worktree_path does not exist.
        let mut rec = state
            .repo
            .read_jira_issue(site, "10001")
            .await
            .unwrap()
            .unwrap();
        rec.worktree_state = Some("creating".into());
        rec.worktree_path = Some(
            repo.path()
                .join("does-not-exist")
                .to_string_lossy()
                .into_owned(),
        );
        rec.branch_name = Some("jira/10001-stale".into());
        state.repo.upsert_jira_issue(&rec).await.unwrap();

        let ready = ensure_issue_worktree(&state, site, "10001", "p1", "T-1", "Login bug")
            .await
            .unwrap();
        assert!(Path::new(&ready.worktree_path).exists());
        let rec = state
            .repo
            .read_jira_issue(site, "10001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.worktree_state.as_deref(), Some("ready"));
    }

    #[tokio::test]
    async fn ensure_recovers_creating_with_present_path_promotes() {
        let repo = init_repo();
        let (_home, state) = scaffold(repo.path());
        let site = "https://x.atlassian.net";
        seed_record(&state, site, "10001").await;

        // Manually create a real worktree, then pre-seed a Creating record
        // pointing at it with all identity fields set.
        let holder = TempDir::new().unwrap();
        let wt = holder.path().join("wt-promote");
        crate::git::add_worktree(repo.path(), &wt, "jira/10001-promote", true, Some("main"))
            .await
            .unwrap();
        let sha = crate::git::rev_parse(&wt, "HEAD").await.unwrap();
        let mut rec = state
            .repo
            .read_jira_issue(site, "10001")
            .await
            .unwrap()
            .unwrap();
        rec.worktree_state = Some("creating".into());
        rec.worktree_path = Some(wt.to_string_lossy().into_owned());
        rec.branch_name = Some("jira/10001-promote".into());
        rec.base_sha = Some(sha.clone());
        state.repo.upsert_jira_issue(&rec).await.unwrap();

        let ready = ensure_issue_worktree(&state, site, "10001", "p1", "T-1", "Login bug")
            .await
            .unwrap();
        assert_eq!(ready.worktree_path, wt.to_string_lossy());
        assert_eq!(ready.base_sha, sha);
        let rec = state
            .repo
            .read_jira_issue(site, "10001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.worktree_state.as_deref(), Some("ready"));
    }

    #[tokio::test]
    async fn ensure_failed_cleanup_sets_failed_state() {
        // A project path that is NOT a git repo → add_worktree fails.
        let not_repo = TempDir::new().unwrap();
        let (_home, state) = scaffold(not_repo.path());
        let site = "https://x.atlassian.net";
        seed_record(&state, site, "10001").await;

        let err = ensure_issue_worktree(&state, site, "10001", "p1", "T-1", "Login bug")
            .await
            .unwrap_err();
        assert!(
            matches!(err, JiraWorktreeError::CreateFailed(_)),
            "got {err:?}"
        );
        let rec = state
            .repo
            .read_jira_issue(site, "10001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.worktree_state.as_deref(), Some("failed"));
        // No partial worktree dir remains.
        if let Some(p) = rec.worktree_path.as_deref() {
            assert!(!Path::new(p).exists(), "partial dir not cleaned: {p}");
        }
    }

    #[tokio::test]
    async fn ensure_not_found_when_record_absent() {
        let repo = init_repo();
        let (_home, state) = scaffold(repo.path());
        let err = ensure_issue_worktree(&state, "https://x.atlassian.net", "404", "p1", "T-1", "x")
            .await
            .unwrap_err();
        assert!(matches!(err, JiraWorktreeError::NotFound));
    }

    // ─── executor busy helper (pure) ───────────────────────────────────

    // The busy helper is generic over the session-map value, so these
    // pure tests use `()` placeholders to model session presence/absence
    // without spawning a real PTY-backed `TerminalSession`. The production
    // call site passes the real `HashMap<String, Arc<TerminalSession>>`.

    #[test]
    fn executor_guard_refuses_second_concurrent_start() {
        let key = ("site".to_string(), "10001".to_string());
        let mut active = HashMap::new();
        active.insert(key.clone(), ExecutorSlot::Live("S-1".to_string()));
        let mut sessions: HashMap<String, ()> = HashMap::new();
        sessions.insert("S-1".to_string(), ());
        assert!(issue_executor_busy(&active, &sessions, &key));
    }

    #[test]
    fn executor_guard_reserving_is_busy() {
        // An in-flight start (passed the guard, session not yet created)
        // must block a concurrent start even though `sessions` is empty —
        // this is what closes the check-then-act TOCTOU.
        let key = ("site".to_string(), "10001".to_string());
        let mut active = HashMap::new();
        active.insert(key.clone(), ExecutorSlot::Reserving);
        let sessions: HashMap<String, ()> = HashMap::new();
        assert!(issue_executor_busy(&active, &sessions, &key));
    }

    #[test]
    fn executor_guard_allows_after_first_ends() {
        let key = ("site".to_string(), "10001".to_string());
        let mut active = HashMap::new();
        active.insert(key.clone(), ExecutorSlot::Live("S-1".to_string()));
        // Session gone (killed/exited) → stale entry, not busy.
        let sessions: HashMap<String, ()> = HashMap::new();
        assert!(!issue_executor_busy(&active, &sessions, &key));
    }

    #[test]
    fn executor_guard_ignores_non_jira_tasks() {
        // No entry for the key at all (caller skips the guard for None
        // issues, but the helper itself returns false on a missing key).
        let key = ("site".to_string(), "10001".to_string());
        let active: HashMap<(String, String), ExecutorSlot> = HashMap::new();
        let sessions: HashMap<String, ()> = HashMap::new();
        assert!(!issue_executor_busy(&active, &sessions, &key));
    }
}
