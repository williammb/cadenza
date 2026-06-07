//! Filesystem-backed `Repository` impl.
//!
//! Wraps the original sync engines (`files_inner::Store`,
//! `triage_inner::Triage`) and exposes their surfaces through the async
//! trait. Each method just `?`-converts the inner error into the
//! unified `StoreError`.
//!
//! No `spawn_blocking`: filesystem ops on a desktop are sub-millisecond,
//! Tauri commands already run on a worker thread, and adding a thread
//! hop would cost more than it saves. If profiling later shows real
//! contention, switch hot paths to `tokio::task::spawn_blocking`.

use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::{
    events_inner::EventStore, files_inner::Store as FileStore, ideias_inner::IdeiaStore,
    jira_inner::JiraIssueStore, jira_review_inner::JiraReviews, memory_inner::MemoryStore,
    review_inner::Reviews, triage_inner::Triage as FileTriage, validate_id, DecisaoRegistro,
    Estado, Ideia, IdeiaStatus, IssueReviewPackage, JiraIssueRecord, MemoryItem, MemorySuggestion,
    NewProposta, PackageStatus, Proposta, RawEvent, Repository, Result, ReviewPackage, RunEvent,
    StoreError, Task,
};

/// Tasks live under `<home>/tasks/`, triage under `<home>/triage/`,
/// ideias under `<home>/inbox/`, memória sob `<home>/memory/`, review
/// packages under `<home>/reviews/`, jira issue records under
/// `<home>/jira/`.
pub struct FileRepository {
    tasks: Arc<FileStore>,
    triage: Arc<FileTriage>,
    ideias: Arc<IdeiaStore>,
    memory: Arc<MemoryStore>,
    reviews: Arc<Reviews>,
    jira: Arc<JiraIssueStore>,
    jira_reviews: Arc<JiraReviews>,
    events: Arc<EventStore>,
}

impl FileRepository {
    pub fn new(home: &Path) -> Result<Self> {
        let tasks = FileStore::new(home.join("tasks")).map_err(StoreError::Io)?;
        let triage = FileTriage::new(home.join("triage"))?;
        let ideias = IdeiaStore::new(home.join("inbox"))?;
        let memory = MemoryStore::new(home.join("memory"))?;
        let reviews = Reviews::new(home.join("reviews"))?;
        let jira = JiraIssueStore::new(home.join("jira"))?;
        // Aggregate (issue-owned) reviews live in a SUBDIR so the flat
        // `reviews/` scans never ingest them.
        let jira_reviews = JiraReviews::new(home.join("reviews").join("jira"))?;
        let events = EventStore::new(home.join("events"))?;
        Ok(Self {
            tasks: Arc::new(tasks),
            triage: Arc::new(triage),
            ideias: Arc::new(ideias),
            memory: Arc::new(memory),
            reviews: Arc::new(reviews),
            jira: Arc::new(jira),
            jira_reviews: Arc::new(jira_reviews),
            events: Arc::new(events),
        })
    }
}

#[async_trait]
impl Repository for FileRepository {
    async fn list_tasks(&self, filter: Option<Estado>) -> Result<Vec<Task>> {
        Ok(self.tasks.list_tasks(filter)?)
    }

    async fn read_task(&self, id: &str) -> Result<Task> {
        Ok(self.tasks.read_task(id)?)
    }

    async fn create_task(&self, task: &Task) -> Result<()> {
        Ok(self.tasks.create_task(task)?)
    }

    async fn set_estado(&self, id: &str, estado: Estado) -> Result<()> {
        Ok(self.tasks.set_estado(id, estado)?)
    }

    async fn set_titulo(&self, id: &str, titulo: &str) -> Result<()> {
        Ok(self.tasks.set_titulo(id, titulo)?)
    }

    async fn update_task_body(&self, id: &str, body: &str) -> Result<()> {
        Ok(self.tasks.update_task_body(id, body)?)
    }

    async fn delete_task(&self, id: &str) -> Result<()> {
        Ok(self.tasks.delete_task(id)?)
    }

    async fn append_log(&self, id: &str, text: &str) -> Result<()> {
        Ok(self.tasks.append_log(id, text)?)
    }

