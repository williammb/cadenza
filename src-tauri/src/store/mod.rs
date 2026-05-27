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
pub mod migrate;
mod postgres;
mod sqlite;
mod triage_inner;

pub use files::FileRepository;
pub use postgres::{PgConnectionParams, PgRepository, PgSslModeChoice};
pub use sqlite::SqliteRepository;

// Re-exports so callers don't have to know which crate hosts the types.
#[allow(unused_imports)]
pub use cadenza_proto::{
    Decisao, DecisaoRegistro, Estado, Ideia, IdeiaStatus, NewProposta, Proposta, Task,
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
    let mut comps = Path::new(id).components();
    let first = comps
        .next()
        .ok_or_else(|| StoreError::BadData("empty id".into()))?;
    if !matches!(first, Component::Normal(_)) || comps.next().is_some() {
        return Err(StoreError::BadData(format!("invalid id: {id}")));
    }
    Ok(())
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
        for bad in ["..", "../auth", "../../etc/passwd", "foo/bar", "foo\\bar", ".", ""] {
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
            assert!(matches!(validate_id("C:\\abs"), Err(StoreError::BadData(_))));
        }
    }

    #[test]
    fn rejects_nul_byte() {
        assert!(matches!(validate_id("foo\0bar"), Err(StoreError::BadData(_))));
    }
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
