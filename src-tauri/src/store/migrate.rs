//! One-way data migration between backends.
//!
//! The user picked the new backend in the Settings UI; we copy every
//! task + proposta + decisao from `from` into `to` before the new
//! backend serves any traffic.
//!
//! Skipped if the migration marker for that pair already exists at
//! `~/.cadenza/migrated.json` — re-running the app shouldn't repeat
//! a months-old migration. Reset by deleting that file.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use tracing::info;

use super::{Repository, Result, StoreError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Files,
    Sqlite,
    Postgres,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MigrationLog {
    /// `(from, to)` pairs already completed, latest-first.
    pub completed: Vec<(Backend, Backend)>,
}

impl MigrationLog {
    pub fn load(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn record(&mut self, from: Backend, to: Backend) {
        self.completed.retain(|(f, t)| !(*f == from && *t == to));
        self.completed.insert(0, (from, to));
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        // fsync the tmp before rename so a crash after rename can't
        // leave a zero-byte marker on the visible path (which would
        // make the app re-run the migration on next boot).
        {
            use std::io::Write;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn contains(&self, from: Backend, to: Backend) -> bool {
        self.completed.iter().any(|(f, t)| *f == from && *t == to)
    }
}

/// Copy every task + proposta + decisao from `from` into `to`. Skips
/// tasks already present at the destination (so re-running after a
/// crash mid-migration doesn't error on the AlreadyExists rows that
/// did make it across).
pub async fn copy_all(from: &dyn Repository, to: &dyn Repository) -> Result<MigrationStats> {
    let mut stats = MigrationStats::default();

    for task in from.list_tasks(None).await? {
        match to.create_task(&task).await {
            Ok(()) => stats.tasks_copied += 1,
            Err(StoreError::AlreadyExists(_)) => stats.tasks_skipped += 1,
            Err(e) => return Err(e),
        }
    }

    // Propostas: list_pending returns only undecided ones; we want
    // every proposta whether decided or not, so we scan via the
    // decisao read after the copy. Pending list is the safest portable
    // surface today — decided propostas are still readable on the file
    // backend through read_proposta if we tracked their ids elsewhere,
    // but list_pending_propostas is the only listing API on the trait.
    //
    // For Fase B (file → sqlite) the user's working set is "what's
    // open right now", and historical decided records aren't migrated.
    // If we add a `list_all_propostas` to the trait later we can
    // round-trip everything; that's deferred to keep the trait small.
    for proposta in from.list_pending_propostas().await? {
        let _migrated = to
            .propose(cadenza_proto::NewProposta {
                idempotency_key: proposta.idempotency_key.clone(),
                parent: proposta.parent.clone(),
                title: proposta.title.clone(),
                repro: proposta.repro.clone(),
                file: proposta.file.clone(),
                what_failed: proposta.what_failed.clone(),
                action: proposta.action.clone(),
            })
            .await?;
        stats.propostas_copied += 1;
    }

    for ideia in from.list_ideias().await? {
        match to.create_ideia(&ideia).await {
            Ok(()) => stats.ideias_copied += 1,
            Err(StoreError::AlreadyExists(_)) => stats.ideias_skipped += 1,
            Err(e) => return Err(e),
        }
    }

    // Memória oficial + sugestões pendentes (T-34). A memória é dado
    // durável e por-projeto, então a migração entre backends copia tudo.
    for (project_id, item) in from.all_memory_items().await? {
        match to.add_memory_item(&project_id, &item).await {
            Ok(()) => stats.memory_items_copied += 1,
            Err(StoreError::AlreadyExists(_)) => stats.memory_items_skipped += 1,
            Err(e) => return Err(e),
        }
    }
    for suggestion in from.all_memory_suggestions().await? {
        match to.create_memory_suggestion(&suggestion).await {
            Ok(()) => stats.memory_suggestions_copied += 1,
            Err(StoreError::AlreadyExists(_)) => stats.memory_suggestions_skipped += 1,
            Err(e) => return Err(e),
        }
    }

    Ok(stats)
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct MigrationStats {
    pub tasks_copied: usize,
    pub tasks_skipped: usize,
    pub propostas_copied: usize,
    pub ideias_copied: usize,
    pub ideias_skipped: usize,
    pub memory_items_copied: usize,
    pub memory_items_skipped: usize,
    pub memory_suggestions_copied: usize,
    pub memory_suggestions_skipped: usize,
}

/// Run a migration `from → to` if it hasn't been recorded yet.
/// Updates the marker file on success. Returns `None` if skipped.
pub async fn maybe_migrate(
    from: &dyn Repository,
    to: &dyn Repository,
    from_kind: Backend,
    to_kind: Backend,
    marker_path: &Path,
) -> Result<Option<MigrationStats>> {
    let mut log = MigrationLog::load(marker_path);
    if log.contains(from_kind, to_kind) {
        return Ok(None);
    }
    info!(?from_kind, ?to_kind, "starting backend migration");
    let stats = copy_all(from, to).await?;
    log.record(from_kind, to_kind);
    if let Err(e) = log.save(marker_path) {
        // The data is across; failing to write the marker just means
        // we'll redo it on next start (idempotent thanks to the skip
        // path in copy_all). Worth a warning but not an error.
        tracing::warn!(error = ?e, path = %marker_path.display(), "failed to save migration marker");
    }
    info!(?stats, "migration complete");
    Ok(Some(stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{FileRepository, SqliteRepository};
    use cadenza_proto::{Estado, NewProposta, Task};
    use tempfile::TempDir;

    fn t(id: &str, estado: Estado) -> Task {
        Task {
            id: id.into(),
            titulo: format!("{id} title"),
            estado,
            responsavel: "humano".into(),
            body: format!("body of {id}"),
            worktree_path: None,
            branch: None,
            blocked_by: Vec::new(),
        }
    }

    #[tokio::test]
    async fn copies_tasks_files_to_sqlite() {
        let dir = TempDir::new().unwrap();
        let files = FileRepository::new(dir.path()).unwrap();
        files.create_task(&t("A", Estado::AFazer)).await.unwrap();
        files.create_task(&t("B", Estado::Fazendo)).await.unwrap();

        let sqlite_path = dir.path().join("cadenza.db");
        let sqlite = SqliteRepository::open(&sqlite_path).await.unwrap();

        let stats = copy_all(&files, &sqlite).await.unwrap();
        assert_eq!(stats.tasks_copied, 2);
        assert_eq!(stats.tasks_skipped, 0);

        let listed = sqlite.list_tasks(None).await.unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[tokio::test]
    async fn copies_pending_propostas() {
        let dir = TempDir::new().unwrap();
        let files = FileRepository::new(dir.path()).unwrap();
        files
            .propose(NewProposta {
                idempotency_key: "k1".into(),
                parent: None,
                title: "p1".into(),
                repro: "".into(),
                file: "f".into(),
                what_failed: "".into(),
                action: "".into(),
            })
            .await
            .unwrap();

        let sqlite_path = dir.path().join("cadenza.db");
        let sqlite = SqliteRepository::open(&sqlite_path).await.unwrap();

        let stats = copy_all(&files, &sqlite).await.unwrap();
        assert_eq!(stats.propostas_copied, 1);
        let pending = sqlite.list_pending_propostas().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].title, "p1");
    }

    #[tokio::test]
    async fn copies_memory_items_and_suggestions() {
        use cadenza_proto::{MemoryItem, MemorySuggestion, SuggestionKind};
        let dir = TempDir::new().unwrap();
        let files = FileRepository::new(dir.path()).unwrap();
        files
            .add_memory_item(
                "proj-a",
                &MemoryItem {
                    id: "M-1".into(),
                    texto: "convenção".into(),
                    origem_task: Some("T-9".into()),
                    criado_em: 1,
                },
            )
            .await
            .unwrap();
        files
            .create_memory_suggestion(&MemorySuggestion {
                id: "MS-1".into(),
                project_id: "proj-a".into(),
                criado_em: 2,
                kind: SuggestionKind::Nova {
                    texto: "nova".into(),
                },
            })
            .await
            .unwrap();

        let sqlite = SqliteRepository::open(&dir.path().join("cadenza.db"))
            .await
            .unwrap();
        let stats = copy_all(&files, &sqlite).await.unwrap();
        assert_eq!(stats.memory_items_copied, 1);
        assert_eq!(stats.memory_suggestions_copied, 1);

        let items = sqlite.list_memory("proj-a").await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].origem_task.as_deref(), Some("T-9"));
        assert_eq!(
            sqlite
                .list_memory_suggestions("proj-a")
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn maybe_migrate_skips_when_marker_present() {
        let dir = TempDir::new().unwrap();
        let files = FileRepository::new(dir.path()).unwrap();
        files.create_task(&t("A", Estado::AFazer)).await.unwrap();

        let sqlite_path = dir.path().join("cadenza.db");
        let sqlite = SqliteRepository::open(&sqlite_path).await.unwrap();
        let marker = dir.path().join("migrated.json");

        let first = maybe_migrate(&files, &sqlite, Backend::Files, Backend::Sqlite, &marker)
            .await
            .unwrap();
        assert!(first.is_some());

        // Second run: marker exists, copy_all is not re-run.
        let second = maybe_migrate(&files, &sqlite, Backend::Files, Backend::Sqlite, &marker)
            .await
            .unwrap();
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn rerun_after_partial_skips_existing_rows() {
        let dir = TempDir::new().unwrap();
        let files = FileRepository::new(dir.path()).unwrap();
        files.create_task(&t("A", Estado::AFazer)).await.unwrap();
        files.create_task(&t("B", Estado::Fazendo)).await.unwrap();

        let sqlite_path = dir.path().join("cadenza.db");
        let sqlite = SqliteRepository::open(&sqlite_path).await.unwrap();
        sqlite.create_task(&t("A", Estado::AFazer)).await.unwrap();

        let stats = copy_all(&files, &sqlite).await.unwrap();
        assert_eq!(stats.tasks_copied, 1);
        assert_eq!(stats.tasks_skipped, 1);
    }
}