    async fn propose(&self, args: NewProposta) -> Result<Proposta> {
        Ok(self.triage.propose(args)?)
    }

    async fn read_proposta(&self, proposta_id: &str) -> Result<Option<Proposta>> {
        Ok(self.triage.read_proposta(proposta_id)?)
    }

    async fn read_decisao(&self, proposta_id: &str) -> Result<Option<DecisaoRegistro>> {
        Ok(self.triage.read_decisao(proposta_id)?)
    }

    async fn list_pending_propostas(&self) -> Result<Vec<Proposta>> {
        Ok(self.triage.list_pending()?)
    }

    async fn write_decisao(&self, registro: DecisaoRegistro) -> Result<()> {
        Ok(self.triage.write_decisao(registro)?)
    }

    async fn await_decisao(
        &self,
        proposta_id: &str,
        timeout: Duration,
    ) -> Result<Option<DecisaoRegistro>> {
        Ok(self.triage.await_decisao(proposta_id, timeout).await?)
    }

    async fn list_ideias(&self) -> Result<Vec<Ideia>> {
        Ok(self.ideias.list()?)
    }

    async fn read_ideia(&self, id: &str) -> Result<Option<Ideia>> {
        Ok(self.ideias.read(id)?)
    }

    async fn create_ideia(&self, ideia: &Ideia) -> Result<()> {
        // `create` (atomic via OpenOptions::create_new) replaces the
        // earlier `read + write` check-then-act, which had a TOCTOU
        // race where two concurrent creators with the same id both
        // saw `read == None` and the second silently overwrote the
        // first. DB backends enforce this via PRIMARY KEY.
        Ok(self.ideias.create(ideia)?)
    }

    async fn delete_ideia(&self, id: &str) -> Result<()> {
        Ok(self.ideias.delete(id)?)
    }

    async fn set_ideia_status(&self, id: &str, status: IdeiaStatus) -> Result<()> {
        Ok(self.ideias.set_status(id, status)?)
    }

    // ─── jira issues ───────────────────────────────────────────────

    async fn upsert_jira_issue(&self, record: &JiraIssueRecord) -> Result<()> {
        self.jira.upsert(record)
    }

    async fn read_jira_issue(&self, site: &str, issue_id: &str) -> Result<Option<JiraIssueRecord>> {
        self.jira.read(site, issue_id)
    }

    async fn list_jira_issues(&self) -> Result<Vec<JiraIssueRecord>> {
        self.jira.list()
    }

    async fn delete_jira_issue(&self, site: &str, issue_id: &str) -> Result<()> {
        self.jira.delete(site, issue_id)
    }

    // ─── memória ───────────────────────────────────────────────────
    // `project_id` é o nome do arquivo no backend de arquivos, então
    // validamos contra path traversal antes de qualquer `path_for`.

    async fn list_memory(&self, project_id: &str) -> Result<Vec<MemoryItem>> {
        validate_id(project_id)?;
        Ok(self.memory.list(project_id)?)
    }

    async fn add_memory_item(&self, project_id: &str, item: &MemoryItem) -> Result<()> {
        validate_id(project_id)?;
        Ok(self.memory.add_item(project_id, item)?)
    }

    async fn update_memory_item(&self, project_id: &str, item_id: &str, texto: &str) -> Result<()> {
        validate_id(project_id)?;
        Ok(self.memory.update_item(project_id, item_id, texto)?)
    }

    async fn delete_memory_item(&self, project_id: &str, item_id: &str) -> Result<()> {
        validate_id(project_id)?;
        Ok(self.memory.delete_item(project_id, item_id)?)
    }

    async fn list_memory_suggestions(&self, project_id: &str) -> Result<Vec<MemorySuggestion>> {
        Ok(self.memory.list_suggestions(project_id)?)
    }

    async fn read_memory_suggestion(&self, id: &str) -> Result<Option<MemorySuggestion>> {
        validate_id(id)?;
        Ok(self.memory.read_suggestion(id)?)
    }

    async fn create_memory_suggestion(&self, suggestion: &MemorySuggestion) -> Result<()> {
        validate_id(&suggestion.id)?;
        Ok(self.memory.create_suggestion(suggestion)?)
    }

