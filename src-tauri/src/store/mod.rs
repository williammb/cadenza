//! Storage layer for tasks and triage.
//!
//! Phase A introduces a `Repository` trait so the backend can be swapped
//! between filesystem (default), SQLite, and PostgreSQL without touching
//! the call sites in `commands.rs` / `ipc.rs`.
//!
//! Implementations:
//!   - `FileRepository` (this module, `files.rs`) — wraps the original
//!     sync `Store` + `Triage` engines kept in `files_inner.rs` and
//!     `triage_inner.rs`. The on-disk format is the one frozen by
//!     CLAUDE.md for compatibility with the Node.js `task-ai` legacy.
//!   - `SqliteRepository` (Phase B, separate file)
//!   - `PgRepository` (Phase C, separate file)

use async_trait::async_trait;
use std::time::Duration;
use thiserror::Error;

mod files;
mod files_inner;
mod ideias_inner;
mod jira_inner;
mod jira_review_inner;
mod memory_inner;
pub mod migrate;
mod postgres;
mod review_inner;
mod sqlite;
mod triage_inner;

// Review-package types live in the app crate (not `cadenza_proto`): the
// CLI never reads a `ReviewPackage`. Re-export from `crate::review` so
// the backends + call sites can name them through `store::`.
pub use crate::review::{PackageStatus, ReviewPackage};
// Aggregate (issue-owned) review packages (Slice 5). Parallel to
// `ReviewPackage`; keyed by `(jira_site, jira_issue_id, attempt)`. The status
// enum is named through `crate::review::issue` where needed.
pub use crate::review::issue::IssueReviewPackage;

pub use files::FileRepository;
pub use postgres::{PgConnectionParams, PgRepository, PgSslModeChoice};
pub use sqlite::SqliteRepository;

// Re-exports so callers don't have to know which crate hosts the types.
#[allow(unused_imports)]
pub use cadenza_proto::{
    Decisao, DecisaoRegistro, Estado, Ideia, IdeiaStatus, JiraIssueRecord, MemoryItem,
    MemorySuggestion, NewProposta, ProjectMemory, Proposta, SuggestionKind, Task,
};

/// Unified error covering tasks + triage + transport. Each backend
/// translates its driver-specific errors into one of these variants so
/// the call sites can pattern-match without caring about the backend.
#[derive(Error, Debug)]
pub enum StoreError {
    #[error("task not found: {0}")]
    NotFound(String),
    #[error("task already exists: {0}")]
    AlreadyExists(String),
    #[error("busy: failed to acquire lock within 3s")]
    Busy,
    #[error("bad data: {0}")]
    BadData(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("other: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Reject ids that could escape the store root via path traversal.
/// Ids must be a single normal path component (no separators, no `..`,
/// no NUL bytes), non-empty, ≤256 bytes. Called at the IPC wire
/// boundary before any id reaches `path_for` on the file backend; DB
/// backends are immune via parameterized queries but get the same
/// hygiene so an id like `"../auth"` never lands in a primary key.
pub fn validate_id(id: &str) -> Result<()> {
    use std::path::{Component, Path};
    if id.is_empty() {
        return Err(StoreError::BadData("id must not be empty".into()));
    }
    if id.len() > 256 {
        return Err(StoreError::BadData(format!(
            "id too long: {} bytes",
            id.len()
        )));
    }
    if id.contains('\0') {
        return Err(StoreError::BadData("id contains NUL byte".into()));
    }
    // `\` is a path separator on Windows only; reject it unconditionally so
    // validation behaves identically on Linux/macOS (where Path::components
    // would otherwise treat `foo\bar` as a single Normal component).
    if id.contains('\\') {
        return Err(StoreError::BadData(format!("invalid id: {id}")));
    }
    let mut comps = Path::new(id).components();
    let first = comps
        .next()
        .ok_or_else(|| StoreError::BadData("empty id".into()))?;
    if !matches!(first, Component::Normal(_)) || comps.next().is_some() {
        return Err(StoreError::BadData(format!("invalid id: {id}")));
    }
    Ok(())
}

/// Reject idempotency keys that are unsafe to embed in a path component or
/// that exceed the wire contract (PLAN §B.6, §C.9). Even though the raw key
/// is never used as a filename on the file backend (it is hashed first), it
/// is validated up front on every backend for parity — exactly like
/// [`validate_id`].
///
/// Rules: non-empty, ≤ 128 bytes, charset restricted to `[A-Za-z0-9._-]`
/// (no path separators, control chars, NUL, or `..`), and Windows-reserved
/// device stems (`CON`, `PRN`, `AUX`, `NUL`, `COM1..9`, `LPT1..9`) rejected
/// case-insensitively. Violations map to [`StoreError::BadData`], which the
/// wire handler surfaces as CLI exit 2 (`bad_args`).
pub fn validate_idempotency_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(StoreError::BadData(
            "idempotency_key must not be empty".into(),
        ));
    }
    if key.len() > 128 {
        return Err(StoreError::BadData(format!(
            "idempotency_key too long: {} bytes",
            key.len()
        )));
    }
    if !key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(StoreError::BadData(format!(
            "idempotency_key has invalid characters: {key}"
        )));
    }
    if key == ".." || key == "." {
        return Err(StoreError::BadData(format!(
            "idempotency_key has invalid value: {key}"
        )));
    }
    let upper = key.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.len() == 4
            && matches!(upper.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        return Err(StoreError::BadData(format!(
            "idempotency_key is a reserved device name: {key}"
        )));
    }
    Ok(())
}

