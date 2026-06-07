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

use super::{PackageStatus, Repository, Result, StoreError};

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

    // NOTE (Slice 1): task Jira identity (`jira_site`/`jira_issue_id`) rides
    // along automatically because `create_task` carries those columns. The
    // `jira_issues` cache table is NOT copied here — backend-switch Jira
    // record migration is a later slice; the cache is re-derivable.
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
                jira_site: proposta.jira_site.clone(),
                jira_issue_id: proposta.jira_issue_id.clone(),
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

    // Review packages (PLAN §F.17). Copied in (task_id, attempt) order so
    // the destination re-derives the same 1..N attempt sequence; the
    // idempotency_key makes a re-run after a partial migration a no-op (the
    // destination upsert returns the stored row). `status` is recomputed at
    // the destination by the supersede-priors step, so a terminal decision
    // (aprovado / alteracoes_solicitadas) is re-applied explicitly to avoid
    // silently dropping an approval across a backend switch.
    for pkg in from.all_review_packages().await? {
        let already = to
            .list_review_packages(&pkg.task_id)
            .await?
            .into_iter()
            .any(|p| p.idempotency_key == pkg.idempotency_key);
        let copied = to.upsert_review_package(&pkg).await?;
        if already {
            stats.review_packages_skipped += 1;
        } else {
            stats.review_packages_copied += 1;
            if matches!(
                pkg.status,
                PackageStatus::Aprovado | PackageStatus::AlteracoesSolicitadas
            ) {
                to.set_package_decision(&pkg.task_id, copied.attempt, pkg.status)
                    .await?;
            }
        }
    }

    // Aggregate (issue-owned) review packages (Slice 5). Copied in
    // (jira_site, jira_issue_id, attempt) order so the destination re-derives
    // the same 1..N attempt sequence; the idempotency_key makes a re-run after
    // a partial migration a no-op (the destination upsert returns the stored
    // row). STATE-NEUTRAL: no estado side-effects. No decision re-apply —
    // Slice 5 has no decision path.
    //
    // `jira_review_packages` has NO foreign key to `jira_issues` (the cache
    // table is deliberately not migrated; jira_key_display falls back to
    // (site, issue_id)), so aggregates always copy regardless of whether a
    // parent row exists on the destination.
    for pkg in from.all_issue_review_packages().await? {
        let already = to
            .list_issue_review_packages(&pkg.jira_site, &pkg.jira_issue_id)
            .await?
            .into_iter()
            .any(|p| p.idempotency_key == pkg.idempotency_key);
        to.upsert_issue_review_package(&pkg).await?;
        if already {
            stats.issue_review_packages_skipped += 1;
        } else {
            stats.issue_review_packages_copied += 1;
        }
    }

    // Run timeline events (feature #8). Copied via the RAW payload path so an
    // unknown future event kind is moved byte-for-byte rather than flattened to
    // `Desconhecido` by a decode/re-encode through `RunEvent` — preserving the
    // forward-compat invariant of the append-only audit log. Append-only with a
    // stable `id`, so a re-run after a partial migration skips ids already
    // present (idempotent). Insertion order is preserved because
    // `all_events_raw` returns events in order and we append in that order.
    let existing_event_ids: std::collections::HashSet<String> = to
        .all_events_raw()
        .await?
        .into_iter()
        .map(|e| e.id)
        .collect();
    for raw in from.all_events_raw().await? {
        if existing_event_ids.contains(&raw.id) {
            stats.events_skipped += 1;
        } else {
            to.append_event_raw(&raw).await?;
            stats.events_copied += 1;
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
    pub review_packages_copied: usize,
    pub review_packages_skipped: usize,
    pub issue_review_packages_copied: usize,
    pub issue_review_packages_skipped: usize,
    pub events_copied: usize,
    pub events_skipped: usize,
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
            jira_site: None,
            jira_issue_id: None,
            jira_key_display: None,
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
                jira_site: None,
                jira_issue_id: None,
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
    async fn copies_review_packages_files_to_sqlite() {
        use crate::review::{EvidenceState, PackageStatus, ReviewPackage, RISK_HEURISTIC_VERSION};
        let dir = TempDir::new().unwrap();
        let files = FileRepository::new(dir.path()).unwrap();
        // The FK on the SQLite table requires the task to exist first.
        files.create_task(&t("T-1", Estado::Fazendo)).await.unwrap();

        let mk = |key: &str| ReviewPackage {
            task_id: "T-1".into(),
            attempt: 0,
            idempotency_key: key.into(),
            status: PackageStatus::Pending,
            checks: vec![],
            groups: vec![],
            open_questions: vec![],
            summary: "s".into(),
            changed_files: vec![],
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
            risks: vec![],
            secret_matches: vec![],
            evidence_state: EvidenceState::NoValidation,
            needs_focused_human_review: false,
            validation_scope_unknown: false,
            base_sha: None,
            head_sha: None,
            worktree_fingerprint: None,
            contract_version: None,
            reported_contract_version: None,
            risk_heuristic_version: RISK_HEURISTIC_VERSION,
            created_at_ms: 1,
            collection_duration_ms: 0,
            collection_errors: vec![],
            truncated: false,
            uncommitted_patch: None,
        };
        files.upsert_review_package(&mk("k1")).await.unwrap();
        files.upsert_review_package(&mk("k2")).await.unwrap();
        // Approve the latest so the decision is carried across the copy.
        files
            .set_package_decision("T-1", 2, PackageStatus::Aprovado)
            .await
            .unwrap();

        let sqlite = SqliteRepository::open(&dir.path().join("cadenza.db"))
            .await
            .unwrap();
        let stats = copy_all(&files, &sqlite).await.unwrap();
        assert_eq!(stats.review_packages_copied, 2);

        let listed = sqlite.list_review_packages("T-1").await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].attempt, 1);
        assert_eq!(listed[0].status, PackageStatus::Superseded);
        assert_eq!(listed[1].attempt, 2);
        assert_eq!(listed[1].status, PackageStatus::Aprovado);

        // Re-run after the data is across: keys already present -> skipped.
        let again = copy_all(&files, &sqlite).await.unwrap();
        assert_eq!(again.review_packages_copied, 0);
        assert_eq!(again.review_packages_skipped, 2);

        // delete_task cascade removes the packages.
        sqlite.delete_review_packages("T-1").await.unwrap();
        assert!(sqlite.list_review_packages("T-1").await.unwrap().is_empty());
    }

    // Regression (#8 forward-compat): an event whose `tipo` is unknown to this
    // binary must survive a backend migration byte-for-byte, NOT be flattened
    // to `{"tipo":"desconhecido"}` by a lossy decode/re-encode. Exercises the
    // raw copy path (all_events_raw / append_event_raw).
    #[tokio::test]
    async fn migration_preserves_unknown_future_event_kind() {
        use crate::store::RawEvent;
        let dir = TempDir::new().unwrap();
        let files = FileRepository::new(dir.path()).unwrap();

        // A future event kind this build doesn't know, with a real payload.
        let future = r#"{"id":"E-future","schema_version":99,"ts_ms":777,"task_id":"T-1","kind":{"tipo":"algo_do_futuro","x":1,"y":"z"}}"#;
        files
            .append_event_raw(&RawEvent {
                id: "E-future".into(),
                task_id: Some("T-1".into()),
                kind: "algo_do_futuro".into(),
                payload: future.into(),
                ts_ms: 777,
            })
            .await
            .unwrap();

        let sqlite = SqliteRepository::open(&dir.path().join("cadenza.db"))
            .await
            .unwrap();
        let stats = copy_all(&files, &sqlite).await.unwrap();
        assert_eq!(stats.events_copied, 1);

        let raw = sqlite.all_events_raw().await.unwrap();
        let copied = raw
            .iter()
            .find(|e| e.id == "E-future")
            .expect("event copied");
        assert_eq!(copied.kind, "algo_do_futuro", "kind column preserved");
        // The original tipo + payload survive — NOT rewritten to desconhecido.
        let v: serde_json::Value = serde_json::from_str(&copied.payload).unwrap();
        assert_eq!(v["kind"]["tipo"], "algo_do_futuro");
        assert_eq!(v["kind"]["x"], 1);
        assert_eq!(v["kind"]["y"], "z");
        assert_eq!(v["schema_version"], 99);

        // Re-run is idempotent: the id is already present -> skipped.
        let again = copy_all(&files, &sqlite).await.unwrap();
        assert_eq!(again.events_copied, 0);
        assert_eq!(again.events_skipped, 1);
    }

    #[tokio::test]
    async fn copies_issue_review_packages_files_to_sqlite() {
        use crate::review::issue::{IssuePackageStatus, IssueReviewPackage};
        use cadenza_proto::JiraIssueRecord;
        let dir = TempDir::new().unwrap();
        let files = FileRepository::new(dir.path()).unwrap();
        // The SQLite FK requires the parent jira_issue to exist first.
        files
            .upsert_jira_issue(&JiraIssueRecord {
                jira_site: "site".into(),
                jira_issue_id: "10001".into(),
                jira_key: "PROJ-1".into(),
                project_id: None,
                analysis_run_id: None,
                secret_hash: None,
                secret_expiry_ms: None,
                secret_status: None,
                raw_adf: None,
                branch_name: None,
                worktree_path: None,
                base_sha: None,
                worktree_state: None,
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .await
            .unwrap();

        let mk = |key: &str| IssueReviewPackage {
            jira_site: "site".into(),
            jira_issue_id: "10001".into(),
            attempt: 0,
            idempotency_key: key.into(),
            status: IssuePackageStatus::Pending,
            branch_name: "jira/10001-x".into(),
            base_sha: "base0".into(),
            head_sha: Some("head1".into()),
            changed_files: vec![],
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
            diff: None,
            truncated: false,
            collection_errors: vec![],
            created_at_ms: 1,
            collection_duration_ms: 0,
        };
        files.upsert_issue_review_package(&mk("k1")).await.unwrap();
        files.upsert_issue_review_package(&mk("k2")).await.unwrap();

        let sqlite = SqliteRepository::open(&dir.path().join("cadenza.db"))
            .await
            .unwrap();
        // `copy_all` does NOT migrate the `jira_issues` cache table (Slice 1),
        // and the SQLite aggregate table has an FK to it, so seed the parent in
        // the destination first — this is the realistic backend-switch setup
        // (the issues cache is re-fetched, not migrated).
        sqlite
            .upsert_jira_issue(&JiraIssueRecord {
                jira_site: "site".into(),
                jira_issue_id: "10001".into(),
                jira_key: "PROJ-1".into(),
                project_id: None,
                analysis_run_id: None,
                secret_hash: None,
                secret_expiry_ms: None,
                secret_status: None,
                raw_adf: None,
                branch_name: None,
                worktree_path: None,
                base_sha: None,
                worktree_state: None,
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .await
            .unwrap();
        let stats = copy_all(&files, &sqlite).await.unwrap();
        assert_eq!(stats.issue_review_packages_copied, 2);

        let listed = sqlite
            .list_issue_review_packages("site", "10001")
            .await
            .unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].attempt, 1);
        assert_eq!(listed[0].status, IssuePackageStatus::Superseded);
        assert_eq!(listed[1].attempt, 2);
        assert_eq!(listed[1].status, IssuePackageStatus::Pending);

        // Re-run: keys already present -> skipped.
        let again = copy_all(&files, &sqlite).await.unwrap();
        assert_eq!(again.issue_review_packages_copied, 0);
        assert_eq!(again.issue_review_packages_skipped, 2);
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