    async fn delete_memory_suggestion(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        Ok(self.memory.delete_suggestion(id)?)
    }

    async fn all_memory_items(&self) -> Result<Vec<(String, MemoryItem)>> {
        Ok(self.memory.all_items()?)
    }

    async fn all_memory_suggestions(&self) -> Result<Vec<MemorySuggestion>> {
        Ok(self.memory.all_suggestions()?)
    }

    // ─── review packages ───────────────────────────────────────────

    async fn list_review_packages(&self, task_id: &str) -> Result<Vec<ReviewPackage>> {
        validate_id(task_id)?;
        Ok(self.reviews.list(task_id)?)
    }

    async fn upsert_review_package(&self, pkg: &ReviewPackage) -> Result<ReviewPackage> {
        validate_id(&pkg.task_id)?;
        Ok(self.reviews.upsert(pkg)?)
    }

    async fn mark_packages_superseded(&self, task_id: &str, except_attempt: u32) -> Result<()> {
        validate_id(task_id)?;
        Ok(self.reviews.mark_superseded(task_id, except_attempt)?)
    }

    async fn set_package_decision(
        &self,
        task_id: &str,
        attempt: u32,
        status: PackageStatus,
    ) -> Result<()> {
        validate_id(task_id)?;
        Ok(self.reviews.set_status(task_id, attempt, status)?)
    }

    async fn delete_review_packages(&self, task_id: &str) -> Result<()> {
        validate_id(task_id)?;
        Ok(self.reviews.delete_all(task_id)?)
    }

    async fn all_review_packages(&self) -> Result<Vec<ReviewPackage>> {
        Ok(self.reviews.all()?)
    }

    /// Atomic `done` via the write-ahead journal (PLAN §C.9). The package
    /// upsert + log append + estado flip are committed together; a crash
    /// anywhere is replayed at startup. The `task_ops` closure runs the
    /// `.md` side effects (log dedup + estado) so the `Reviews` engine
    /// stays focused on its sidecars.
    async fn done_with_review_package(
        &self,
        pkg: &ReviewPackage,
        log_line: Option<&str>,
        target_estado: Option<Estado>,
    ) -> Result<ReviewPackage> {
        validate_id(&pkg.task_id)?;
        let target_estado_str = target_estado.map(|e| e.as_str().to_string());
        match self
            .reviews
            .prepare_done(pkg, log_line.map(str::to_string), target_estado_str)?
        {
            // Key already seen ⇒ no-op returning the stored package.
            Err(existing) => Ok(existing),
            Ok(journal) => {
                let tasks = self.tasks.clone();
                let stored = self
                    .reviews
                    .commit_done(&journal, move |record| apply_task_ops(&tasks, record))?;
                Ok(stored)
            }
        }
    }

    // ─── aggregate (issue-owned) review packages (Slice 5) ─────────
    async fn upsert_issue_review_package(
        &self,
        pkg: &IssueReviewPackage,
    ) -> Result<IssueReviewPackage> {
        Ok(self.jira_reviews.upsert(pkg)?)
    }

    async fn list_issue_review_packages(
        &self,
        jira_site: &str,
        jira_issue_id: &str,
    ) -> Result<Vec<IssueReviewPackage>> {
        Ok(self.jira_reviews.list(jira_site, jira_issue_id)?)
    }

    async fn all_issue_review_packages(&self) -> Result<Vec<IssueReviewPackage>> {
        Ok(self.jira_reviews.all()?)
    }

    // ─── run timeline / audit event log (feature #8) ───────────────
    async fn append_event(&self, event: &RunEvent) -> Result<()> {
        Ok(self.events.append(event)?)
    }