/// Backend-agnostic data layer. `Send + Sync` so it can sit inside
/// `Arc<dyn Repository>` in the Tauri state.
#[async_trait]
pub trait Repository: Send + Sync {
    // ─── tasks ─────────────────────────────────────────────────────
    async fn list_tasks(&self, filter: Option<Estado>) -> Result<Vec<Task>>;
    async fn read_task(&self, id: &str) -> Result<Task>;
    async fn create_task(&self, task: &Task) -> Result<()>;
    async fn set_estado(&self, id: &str, estado: Estado) -> Result<()>;
    async fn set_titulo(&self, id: &str, titulo: &str) -> Result<()>;
    async fn update_task_body(&self, id: &str, body: &str) -> Result<()>;
    async fn delete_task(&self, id: &str) -> Result<()>;
    async fn append_log(&self, id: &str, text: &str) -> Result<()>;

    /// Convenience: first task in `fazendo`, or `None`.
    async fn current_task(&self) -> Result<Option<Task>> {
        let tasks = self.list_tasks(Some(Estado::Fazendo)).await?;
        Ok(tasks.into_iter().next())
    }

    // ─── triage ────────────────────────────────────────────────────
    async fn propose(&self, args: NewProposta) -> Result<Proposta>;
    async fn read_proposta(&self, proposta_id: &str) -> Result<Option<Proposta>>;
    async fn read_decisao(&self, proposta_id: &str) -> Result<Option<DecisaoRegistro>>;
    async fn list_pending_propostas(&self) -> Result<Vec<Proposta>>;
    async fn write_decisao(&self, registro: DecisaoRegistro) -> Result<()>;
    async fn await_decisao(
        &self,
        proposta_id: &str,
        timeout: Duration,
    ) -> Result<Option<DecisaoRegistro>>;

    // ─── ideias (Inbox) ────────────────────────────────────────────
    async fn list_ideias(&self) -> Result<Vec<Ideia>>;
    async fn read_ideia(&self, id: &str) -> Result<Option<Ideia>>;
    async fn create_ideia(&self, ideia: &Ideia) -> Result<()>;
    async fn delete_ideia(&self, id: &str) -> Result<()>;
    async fn set_ideia_status(&self, id: &str, status: IdeiaStatus) -> Result<()>;

    // ─── jira issues (cache de identidade) ─────────────────────────
    // Slice 1 wires only `list_jira_issues` (startup index seeding); the
    // upsert/read/delete surface is exercised by later slices (HTTP import,
    // worktree lifecycle) and by the backend tests. Allow until wired.
    /// Insert or replace a Jira issue record, keyed by (jira_site, jira_issue_id).
    #[allow(dead_code)]
    async fn upsert_jira_issue(&self, record: &JiraIssueRecord) -> Result<()>;