    async fn list_events(
        &self,
        task_id: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Vec<RunEvent>> {
        Ok(self.events.list(task_id, limit)?)
    }

    async fn all_events(&self) -> Result<Vec<RunEvent>> {
        Ok(self.events.all()?)
    }

    async fn all_events_raw(&self) -> Result<Vec<RawEvent>> {
        Ok(self.events.all_raw()?)
    }

    async fn append_event_raw(&self, raw: &RawEvent) -> Result<()> {
        Ok(self.events.append_raw(&raw.payload)?)
    }
}

/// Apply the task `.md` side of a `done` journal: append the log line
/// (skipped when the body already ends with that exact line, so a replay or
/// retry can't double it) then set the target estado. Idempotent — both
/// steps are safe to re-run. Translated into the `review_inner` error type so
/// it can fail the journal commit and leave the WAL for the next boot.
fn apply_task_ops(
    tasks: &FileStore,
    record: &super::review_inner::DoneJournal,
) -> std::result::Result<(), super::review_inner::ReviewError> {
    use super::review_inner::ReviewError;
    if let Some(line) = &record.log_line {
        let task = tasks
            .read_task(&record.task_id)
            .map_err(|e| ReviewError::Other(anyhow::anyhow!(e.to_string())))?;
        if !body_ends_with_line(&task.body, line) {
            tasks
                .append_log(&record.task_id, line)
                .map_err(|e| ReviewError::Other(anyhow::anyhow!(e.to_string())))?;
        }
    }
    if let Some(estado) = &record.target_estado {
        let parsed = Estado::parse(estado)
            .ok_or_else(|| ReviewError::BadData(format!("bad target estado: {estado}")))?;
        tasks
            .set_estado(&record.task_id, parsed)
            .map_err(|e| ReviewError::Other(anyhow::anyhow!(e.to_string())))?;
    }
    Ok(())
}

/// True when `body`'s last non-empty line is exactly `line` (ignoring a
/// trailing newline). Used to dedup the `[done request]` append so the
/// legacy behavior (append once) is preserved across journal replays.
fn body_ends_with_line(body: &str, line: &str) -> bool {
    body.lines().last().map(str::trim_end) == Some(line.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::{EvidenceState, PackageStatus, RISK_HEURISTIC_VERSION};
    use tempfile::TempDir;

    fn mk_task(id: &str) -> Task {
        Task {
            id: id.into(),
            titulo: format!("{id} title"),
            estado: Estado::Fazendo,
            responsavel: "humano".into(),
            body: format!("# {id}\n\ninitial body\n"),
            worktree_path: None,
            branch: None,
            blocked_by: Vec::new(),
            jira_site: None,
            jira_issue_id: None,
            jira_key_display: None,
        }
    }

    fn mk_pkg(task_id: &str, key: &str) -> ReviewPackage {
        ReviewPackage {
            task_id: task_id.into(),
            attempt: 0,
            idempotency_key: key.into(),
            status: PackageStatus::Pending,
            checks: vec![],
            groups: vec![],
            open_questions: vec![],
            summary: "did it".into(),
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
        }
    }

    #[tokio::test]
    async fn done_with_review_package_is_idempotent_and_dedups_log() {
        let dir = TempDir::new().unwrap();
        let repo = FileRepository::new(dir.path()).unwrap();
        repo.create_task(&mk_task("T-1")).await.unwrap();

        let line = "[done request] finished";
        let first = repo
            .done_with_review_package(
                &mk_pkg("T-1", "k1"),
                Some(line),
                Some(Estado::AguardandoRevisao),
            )
            .await
            .unwrap();
        assert_eq!(first.attempt, 1);

        let task = repo.read_task("T-1").await.unwrap();
        assert_eq!(task.estado, Estado::AguardandoRevisao);
        let count_first = task.body.matches(line).count();
        assert_eq!(count_first, 1, "log line appended exactly once");

        // Re-run with the SAME key ⇒ no-op returning the stored package, no
        // second log line, one package only.
        let second = repo
            .done_with_review_package(
                &mk_pkg("T-1", "k1"),
                Some(line),
                Some(Estado::AguardandoRevisao),
            )
            .await
            .unwrap();
        assert_eq!(second.attempt, first.attempt);
        let task2 = repo.read_task("T-1").await.unwrap();
        assert_eq!(
            task2.body.matches(line).count(),
            1,
            "idempotent re-run must not append a second [done request] line"
        );
        assert_eq!(repo.list_review_packages("T-1").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn second_done_supersedes_first_and_appends_once() {
        let dir = TempDir::new().unwrap();
        let repo = FileRepository::new(dir.path()).unwrap();
        repo.create_task(&mk_task("T-2")).await.unwrap();

        repo.done_with_review_package(
            &mk_pkg("T-2", "k1"),
            Some("[done request] a"),
            Some(Estado::AguardandoRevisao),
        )
        .await
        .unwrap();
        let second = repo
            .done_with_review_package(
                &mk_pkg("T-2", "k2"),
                Some("[done request] b"),
                Some(Estado::AguardandoRevisao),
            )
            .await
            .unwrap();
        assert_eq!(second.attempt, 2);

        let list = repo.list_review_packages("T-2").await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].status, PackageStatus::Superseded);
        assert_eq!(list[1].status, PackageStatus::Pending);
    }

    fn mk_jira(site: &str, id: &str, key: &str) -> JiraIssueRecord {
        JiraIssueRecord {
            jira_site: site.into(),
            jira_issue_id: id.into(),
            jira_key: key.into(),
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
        }
    }

    #[tokio::test]
    async fn file_jira_issue_upsert_read_delete() {
        let dir = TempDir::new().unwrap();
        let repo = FileRepository::new(dir.path()).unwrap();
        repo.upsert_jira_issue(&mk_jira("site", "1", "PROJ-1"))
            .await
            .unwrap();
        assert_eq!(
            repo.read_jira_issue("site", "1")
                .await
                .unwrap()
                .unwrap()
                .jira_key,
            "PROJ-1"
        );
        // Upsert overwrites (changed key reflects on read).
        repo.upsert_jira_issue(&mk_jira("site", "1", "PROJ-2"))
            .await
            .unwrap();
        assert_eq!(
            repo.read_jira_issue("site", "1")
                .await
                .unwrap()
                .unwrap()
                .jira_key,
            "PROJ-2"
        );
        assert_eq!(repo.list_jira_issues().await.unwrap().len(), 1);
        repo.delete_jira_issue("site", "1").await.unwrap();
        assert!(repo.read_jira_issue("site", "1").await.unwrap().is_none());
        assert!(matches!(
            repo.delete_jira_issue("site", "1").await,
            Err(StoreError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn file_jira_issue_composite_key_with_special_chars() {
        let dir = TempDir::new().unwrap();
        let repo = FileRepository::new(dir.path()).unwrap();
        let site = "https://x.atlassian.net";
        repo.upsert_jira_issue(&mk_jira(site, "10001", "PROJ-123"))
            .await
            .unwrap();
        let got = repo.read_jira_issue(site, "10001").await.unwrap().unwrap();
        assert_eq!(got.jira_key, "PROJ-123");
        // A second pair on the same site must not collide.
        repo.upsert_jira_issue(&mk_jira(site, "10002", "PROJ-124"))
            .await
            .unwrap();
        assert_eq!(repo.list_jira_issues().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn file_task_roundtrip_carries_jira_identity() {
        // The file backend's frozen frontmatter cannot hold Jira identity,
        // so `read_task` returns None for those fields; the durable identity
        // lives in the `task-jira.json` sidecar (TaskJira), which restores it
        // via its `enrich` merge.
        let dir = TempDir::new().unwrap();
        let repo = FileRepository::new(dir.path()).unwrap();
        let mut task = mk_task("T-jira");
        task.estado = Estado::AFazer;
        task.jira_site = Some("https://x.atlassian.net".into());
        task.jira_issue_id = Some("10001".into());
        repo.create_task(&task).await.unwrap();

        // Store-level read drops identity (frozen frontmatter).
        let raw = repo.read_task("T-jira").await.unwrap();
        assert!(raw.jira_site.is_none());

        // The sidecar holds it durably and restores it on merge.
        let sidecar = crate::jira_sidecar::TaskJira::load(dir.path()).unwrap();
        sidecar
            .set("T-jira", "https://x.atlassian.net", "10001")
            .unwrap();
        let merged = sidecar.enrich(raw);
        assert_eq!(merged.jira_site.as_deref(), Some("https://x.atlassian.net"));
        assert_eq!(merged.jira_issue_id.as_deref(), Some("10001"));
    }
}