    /// Read one record by composite key; Ok(None) if absent.
    #[allow(dead_code)]
    async fn read_jira_issue(
        &self,
        jira_site: &str,
        jira_issue_id: &str,
    ) -> Result<Option<JiraIssueRecord>>;

    /// All records, ordered by (jira_site, jira_issue_id).
    async fn list_jira_issues(&self) -> Result<Vec<JiraIssueRecord>>;

    /// Delete by composite key; NotFound if absent.
    #[allow(dead_code)]
    async fn delete_jira_issue(&self, jira_site: &str, jira_issue_id: &str) -> Result<()>;

    // ─── memória compartilhada por projeto (T-34) ──────────────────
    /// Itens da memória oficial de um projeto. Vazio quando o projeto
    /// nunca teve memória.
    async fn list_memory(&self, project_id: &str) -> Result<Vec<MemoryItem>>;
    async fn add_memory_item(&self, project_id: &str, item: &MemoryItem) -> Result<()>;
    async fn update_memory_item(&self, project_id: &str, item_id: &str, texto: &str) -> Result<()>;
    async fn delete_memory_item(&self, project_id: &str, item_id: &str) -> Result<()>;

    /// Sugestões pendentes (aprendizados + ops de reeval) de um projeto.
    async fn list_memory_suggestions(&self, project_id: &str) -> Result<Vec<MemorySuggestion>>;
    async fn read_memory_suggestion(&self, id: &str) -> Result<Option<MemorySuggestion>>;
    async fn create_memory_suggestion(&self, suggestion: &MemorySuggestion) -> Result<()>;
    async fn delete_memory_suggestion(&self, id: &str) -> Result<()>;

    /// Migration helpers: dump tudo entre projetos para copiar entre
    /// backends. `(project_id, item)` pares e sugestões (que já carregam
    /// `project_id`).
    async fn all_memory_items(&self) -> Result<Vec<(String, MemoryItem)>>;
    async fn all_memory_suggestions(&self) -> Result<Vec<MemorySuggestion>>;

    // ─── review packages (PLAN §C.9, §F.17, §F.18) ─────────────────
    /// Every package for a task, ordered by `attempt` ascending. Empty
    /// when the task never had a `done` attempt; the latest is the last
    /// element.
    async fn list_review_packages(&self, task_id: &str) -> Result<Vec<ReviewPackage>>;

    /// Latest (highest-`attempt`) package for a task, or `None`. Default
    /// impl in terms of [`list_review_packages`]; the SQL backends
    /// override it with an indexed `ORDER BY attempt DESC LIMIT 1`.
    ///
    /// Consumed by the `done` / `review_decision` orchestration and the
    /// `get_review_package` command (separate workflows); allow until wired.
    #[allow(dead_code)]
    async fn latest_review_package(&self, task_id: &str) -> Result<Option<ReviewPackage>> {
        Ok(self.list_review_packages(task_id).await?.pop())
    }

    /// Idempotent upsert keyed on `(task_id, idempotency_key)`:
    /// - If a package with this key already exists, the stored package is
    ///   returned UNCHANGED (no-op) — the resumability contract (PLAN §C.9).
    /// - Otherwise `attempt = max(existing attempts) + 1` is allocated by
    ///   the backend (the `attempt` field of `pkg` is ignored), all prior
    ///   attempts of the task are marked `Superseded`, and the new package
    ///   is inserted. Attempt allocation + supersede + insert is ONE atomic
    ///   unit per backend (single tx for SQL; journal for files).
    async fn upsert_review_package(&self, pkg: &ReviewPackage) -> Result<ReviewPackage>;

    /// Mark every attempt of a task except `except_attempt` as
    /// [`PackageStatus::Superseded`]. Idempotent and best-effort: attempts
    /// already decided are left untouched only by the upsert path; this
    /// bulk call always wins, so callers pass the just-inserted attempt to
    /// preserve it. Used by the `done` orchestration after an upsert
    /// (separate workflow); allow until wired.
    #[allow(dead_code)]
    async fn mark_packages_superseded(&self, task_id: &str, except_attempt: u32) -> Result<()>;

    /// Record a reviewer decision (`status`) on one `(task_id, attempt)`
    /// package (PLAN §F.18). [`StoreError::NotFound`] when absent.
    async fn set_package_decision(
        &self,
        task_id: &str,
        attempt: u32,
        status: PackageStatus,
    ) -> Result<()>;

    /// Drop every package for a task — the `delete_task` cascade
    /// (PLAN §F.17). Idempotent: a task with zero packages is `Ok(())`.
    /// On the file backend this also removes any dangling `done-*.journal`
    /// belonging to the task.
    async fn delete_review_packages(&self, task_id: &str) -> Result<()>;

    /// Migration dump: every package across all tasks, ordered by
    /// `(task_id, attempt)` so the destination re-derives the same attempt
    /// sequence (PLAN §F.17 `copy_all`).
    async fn all_review_packages(&self) -> Result<Vec<ReviewPackage>>;

    /// Atomic `done` (PLAN §C.9): fold the package upsert (attempt
    /// allocation + supersede priors), the `[done request]` log append, and
    /// the estado flip into ONE crash-safe unit. The bare
    /// [`upsert_review_package`](Repository::upsert_review_package) is
    /// sidecar-only and does NOT touch the task `.md`/row; this method is the
    /// one the wire `done` handler calls so the three writes never split.
    ///
    /// - File backend: the write-ahead journal (`prepare_done` →
    ///   `commit_done`) — a crash mid-`done` is replayed at startup.
    /// - SQL backends: a single transaction.
    ///
    /// `log_line`, when `Some`, is appended verbatim to the task body (with
    /// dedup against an identical trailing line so a journal/transaction
    /// replay can't double it). `target_estado`, when `Some`, sets the task
    /// estado. Re-running the same `(task_id, idempotency_key)` is a no-op
    /// that returns the stored package without re-appending/re-flipping.
    async fn done_with_review_package(
        &self,
        pkg: &ReviewPackage,
        log_line: Option<&str>,
        target_estado: Option<Estado>,
    ) -> Result<ReviewPackage>;

    // ─── aggregate (issue-owned) review packages (Slice 5) ─────────
    // These are PARALLEL to the per-task review methods above and keyed by
    // `(jira_site, jira_issue_id, attempt)`. STATE-NEUTRAL: persisting an
    // aggregate touches ONLY its own row/sidecar — never a task `.md`/row,
    // never any estado, never a `done` path.

    /// Persist an aggregate (issue-owned) review package. Idempotent on
    /// `(jira_site, jira_issue_id, idempotency_key)`: a repeat key returns the
    /// stored package unchanged. Otherwise allocates `attempt = max + 1`,
    /// supersedes prior `Pending` aggregates for the same issue, and inserts
    /// (the package's own carried `status` is written, defaulting to
    /// `Pending`). Allocation + supersede + insert is ONE atomic unit per
    /// backend. Never touches any task row or estado.
    async fn upsert_issue_review_package(
        &self,
        pkg: &IssueReviewPackage,
    ) -> Result<IssueReviewPackage>;

    /// Latest (highest-`attempt`) aggregate review for an issue, or `None`.
    /// Default impl in terms of [`list_issue_review_packages`]; SQL backends
    /// override with an indexed `ORDER BY attempt DESC LIMIT 1`. Consumed by
    /// the `jira_review` command (a separate workflow); allow until wired.
    #[allow(dead_code)]
    async fn latest_issue_review_package(
        &self,
        jira_site: &str,
        jira_issue_id: &str,
    ) -> Result<Option<IssueReviewPackage>> {
        Ok(self
            .list_issue_review_packages(jira_site, jira_issue_id)
            .await?
            .pop())
    }

    /// All attempts for an issue, ordered by `attempt` ascending.
    async fn list_issue_review_packages(
        &self,
        jira_site: &str,
        jira_issue_id: &str,
    ) -> Result<Vec<IssueReviewPackage>>;

    /// Migration dump, ordered `(jira_site, jira_issue_id, attempt)`.
    async fn all_issue_review_packages(&self) -> Result<Vec<IssueReviewPackage>>;
}

// ─── error conversions from the legacy sync engines ────────────────

impl From<files_inner::StoreError> for StoreError {
    fn from(e: files_inner::StoreError) -> Self {
        use files_inner::StoreError as Inner;
        match e {
            Inner::NotFound(id) => StoreError::NotFound(id),
            Inner::AlreadyExists(id) => StoreError::AlreadyExists(id),
            Inner::Busy => StoreError::Busy,
            Inner::BadFrontmatter(s) => StoreError::BadData(s),
            Inner::Io(e) => StoreError::Io(e),
            Inner::Yaml(e) => StoreError::BadData(e.to_string()),
        }
    }
}

impl From<triage_inner::TriageError> for StoreError {
    fn from(e: triage_inner::TriageError) -> Self {
        use triage_inner::TriageError as Inner;
        match e {
            Inner::Io(e) => StoreError::Io(e),
            Inner::Json(e) => StoreError::BadData(e.to_string()),
            Inner::Other(e) => StoreError::Other(e.to_string()),
        }
    }
}

impl From<ideias_inner::IdeiaError> for StoreError {
    fn from(e: ideias_inner::IdeiaError) -> Self {
        use ideias_inner::IdeiaError as Inner;
        match e {
            Inner::NotFound(id) => StoreError::NotFound(id),
            Inner::AlreadyExists(id) => StoreError::AlreadyExists(id),
            Inner::Io(e) => StoreError::Io(e),
            Inner::Json(e) => StoreError::BadData(e.to_string()),
        }
    }
}

impl From<review_inner::ReviewError> for StoreError {
    fn from(e: review_inner::ReviewError) -> Self {
        use review_inner::ReviewError as Inner;
        match e {
            Inner::NotFound(id) => StoreError::NotFound(id),
            Inner::BadData(s) => StoreError::BadData(s),
            Inner::Io(e) => StoreError::Io(e),
            Inner::Json(e) => StoreError::BadData(e.to_string()),
            Inner::Other(e) => StoreError::Other(e.to_string()),
        }
    }
}

impl From<memory_inner::MemoryError> for StoreError {
    fn from(e: memory_inner::MemoryError) -> Self {
        use memory_inner::MemoryError as Inner;
        match e {
            Inner::ItemNotFound(id) | Inner::SuggestionNotFound(id) => StoreError::NotFound(id),
            Inner::SuggestionExists(id) | Inner::ItemExists(id) => StoreError::AlreadyExists(id),
            Inner::Io(e) => StoreError::Io(e),
            Inner::Json(e) => StoreError::BadData(e.to_string()),
        }
    }
}

#[cfg(test)]
mod id_validation_tests {
    use super::{validate_id, StoreError};

    #[test]
    fn accepts_normal_ids() {
        assert!(validate_id("T-1").is_ok());
        assert!(validate_id("I-abc123").is_ok());
        assert!(validate_id("P-aabbccdd").is_ok());
    }

    #[test]
    fn rejects_path_traversal() {
        for bad in [
            "..",
            "../auth",
            "../../etc/passwd",
            "foo/bar",
            "foo\\bar",
            ".",
            "",
        ] {
            assert!(
                matches!(validate_id(bad), Err(StoreError::BadData(_))),
                "expected BadData for {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_absolute_paths() {
        assert!(matches!(validate_id("/abs"), Err(StoreError::BadData(_))));
        if cfg!(windows) {
            assert!(matches!(
                validate_id("C:\\abs"),
                Err(StoreError::BadData(_))
            ));
        }
    }

    #[test]
    fn rejects_nul_byte() {
        assert!(matches!(
            validate_id("foo\0bar"),
            Err(StoreError::BadData(_))
        ));
    }
}
