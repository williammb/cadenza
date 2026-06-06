//! SQLite-backed `Repository` impl (Fase B).
//!
//! Schema lives in `src-tauri/migrations/*.sql` and is embedded at
//! compile time via `sqlx::migrate!`. The database is a single file at
//! `~/.cadenza/cadenza.db` (per the user's MVP choice) so backups are
//! one-file copies and reset is `rm cadenza.db`.
//!
//! `await_decisao` uses the same in-process `Notify` waiter pattern as
//! the file backend (SQLite has no NOTIFY) — every Cadenza process has
//! exactly one writer (this app), so a process-local waiter is enough.

use async_trait::async_trait;
use cadenza_proto::Decisao;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

use super::{
    DecisaoRegistro, Estado, Ideia, IdeiaStatus, IssueReviewPackage, JiraIssueRecord, MemoryItem,
    MemorySuggestion, NewProposta, PackageStatus, Proposta, Repository, Result, ReviewPackage,
    StoreError, SuggestionKind, Task,
};

/// Embedded migrations from `src-tauri/migrations/`. Runs every startup
/// via `migrator.run(&pool)` — sqlx tracks applied migrations in a
/// `_sqlx_migrations` table inside the same database.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub struct SqliteRepository {
    pool: SqlitePool,
    /// proposta_id → Notify woken when a decision is written. Same shape
    /// as the file backend's triage waiters; reset on process restart.
    waiters: Mutex<HashMap<String, Arc<Notify>>>,
}

impl SqliteRepository {
    /// Open (or create) the database file at `path` and run pending
    /// migrations. The connect options set `create_if_missing(true)` and
    /// `journal_mode=WAL` so concurrent reads don't block writes.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(StoreError::Io)?;
        }
        let url = format!("sqlite://{}", path.display());
        let opts = SqliteConnectOptions::from_str(&url)
            .map_err(|e| StoreError::Other(format!("sqlite connect opts: {e}")))?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .map_err(|e| StoreError::Other(format!("sqlite pool: {e}")))?;
        MIGRATOR
            .run(&pool)
            .await
            .map_err(|e| StoreError::Other(format!("sqlite migrate: {e}")))?;
        Ok(Self {
            pool,
            waiters: Mutex::new(HashMap::new()),
        })
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn map_sqlx(e: sqlx::Error) -> StoreError {
    match e {
        sqlx::Error::RowNotFound => StoreError::NotFound(String::new()),
        other => StoreError::Other(other.to_string()),
    }
}

fn task_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Task> {
    let estado_str: String = row.try_get("estado").map_err(map_sqlx)?;
    let estado = Estado::parse(&estado_str)
        .ok_or_else(|| StoreError::BadData(format!("unknown estado: {estado_str}")))?;
    Ok(Task {
        id: row.try_get("id").map_err(map_sqlx)?,
        titulo: row.try_get("titulo").map_err(map_sqlx)?,
        estado,
        responsavel: row.try_get("responsavel").map_err(map_sqlx)?,
        body: row.try_get("body").map_err(map_sqlx)?,
        worktree_path: None,
        branch: None,
        blocked_by: Vec::new(),
        jira_site: row.try_get("jira_site").map_err(map_sqlx)?,
        jira_issue_id: row.try_get("jira_issue_id").map_err(map_sqlx)?,
        jira_key_display: None,
    })
}

fn jira_issue_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<JiraIssueRecord> {
    Ok(JiraIssueRecord {
        jira_site: row.try_get("jira_site").map_err(map_sqlx)?,
        jira_issue_id: row.try_get("jira_issue_id").map_err(map_sqlx)?,
        jira_key: row.try_get("jira_key").map_err(map_sqlx)?,
        project_id: row.try_get("project_id").map_err(map_sqlx)?,
        analysis_run_id: row.try_get("analysis_run_id").map_err(map_sqlx)?,
        secret_hash: row.try_get("secret_hash").map_err(map_sqlx)?,
        secret_expiry_ms: row.try_get("secret_expiry_ms").map_err(map_sqlx)?,
        secret_status: row.try_get("secret_status").map_err(map_sqlx)?,
        raw_adf: row.try_get("raw_adf").map_err(map_sqlx)?,
        branch_name: row.try_get("branch_name").map_err(map_sqlx)?,
        worktree_path: row.try_get("worktree_path").map_err(map_sqlx)?,
        base_sha: row.try_get("base_sha").map_err(map_sqlx)?,
        worktree_state: row.try_get("worktree_state").map_err(map_sqlx)?,
        created_at_ms: row.try_get("created_at_ms").map_err(map_sqlx)?,
        updated_at_ms: row.try_get("updated_at_ms").map_err(map_sqlx)?,
    })
}

fn proposta_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Proposta> {
    Ok(Proposta {
        proposta_id: row.try_get("proposta_id").map_err(map_sqlx)?,
        idempotency_key: row.try_get("idempotency_key").map_err(map_sqlx)?,
        parent: row.try_get("parent").map_err(map_sqlx)?,
        title: row.try_get("title").map_err(map_sqlx)?,
        repro: row.try_get("repro").map_err(map_sqlx)?,
        file: row.try_get("file").map_err(map_sqlx)?,
        what_failed: row.try_get("what_failed").map_err(map_sqlx)?,
        action: row.try_get("action").map_err(map_sqlx)?,
        // Jira identity is not persisted on the propostas table in Slice 1;
        // it rides in-memory on the proposta only when freshly minted.
        jira_site: None,
        jira_issue_id: None,
        jira_key_display: None,
        created_at_ms: row.try_get("created_at_ms").map_err(map_sqlx)?,
    })
}

fn decisao_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<DecisaoRegistro> {
    let decisao_str: String = row.try_get("decisao").map_err(map_sqlx)?;
    let decisao = match decisao_str.as_str() {
        "aceita" => Decisao::Aceita,
        "rejeitada" => Decisao::Rejeitada,
        "mesclada" => Decisao::Mesclada,
        other => return Err(StoreError::BadData(format!("unknown decisao: {other}"))),
    };
    Ok(DecisaoRegistro {
        proposta_id: row.try_get("proposta_id").map_err(map_sqlx)?,
        decisao,
        task_id: row.try_get("task_id").map_err(map_sqlx)?,
        autor: row.try_get("autor").map_err(map_sqlx)?,
        decided_at_ms: row.try_get("decided_at_ms").map_err(map_sqlx)?,
    })
}

fn decisao_as_str(d: Decisao) -> &'static str {
    match d {
        Decisao::Aceita => "aceita",
        Decisao::Rejeitada => "rejeitada",
        Decisao::Mesclada => "mesclada",
    }
}

fn ideia_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Ideia> {
    let status_str: String = row.try_get("status").map_err(map_sqlx)?;
    let status = IdeiaStatus::parse(&status_str)
        .ok_or_else(|| StoreError::BadData(format!("unknown ideia status: {status_str}")))?;
    Ok(Ideia {
        id: row.try_get("id").map_err(map_sqlx)?,
        titulo: row.try_get("titulo").map_err(map_sqlx)?,
        body: row.try_get("body").map_err(map_sqlx)?,
        project_id: row.try_get("project_id").map_err(map_sqlx)?,
        status,
        created_at_ms: row.try_get("created_at_ms").map_err(map_sqlx)?,
    })
}

fn memory_item_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<MemoryItem> {
    Ok(MemoryItem {
        id: row.try_get("id").map_err(map_sqlx)?,
        texto: row.try_get("texto").map_err(map_sqlx)?,
        origem_task: row.try_get("origem_task").map_err(map_sqlx)?,
        criado_em: row.try_get("criado_em").map_err(map_sqlx)?,
    })
}

fn memory_suggestion_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<MemorySuggestion> {
    let kind_json: String = row.try_get("kind_json").map_err(map_sqlx)?;
    let kind: SuggestionKind = serde_json::from_str(&kind_json)
        .map_err(|e| StoreError::BadData(format!("bad suggestion kind json: {e}")))?;
    Ok(MemorySuggestion {
        id: row.try_get("id").map_err(map_sqlx)?,
        project_id: row.try_get("project_id").map_err(map_sqlx)?,
        criado_em: row.try_get("criado_em").map_err(map_sqlx)?,
        kind,
    })
}

/// Canonical snake_case string for a package's lifecycle status — the value
/// stored in the `status` column (matches the CHECK in 004_reviews.sql and
/// the `serde(rename_all = "snake_case")` wire form).
fn package_status_as_str(s: PackageStatus) -> &'static str {
    match s {
        PackageStatus::Pending => "pending",
        PackageStatus::Superseded => "superseded",
        PackageStatus::Aprovado => "aprovado",
        PackageStatus::AlteracoesSolicitadas => "alteracoes_solicitadas",
    }
}

fn package_status_parse(s: &str) -> Result<PackageStatus> {
    match s {
        "pending" => Ok(PackageStatus::Pending),
        "superseded" => Ok(PackageStatus::Superseded),
        "aprovado" => Ok(PackageStatus::Aprovado),
        "alteracoes_solicitadas" => Ok(PackageStatus::AlteracoesSolicitadas),
        other => Err(StoreError::BadData(format!(
            "unknown package status: {other}"
        ))),
    }
}

/// Reconstruct the package from its JSON `payload`, then overlay the
/// authoritative column values (`status` is the queryable source of truth
/// for lifecycle, so a supersede UPDATE doesn't require rewriting the blob).
fn review_package_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<ReviewPackage> {
    let payload: String = row.try_get("payload").map_err(map_sqlx)?;
    let mut pkg: ReviewPackage = serde_json::from_str(&payload)
        .map_err(|e| StoreError::BadData(format!("bad review package payload: {e}")))?;
    let status: String = row.try_get("status").map_err(map_sqlx)?;
    pkg.status = package_status_parse(&status)?;
    let attempt: i64 = row.try_get("attempt").map_err(map_sqlx)?;
    pkg.attempt = attempt as u32;
    Ok(pkg)
}

/// Reconstruct the aggregate package from its JSON `payload`, then overlay the
/// authoritative `status`/`attempt` columns (mirroring
/// [`review_package_from_row`]). The supersede UPDATE only touches the column,
/// not the blob, so the column is the source of truth for lifecycle.
fn issue_review_package_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<IssueReviewPackage> {
    let payload: String = row.try_get("payload").map_err(map_sqlx)?;
    let mut pkg: IssueReviewPackage = serde_json::from_str(&payload)
        .map_err(|e| StoreError::BadData(format!("bad aggregate review payload: {e}")))?;
    let status: String = row.try_get("status").map_err(map_sqlx)?;
    pkg.status = issue_package_status_parse(&status)?;
    let attempt: i64 = row.try_get("attempt").map_err(map_sqlx)?;
    pkg.attempt = attempt as u32;
    Ok(pkg)
}

/// Parse the aggregate status column string into [`IssuePackageStatus`].
fn issue_package_status_parse(s: &str) -> Result<crate::review::issue::IssuePackageStatus> {
    use crate::review::issue::IssuePackageStatus;
    match s {
        "pending" => Ok(IssuePackageStatus::Pending),
        "superseded" => Ok(IssuePackageStatus::Superseded),
        "aprovado" => Ok(IssuePackageStatus::Aprovado),
        "alteracoes_solicitadas" => Ok(IssuePackageStatus::AlteracoesSolicitadas),
        other => Err(StoreError::BadData(format!(
            "unknown aggregate review status: {other}"
        ))),
    }
}

#[async_trait]
impl Repository for SqliteRepository {
    async fn list_tasks(&self, filter: Option<Estado>) -> Result<Vec<Task>> {
        let rows = match filter {
            Some(e) => {
                sqlx::query("SELECT * FROM tasks WHERE estado = ?1 ORDER BY id")
                    .bind(e.as_str())
                    .fetch_all(&self.pool)
                    .await
            }
            None => {
                sqlx::query("SELECT * FROM tasks ORDER BY id")
                    .fetch_all(&self.pool)
                    .await
            }
        }
        .map_err(map_sqlx)?;
        rows.iter().map(task_from_row).collect()
    }

    async fn read_task(&self, id: &str) -> Result<Task> {
        let row = sqlx::query("SELECT * FROM tasks WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        task_from_row(&row)
    }

    async fn create_task(&self, task: &Task) -> Result<()> {
        let now = now_ms();
        let res = sqlx::query(
            "INSERT INTO tasks (id, titulo, estado, responsavel, body, created_at_ms, updated_at_ms, jira_site, jira_issue_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8)",
        )
        .bind(&task.id)
        .bind(&task.titulo)
        .bind(task.estado.as_str())
        .bind(&task.responsavel)
        .bind(&task.body)
        .bind(now)
        .bind(&task.jira_site)
        .bind(&task.jira_issue_id)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                Err(StoreError::AlreadyExists(task.id.clone()))
            }
            Err(e) => Err(map_sqlx(e)),
        }
    }

    async fn set_estado(&self, id: &str, estado: Estado) -> Result<()> {
        let res = sqlx::query("UPDATE tasks SET estado = ?1, updated_at_ms = ?2 WHERE id = ?3")
            .bind(estado.as_str())
            .bind(now_ms())
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn set_titulo(&self, id: &str, titulo: &str) -> Result<()> {
        let res = sqlx::query("UPDATE tasks SET titulo = ?1, updated_at_ms = ?2 WHERE id = ?3")
            .bind(titulo)
            .bind(now_ms())
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn update_task_body(&self, id: &str, body: &str) -> Result<()> {
        let res = sqlx::query("UPDATE tasks SET body = ?1, updated_at_ms = ?2 WHERE id = ?3")
            .bind(body)
            .bind(now_ms())
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn delete_task(&self, id: &str) -> Result<()> {
        let res = sqlx::query("DELETE FROM tasks WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(id.to_string()));
        }
        Ok(())
    }

    /// Append a log line to `body`. Matches the file backend's
    /// `append_log`: text is appended as-is, with a trailing newline
    /// added if missing. Read-modify-write inside a transaction so
    /// concurrent appends don't lose lines.
    async fn append_log(&self, id: &str, text: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
        let row = sqlx::query("SELECT body FROM tasks WHERE id = ?1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        let row = row.ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        let body: String = row.try_get("body").map_err(map_sqlx)?;
        let mut next = body;
        next.push_str(text);
        if !text.ends_with('\n') {
            next.push('\n');
        }
        sqlx::query("UPDATE tasks SET body = ?1, updated_at_ms = ?2 WHERE id = ?3")
            .bind(next)
            .bind(now_ms())
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        tx.commit().await.map_err(map_sqlx)?;
        Ok(())
    }

    async fn propose(&self, args: NewProposta) -> Result<Proposta> {
        // Fast dedup path: existing key returns the original proposta.
        if let Some(row) = sqlx::query("SELECT * FROM propostas WHERE idempotency_key = ?1")
            .bind(&args.idempotency_key)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?
        {
            return proposta_from_row(&row);
        }

        let proposta_id = format!("P-{}", Uuid::new_v4().simple());
        let created_at_ms = now_ms();
        let proposta = Proposta {
            proposta_id: proposta_id.clone(),
            idempotency_key: args.idempotency_key.clone(),
            parent: args.parent.clone(),
            title: args.title.clone(),
            repro: args.repro.clone(),
            file: args.file.clone(),
            what_failed: args.what_failed.clone(),
            action: args.action.clone(),
            jira_site: args.jira_site.clone(),
            jira_issue_id: args.jira_issue_id.clone(),
            jira_key_display: None,
            created_at_ms,
        };

        let res = sqlx::query(
            "INSERT INTO propostas (
                proposta_id, idempotency_key, parent, title, repro, file,
                what_failed, action, created_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .bind(&proposta.proposta_id)
        .bind(&proposta.idempotency_key)
        .bind(&proposta.parent)
        .bind(&proposta.title)
        .bind(&proposta.repro)
        .bind(&proposta.file)
        .bind(&proposta.what_failed)
        .bind(&proposta.action)
        .bind(proposta.created_at_ms)
        .execute(&self.pool)
        .await;

        match res {
            Ok(_) => Ok(proposta),
            // A concurrent writer raced us on the same key: re-fetch
            // the winner instead of returning two distinct proposta_ids
            // for one key.
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                let row = sqlx::query("SELECT * FROM propostas WHERE idempotency_key = ?1")
                    .bind(&args.idempotency_key)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(map_sqlx)?;
                proposta_from_row(&row)
            }
            Err(e) => Err(map_sqlx(e)),
        }
    }

    async fn read_proposta(&self, proposta_id: &str) -> Result<Option<Proposta>> {
        let row = sqlx::query("SELECT * FROM propostas WHERE proposta_id = ?1")
            .bind(proposta_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?;
        match row {
            Some(r) => Ok(Some(proposta_from_row(&r)?)),
            None => Ok(None),
        }
    }

    async fn read_decisao(&self, proposta_id: &str) -> Result<Option<DecisaoRegistro>> {
        let row = sqlx::query("SELECT * FROM decisoes WHERE proposta_id = ?1")
            .bind(proposta_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?;
        match row {
            Some(r) => Ok(Some(decisao_from_row(&r)?)),
            None => Ok(None),
        }
    }

    async fn list_pending_propostas(&self) -> Result<Vec<Proposta>> {
        let rows = sqlx::query(
            "SELECT p.* FROM propostas p
             LEFT JOIN decisoes d ON d.proposta_id = p.proposta_id
             WHERE d.proposta_id IS NULL
             ORDER BY p.created_at_ms",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        rows.iter().map(proposta_from_row).collect()
    }

    async fn write_decisao(&self, registro: DecisaoRegistro) -> Result<()> {
        sqlx::query(
            "INSERT INTO decisoes (proposta_id, decisao, task_id, autor, decided_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(proposta_id) DO UPDATE SET
                decisao = excluded.decisao,
                task_id = excluded.task_id,
                autor = excluded.autor,
                decided_at_ms = excluded.decided_at_ms",
        )
        .bind(&registro.proposta_id)
        .bind(decisao_as_str(registro.decisao.clone()))
        .bind(&registro.task_id)
        .bind(&registro.autor)
        .bind(registro.decided_at_ms)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;

        // Wake any in-process waiter.
        let mut waiters = self.waiters.lock().await;
        if let Some(n) = waiters.remove(&registro.proposta_id) {
            n.notify_waiters();
        }
        Ok(())
    }

    async fn await_decisao(
        &self,
        proposta_id: &str,
        timeout: Duration,
    ) -> Result<Option<DecisaoRegistro>> {
        // Fast path: already decided.
        if let Some(d) = self.read_decisao(proposta_id).await? {
            return Ok(Some(d));
        }
        // Register / reuse the waiter.
        let notify = {
            let mut waiters = self.waiters.lock().await;
            waiters
                .entry(proposta_id.to_string())
                .or_insert_with(|| Arc::new(Notify::new()))
                .clone()
        };
        // Arm the `Notified` future BEFORE the second disk check so a
        // writer landing between the check and our await still wakes
        // us. `Notify::notify_waiters` stores no permit, so a future
        // that isn't yet armed when notify fires misses the wake.
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(d) = self.read_decisao(proposta_id).await? {
            return Ok(Some(d));
        }
        match tokio::time::timeout(timeout, notified).await {
            Ok(()) => Ok(self.read_decisao(proposta_id).await?),
            Err(_) => Ok(None),
        }
    }

    async fn list_ideias(&self) -> Result<Vec<Ideia>> {
        let rows = sqlx::query("SELECT * FROM ideias ORDER BY created_at_ms")
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
        rows.iter().map(ideia_from_row).collect()
    }

    async fn read_ideia(&self, id: &str) -> Result<Option<Ideia>> {
        let row = sqlx::query("SELECT * FROM ideias WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?;
        match row {
            Some(r) => Ok(Some(ideia_from_row(&r)?)),
            None => Ok(None),
        }
    }

    async fn create_ideia(&self, ideia: &Ideia) -> Result<()> {
        let res = sqlx::query(
            "INSERT INTO ideias (id, titulo, body, project_id, status, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&ideia.id)
        .bind(&ideia.titulo)
        .bind(&ideia.body)
        .bind(&ideia.project_id)
        .bind(ideia.status.as_str())
        .bind(ideia.created_at_ms)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                Err(StoreError::AlreadyExists(ideia.id.clone()))
            }
            Err(e) => Err(map_sqlx(e)),
        }
    }

    async fn delete_ideia(&self, id: &str) -> Result<()> {
        let res = sqlx::query("DELETE FROM ideias WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn set_ideia_status(&self, id: &str, status: IdeiaStatus) -> Result<()> {
        let res = sqlx::query("UPDATE ideias SET status = ?1 WHERE id = ?2")
            .bind(status.as_str())
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(id.to_string()));
        }
        Ok(())
    }

    // ─── jira issues ───────────────────────────────────────────────

    async fn upsert_jira_issue(&self, record: &JiraIssueRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO jira_issues
               (jira_site, jira_issue_id, jira_key, project_id, analysis_run_id,
                secret_hash, secret_expiry_ms, secret_status, raw_adf,
                branch_name, worktree_path, base_sha, worktree_state,
                created_at_ms, updated_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
             ON CONFLICT(jira_site, jira_issue_id) DO UPDATE SET
               jira_key=excluded.jira_key, project_id=excluded.project_id,
               analysis_run_id=excluded.analysis_run_id, secret_hash=excluded.secret_hash,
               secret_expiry_ms=excluded.secret_expiry_ms, secret_status=excluded.secret_status,
               raw_adf=excluded.raw_adf, branch_name=excluded.branch_name,
               worktree_path=excluded.worktree_path, base_sha=excluded.base_sha,
               worktree_state=excluded.worktree_state, updated_at_ms=excluded.updated_at_ms",
        )
        .bind(&record.jira_site)
        .bind(&record.jira_issue_id)
        .bind(&record.jira_key)
        .bind(&record.project_id)
        .bind(&record.analysis_run_id)
        .bind(&record.secret_hash)
        .bind(record.secret_expiry_ms)
        .bind(&record.secret_status)
        .bind(&record.raw_adf)
        .bind(&record.branch_name)
        .bind(&record.worktree_path)
        .bind(&record.base_sha)
        .bind(&record.worktree_state)
        .bind(record.created_at_ms)
        .bind(record.updated_at_ms)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(())
    }

    async fn read_jira_issue(
        &self,
        jira_site: &str,
        jira_issue_id: &str,
    ) -> Result<Option<JiraIssueRecord>> {
        let row =
            sqlx::query("SELECT * FROM jira_issues WHERE jira_site = ?1 AND jira_issue_id = ?2")
                .bind(jira_site)
                .bind(jira_issue_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(map_sqlx)?;
        match row {
            Some(r) => Ok(Some(jira_issue_from_row(&r)?)),
            None => Ok(None),
        }
    }

    async fn list_jira_issues(&self) -> Result<Vec<JiraIssueRecord>> {
        let rows = sqlx::query("SELECT * FROM jira_issues ORDER BY jira_site, jira_issue_id")
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
        rows.iter().map(jira_issue_from_row).collect()
    }

    async fn delete_jira_issue(&self, jira_site: &str, jira_issue_id: &str) -> Result<()> {
        let res =
            sqlx::query("DELETE FROM jira_issues WHERE jira_site = ?1 AND jira_issue_id = ?2")
                .bind(jira_site)
                .bind(jira_issue_id)
                .execute(&self.pool)
                .await
                .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("{jira_site}/{jira_issue_id}")));
        }
        Ok(())
    }

    async fn list_memory(&self, project_id: &str) -> Result<Vec<MemoryItem>> {
        let rows =
            sqlx::query("SELECT * FROM memory_items WHERE project_id = ?1 ORDER BY criado_em, id")
                .bind(project_id)
                .fetch_all(&self.pool)
                .await
                .map_err(map_sqlx)?;
        rows.iter().map(memory_item_from_row).collect()
    }

    async fn add_memory_item(&self, project_id: &str, item: &MemoryItem) -> Result<()> {
        let res = sqlx::query(
            "INSERT INTO memory_items (id, project_id, texto, origem_task, criado_em)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&item.id)
        .bind(project_id)
        .bind(&item.texto)
        .bind(&item.origem_task)
        .bind(item.criado_em)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                Err(StoreError::AlreadyExists(item.id.clone()))
            }
            Err(e) => Err(map_sqlx(e)),
        }
    }

    async fn update_memory_item(&self, project_id: &str, item_id: &str, texto: &str) -> Result<()> {
        let res =
            sqlx::query("UPDATE memory_items SET texto = ?1 WHERE id = ?2 AND project_id = ?3")
                .bind(texto)
                .bind(item_id)
                .bind(project_id)
                .execute(&self.pool)
                .await
                .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(item_id.to_string()));
        }
        Ok(())
    }

    async fn delete_memory_item(&self, project_id: &str, item_id: &str) -> Result<()> {
        let res = sqlx::query("DELETE FROM memory_items WHERE id = ?1 AND project_id = ?2")
            .bind(item_id)
            .bind(project_id)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(item_id.to_string()));
        }
        Ok(())
    }

    async fn list_memory_suggestions(&self, project_id: &str) -> Result<Vec<MemorySuggestion>> {
        let rows = sqlx::query(
            "SELECT * FROM memory_suggestions WHERE project_id = ?1 ORDER BY criado_em, id",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        rows.iter().map(memory_suggestion_from_row).collect()
    }

    async fn read_memory_suggestion(&self, id: &str) -> Result<Option<MemorySuggestion>> {
        let row = sqlx::query("SELECT * FROM memory_suggestions WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?;
        match row {
            Some(r) => Ok(Some(memory_suggestion_from_row(&r)?)),
            None => Ok(None),
        }
    }

    async fn create_memory_suggestion(&self, suggestion: &MemorySuggestion) -> Result<()> {
        let kind_json = serde_json::to_string(&suggestion.kind)
            .map_err(|e| StoreError::BadData(e.to_string()))?;
        let res = sqlx::query(
            "INSERT INTO memory_suggestions (id, project_id, criado_em, kind_json)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(&suggestion.id)
        .bind(&suggestion.project_id)
        .bind(suggestion.criado_em)
        .bind(kind_json)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                Err(StoreError::AlreadyExists(suggestion.id.clone()))
            }
            Err(e) => Err(map_sqlx(e)),
        }
    }

    async fn delete_memory_suggestion(&self, id: &str) -> Result<()> {
        let res = sqlx::query("DELETE FROM memory_suggestions WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn all_memory_items(&self) -> Result<Vec<(String, MemoryItem)>> {
        let rows = sqlx::query("SELECT * FROM memory_items ORDER BY criado_em, id")
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
        rows.iter()
            .map(|r| {
                let project_id: String = r.try_get("project_id").map_err(map_sqlx)?;
                Ok((project_id, memory_item_from_row(r)?))
            })
            .collect()
    }

    async fn all_memory_suggestions(&self) -> Result<Vec<MemorySuggestion>> {
        let rows = sqlx::query("SELECT * FROM memory_suggestions ORDER BY criado_em, id")
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
        rows.iter().map(memory_suggestion_from_row).collect()
    }

    // ─── review packages ───────────────────────────────────────────

    async fn list_review_packages(&self, task_id: &str) -> Result<Vec<ReviewPackage>> {
        let rows = sqlx::query("SELECT * FROM review_packages WHERE task_id = ?1 ORDER BY attempt")
            .bind(task_id)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
        rows.iter().map(review_package_from_row).collect()
    }

    async fn latest_review_package(&self, task_id: &str) -> Result<Option<ReviewPackage>> {
        let row = sqlx::query(
            "SELECT * FROM review_packages WHERE task_id = ?1 ORDER BY attempt DESC LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        match row {
            Some(r) => Ok(Some(review_package_from_row(&r)?)),
            None => Ok(None),
        }
    }

    /// Single transaction: dedup-check → MAX(attempt)+1 → supersede priors →
    /// insert. Re-running the same `(task_id, idempotency_key)` returns the
    /// stored row unchanged (no-op); a concurrent racer that won the UNIQUE
    /// constraint is re-read.
    async fn upsert_review_package(&self, pkg: &ReviewPackage) -> Result<ReviewPackage> {
        super::validate_idempotency_key(&pkg.idempotency_key)?;
        let now = now_ms();
        let mut tx = self.pool.begin().await.map_err(map_sqlx)?;

        if let Some(row) =
            sqlx::query("SELECT * FROM review_packages WHERE task_id = ?1 AND idempotency_key = ?2")
                .bind(&pkg.task_id)
                .bind(&pkg.idempotency_key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?
        {
            tx.commit().await.map_err(map_sqlx)?;
            return review_package_from_row(&row);
        }

        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(attempt), 0) + 1 FROM review_packages WHERE task_id = ?1",
        )
        .bind(&pkg.task_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        // Supersede every prior Pending attempt.
        sqlx::query(
            "UPDATE review_packages SET status = 'superseded'
             WHERE task_id = ?1 AND status = 'pending'",
        )
        .bind(&pkg.task_id)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        let mut stored = pkg.clone();
        stored.attempt = next as u32;
        stored.status = PackageStatus::Pending;
        let payload =
            serde_json::to_string(&stored).map_err(|e| StoreError::BadData(e.to_string()))?;

        let res = sqlx::query(
            "INSERT INTO review_packages
                (task_id, attempt, idempotency_key, status, payload, created_at_ms)
             VALUES (?1, ?2, ?3, 'pending', ?4, ?5)",
        )
        .bind(&stored.task_id)
        .bind(next)
        .bind(&stored.idempotency_key)
        .bind(&payload)
        .bind(now)
        .execute(&mut *tx)
        .await;

        match res {
            Ok(_) => {
                tx.commit().await.map_err(map_sqlx)?;
                Ok(stored)
            }
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                tx.rollback().await.ok();
                let row = sqlx::query(
                    "SELECT * FROM review_packages WHERE task_id = ?1 AND idempotency_key = ?2",
                )
                .bind(&pkg.task_id)
                .bind(&pkg.idempotency_key)
                .fetch_one(&self.pool)
                .await
                .map_err(map_sqlx)?;
                review_package_from_row(&row)
            }
            Err(e) => {
                tx.rollback().await.ok();
                Err(map_sqlx(e))
            }
        }
    }

    async fn mark_packages_superseded(&self, task_id: &str, except_attempt: u32) -> Result<()> {
        sqlx::query(
            "UPDATE review_packages SET status = 'superseded'
             WHERE task_id = ?1 AND attempt <> ?2 AND status = 'pending'",
        )
        .bind(task_id)
        .bind(except_attempt as i64)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(())
    }

    async fn set_package_decision(
        &self,
        task_id: &str,
        attempt: u32,
        status: PackageStatus,
    ) -> Result<()> {
        let res = sqlx::query(
            "UPDATE review_packages SET status = ?1 WHERE task_id = ?2 AND attempt = ?3",
        )
        .bind(package_status_as_str(status))
        .bind(task_id)
        .bind(attempt as i64)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("{task_id}.a{attempt}")));
        }
        Ok(())
    }

    async fn delete_review_packages(&self, task_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM review_packages WHERE task_id = ?1")
            .bind(task_id)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        Ok(())
    }

    async fn all_review_packages(&self) -> Result<Vec<ReviewPackage>> {
        let rows = sqlx::query("SELECT * FROM review_packages ORDER BY task_id, attempt")
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
        rows.iter().map(review_package_from_row).collect()
    }

    /// Single transaction (PLAN §C.9): dedup-check the package key →
    /// allocate `attempt` → supersede priors → insert → append the log line
    /// (deduped against the body's last line) → flip estado. Re-running the
    /// same key short-circuits to the stored package with no `.md` mutation.
    async fn done_with_review_package(
        &self,
        pkg: &ReviewPackage,
        log_line: Option<&str>,
        target_estado: Option<Estado>,
    ) -> Result<ReviewPackage> {
        super::validate_idempotency_key(&pkg.idempotency_key)?;
        let now = now_ms();
        let mut tx = self.pool.begin().await.map_err(map_sqlx)?;

        // Idempotent no-op: the key already produced a package.
        if let Some(row) =
            sqlx::query("SELECT * FROM review_packages WHERE task_id = ?1 AND idempotency_key = ?2")
                .bind(&pkg.task_id)
                .bind(&pkg.idempotency_key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?
        {
            tx.commit().await.map_err(map_sqlx)?;
            return review_package_from_row(&row);
        }

        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(attempt), 0) + 1 FROM review_packages WHERE task_id = ?1",
        )
        .bind(&pkg.task_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        sqlx::query(
            "UPDATE review_packages SET status = 'superseded'
             WHERE task_id = ?1 AND status = 'pending'",
        )
        .bind(&pkg.task_id)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        let mut stored = pkg.clone();
        stored.attempt = next as u32;
        stored.status = PackageStatus::Pending;
        let payload =
            serde_json::to_string(&stored).map_err(|e| StoreError::BadData(e.to_string()))?;

        sqlx::query(
            "INSERT INTO review_packages
                (task_id, attempt, idempotency_key, status, payload, created_at_ms)
             VALUES (?1, ?2, ?3, 'pending', ?4, ?5)",
        )
        .bind(&stored.task_id)
        .bind(next)
        .bind(&stored.idempotency_key)
        .bind(&payload)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        // Read the task body once for the log dedup + the NotFound guard.
        let body_row = sqlx::query("SELECT body FROM tasks WHERE id = ?1")
            .bind(&stored.task_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?
            .ok_or_else(|| StoreError::NotFound(stored.task_id.clone()))?;
        let mut body: String = body_row.try_get("body").map_err(map_sqlx)?;

        if let Some(line) = log_line {
            if !body_ends_with_line(&body, line) {
                body.push_str(line);
                if !line.ends_with('\n') {
                    body.push('\n');
                }
            }
        }
        let estado = target_estado.unwrap_or(Estado::AguardandoRevisao);
        sqlx::query("UPDATE tasks SET body = ?1, estado = ?2, updated_at_ms = ?3 WHERE id = ?4")
            .bind(&body)
            .bind(estado.as_str())
            .bind(now)
            .bind(&stored.task_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;

        tx.commit().await.map_err(map_sqlx)?;
        Ok(stored)
    }

    // ─── aggregate (issue-owned) review packages (Slice 5) ─────────
    // STATE-NEUTRAL: these touch ONLY `jira_review_packages` — never `tasks`.

    async fn upsert_issue_review_package(
        &self,
        pkg: &IssueReviewPackage,
    ) -> Result<IssueReviewPackage> {
        super::validate_idempotency_key(&pkg.idempotency_key)?;
        let now = now_ms();
        let mut tx = self.pool.begin().await.map_err(map_sqlx)?;

        if let Some(row) = sqlx::query(
            "SELECT * FROM jira_review_packages
             WHERE jira_site = ?1 AND jira_issue_id = ?2 AND idempotency_key = ?3",
        )
        .bind(&pkg.jira_site)
        .bind(&pkg.jira_issue_id)
        .bind(&pkg.idempotency_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx)?
        {
            tx.commit().await.map_err(map_sqlx)?;
            return issue_review_package_from_row(&row);
        }

        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(attempt), 0) + 1 FROM jira_review_packages
             WHERE jira_site = ?1 AND jira_issue_id = ?2",
        )
        .bind(&pkg.jira_site)
        .bind(&pkg.jira_issue_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        // Supersede every prior Pending aggregate for this issue.
        sqlx::query(
            "UPDATE jira_review_packages SET status = 'superseded'
             WHERE jira_site = ?1 AND jira_issue_id = ?2 AND status = 'pending'",
        )
        .bind(&pkg.jira_site)
        .bind(&pkg.jira_issue_id)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        let mut stored = pkg.clone();
        stored.attempt = next as u32;
        let status = stored.status.as_str().to_string();
        let payload =
            serde_json::to_string(&stored).map_err(|e| StoreError::BadData(e.to_string()))?;

        let res = sqlx::query(
            "INSERT INTO jira_review_packages
                (jira_site, jira_issue_id, attempt, idempotency_key, status, payload, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(&stored.jira_site)
        .bind(&stored.jira_issue_id)
        .bind(next)
        .bind(&stored.idempotency_key)
        .bind(&status)
        .bind(&payload)
        .bind(now)
        .execute(&mut *tx)
        .await;

        match res {
            Ok(_) => {
                tx.commit().await.map_err(map_sqlx)?;
                Ok(stored)
            }
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                tx.rollback().await.ok();
                let row = sqlx::query(
                    "SELECT * FROM jira_review_packages
                     WHERE jira_site = ?1 AND jira_issue_id = ?2 AND idempotency_key = ?3",
                )
                .bind(&pkg.jira_site)
                .bind(&pkg.jira_issue_id)
                .bind(&pkg.idempotency_key)
                .fetch_one(&self.pool)
                .await
                .map_err(map_sqlx)?;
                issue_review_package_from_row(&row)
            }
            Err(e) => {
                tx.rollback().await.ok();
                Err(map_sqlx(e))
            }
        }
    }

    async fn latest_issue_review_package(
        &self,
        jira_site: &str,
        jira_issue_id: &str,
    ) -> Result<Option<IssueReviewPackage>> {
        let row = sqlx::query(
            "SELECT * FROM jira_review_packages
             WHERE jira_site = ?1 AND jira_issue_id = ?2 ORDER BY attempt DESC LIMIT 1",
        )
        .bind(jira_site)
        .bind(jira_issue_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        match row {
            Some(r) => Ok(Some(issue_review_package_from_row(&r)?)),
            None => Ok(None),
        }
    }

    async fn list_issue_review_packages(
        &self,
        jira_site: &str,
        jira_issue_id: &str,
    ) -> Result<Vec<IssueReviewPackage>> {
        let rows = sqlx::query(
            "SELECT * FROM jira_review_packages
             WHERE jira_site = ?1 AND jira_issue_id = ?2 ORDER BY attempt",
        )
        .bind(jira_site)
        .bind(jira_issue_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        rows.iter().map(issue_review_package_from_row).collect()
    }

    async fn all_issue_review_packages(&self) -> Result<Vec<IssueReviewPackage>> {
        let rows = sqlx::query(
            "SELECT * FROM jira_review_packages ORDER BY jira_site, jira_issue_id, attempt",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        rows.iter().map(issue_review_package_from_row).collect()
    }
}

/// True when `body`'s last non-empty line equals `line` (trailing newline
/// ignored). Shared dedup for the atomic `done` log append so a retry can't
/// stack a second `[done request]` line.
fn body_ends_with_line(body: &str, line: &str) -> bool {
    body.lines().last().map(str::trim_end) == Some(line.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadenza_proto::Decisao;
    use tempfile::TempDir;

    async fn mk() -> (TempDir, SqliteRepository) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cadenza.db");
        let repo = SqliteRepository::open(&path).await.unwrap();
        (dir, repo)
    }

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

    fn mk_args(key: &str, title: &str) -> NewProposta {
        NewProposta {
            idempotency_key: key.into(),
            parent: Some("T-1".into()),
            title: title.into(),
            repro: "...".into(),
            file: "src/foo.rs".into(),
            what_failed: "panic".into(),
            action: "fix bounds check".into(),
            jira_site: None,
            jira_issue_id: None,
        }
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
    async fn create_and_read_round_trip() {
        let (_d, repo) = mk().await;
        repo.create_task(&t("T-1", Estado::Fazendo)).await.unwrap();
        let got = repo.read_task("T-1").await.unwrap();
        assert_eq!(got.titulo, "T-1 title");
        assert_eq!(got.estado, Estado::Fazendo);
        assert_eq!(got.responsavel, "humano");
        assert_eq!(got.body, "body of T-1");
    }

    #[tokio::test]
    async fn list_filters_by_estado() {
        let (_d, repo) = mk().await;
        repo.create_task(&t("A", Estado::AFazer)).await.unwrap();
        repo.create_task(&t("B", Estado::Fazendo)).await.unwrap();
        repo.create_task(&t("C", Estado::Fazendo)).await.unwrap();
        repo.create_task(&t("D", Estado::Feito)).await.unwrap();
        assert_eq!(
            repo.list_tasks(Some(Estado::Fazendo)).await.unwrap().len(),
            2
        );
        assert_eq!(repo.list_tasks(None).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn set_estado_preserves_other_fields() {
        let (_d, repo) = mk().await;
        repo.create_task(&t("X", Estado::AFazer)).await.unwrap();
        repo.set_estado("X", Estado::Fazendo).await.unwrap();
        let got = repo.read_task("X").await.unwrap();
        assert_eq!(got.estado, Estado::Fazendo);
        assert_eq!(got.titulo, "X title");
        assert_eq!(got.body, "body of X");
    }

    #[tokio::test]
    async fn append_log_extends_body() {
        let (_d, repo) = mk().await;
        repo.create_task(&t("Y", Estado::Fazendo)).await.unwrap();
        repo.append_log("Y", "first log line").await.unwrap();
        repo.append_log("Y", "second").await.unwrap();
        let got = repo.read_task("Y").await.unwrap();
        assert!(got.body.contains("first log line"));
        assert!(got.body.contains("second"));
        assert!(got.body.ends_with('\n'));
    }

    #[tokio::test]
    async fn create_duplicate_errors_not_found_after_delete() {
        let (_d, repo) = mk().await;
        repo.create_task(&t("D", Estado::AFazer)).await.unwrap();
        assert!(matches!(
            repo.create_task(&t("D", Estado::Fazendo)).await,
            Err(StoreError::AlreadyExists(_))
        ));
        repo.delete_task("D").await.unwrap();
        assert!(matches!(
            repo.read_task("D").await,
            Err(StoreError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn propose_dedup_on_idempotency_key() {
        let (_d, repo) = mk().await;
        let p1 = repo.propose(mk_args("k1", "first")).await.unwrap();
        let p2 = repo.propose(mk_args("k1", "different")).await.unwrap();
        assert_eq!(p1.proposta_id, p2.proposta_id);
        assert_eq!(p2.title, "first"); // original wins
    }

    #[tokio::test]
    async fn list_pending_excludes_decided() {
        let (_d, repo) = mk().await;
        let p1 = repo.propose(mk_args("k1", "one")).await.unwrap();
        let _p2 = repo.propose(mk_args("k2", "two")).await.unwrap();
        repo.write_decisao(DecisaoRegistro {
            proposta_id: p1.proposta_id.clone(),
            decisao: Decisao::Aceita,
            task_id: None,
            autor: "h".into(),
            decided_at_ms: 0,
        })
        .await
        .unwrap();
        let pending = repo.list_pending_propostas().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_ne!(pending[0].proposta_id, p1.proposta_id);
    }

    #[tokio::test]
    async fn await_decisao_wakes_on_write() {
        let (_d, repo) = mk().await;
        let repo = Arc::new(repo);
        let p = repo.propose(mk_args("k", "x")).await.unwrap();

        let writer = repo.clone();
        let pid = p.proposta_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            writer
                .write_decisao(DecisaoRegistro {
                    proposta_id: pid,
                    decisao: Decisao::Mesclada,
                    task_id: Some("T-77".into()),
                    autor: "h".into(),
                    decided_at_ms: 0,
                })
                .await
                .unwrap();
        });

        let got = repo
            .await_decisao(&p.proposta_id, Duration::from_secs(2))
            .await
            .unwrap();
        let d = got.expect("waiter should have been notified");
        assert_eq!(d.decisao, Decisao::Mesclada);
        assert_eq!(d.task_id.as_deref(), Some("T-77"));
    }

    #[tokio::test]
    async fn await_decisao_times_out() {
        let (_d, repo) = mk().await;
        let p = repo.propose(mk_args("k", "x")).await.unwrap();
        let got = repo
            .await_decisao(&p.proposta_id, Duration::from_millis(50))
            .await
            .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn set_titulo_preserves_other_fields() {
        let (_d, repo) = mk().await;
        repo.create_task(&t("Z", Estado::AFazer)).await.unwrap();
        repo.set_titulo("Z", "new title").await.unwrap();
        let got = repo.read_task("Z").await.unwrap();
        assert_eq!(got.titulo, "new title");
        assert_eq!(got.estado, Estado::AFazer);
        assert_eq!(got.body, "body of Z");
    }

    #[tokio::test]
    async fn update_task_body_replaces_body() {
        let (_d, repo) = mk().await;
        repo.create_task(&t("B", Estado::Fazendo)).await.unwrap();
        repo.update_task_body("B", "replaced body").await.unwrap();
        let got = repo.read_task("B").await.unwrap();
        assert_eq!(got.body, "replaced body");
        assert_eq!(got.titulo, "B title");
        assert_eq!(got.estado, Estado::Fazendo);
    }

    #[tokio::test]
    async fn delete_task_makes_it_not_found() {
        let (_d, repo) = mk().await;
        repo.create_task(&t("R", Estado::AFazer)).await.unwrap();
        repo.delete_task("R").await.unwrap();
        assert!(matches!(
            repo.read_task("R").await,
            Err(StoreError::NotFound(_))
        ));
    }

    fn mem_item(id: &str, texto: &str) -> MemoryItem {
        MemoryItem {
            id: id.into(),
            texto: texto.into(),
            origem_task: None,
            criado_em: 1,
        }
    }

    #[tokio::test]
    async fn memory_item_crud_round_trip() {
        let (_d, repo) = mk().await;
        assert!(repo.list_memory("p1").await.unwrap().is_empty());
        repo.add_memory_item("p1", &mem_item("M-1", "fato"))
            .await
            .unwrap();
        repo.add_memory_item("p2", &mem_item("M-2", "outro"))
            .await
            .unwrap();
        // Scoped per project.
        assert_eq!(repo.list_memory("p1").await.unwrap().len(), 1);
        assert_eq!(repo.list_memory("p2").await.unwrap().len(), 1);
        repo.update_memory_item("p1", "M-1", "novo").await.unwrap();
        assert_eq!(repo.list_memory("p1").await.unwrap()[0].texto, "novo");
        // Wrong project can't touch the item.
        assert!(matches!(
            repo.update_memory_item("p2", "M-1", "x").await,
            Err(StoreError::NotFound(_))
        ));
        repo.delete_memory_item("p1", "M-1").await.unwrap();
        assert!(repo.list_memory("p1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn memory_suggestion_round_trip_preserves_kind() {
        let (_d, repo) = mk().await;
        let sug = MemorySuggestion {
            id: "MS-1".into(),
            project_id: "p1".into(),
            criado_em: 7,
            kind: SuggestionKind::Mesclar {
                target_ids: vec!["M-a".into(), "M-b".into()],
                texto_mesclado: "fundido".into(),
            },
        };
        repo.create_memory_suggestion(&sug).await.unwrap();
        let got = repo.read_memory_suggestion("MS-1").await.unwrap().unwrap();
        assert_eq!(got.kind, sug.kind);
        assert_eq!(repo.list_memory_suggestions("p1").await.unwrap().len(), 1);
        assert_eq!(repo.list_memory_suggestions("p2").await.unwrap().len(), 0);
        repo.delete_memory_suggestion("MS-1").await.unwrap();
        assert!(repo.read_memory_suggestion("MS-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sqlite_task_roundtrip_carries_jira_identity() {
        let (_d, repo) = mk().await;
        let mut task = t("T-1", Estado::AFazer);
        task.jira_site = Some("https://x.atlassian.net".into());
        task.jira_issue_id = Some("10001".into());
        repo.create_task(&task).await.unwrap();
        let got = repo.read_task("T-1").await.unwrap();
        assert_eq!(got.jira_site.as_deref(), Some("https://x.atlassian.net"));
        assert_eq!(got.jira_issue_id.as_deref(), Some("10001"));
        // jira_key_display is never read from the row.
        assert!(got.jira_key_display.is_none());
    }

    #[tokio::test]
    async fn sqlite_jira_issue_upsert_read_delete() {
        let (_d, repo) = mk().await;
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
        // Upsert again with a changed key reflects on read.
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
    async fn sqlite_migrate_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cadenza.db");
        // Opening twice re-runs the migrator; the sqlx ledger no-ops.
        let _r1 = SqliteRepository::open(&path).await.unwrap();
        let _r2 = SqliteRepository::open(&path).await.unwrap();
    }

    // ─── aggregate (issue-owned) review packages (Slice 5) ─────────

    fn mk_issue_pkg(site: &str, issue: &str, key: &str) -> IssueReviewPackage {
        use crate::review::issue::IssuePackageStatus;
        IssueReviewPackage {
            jira_site: site.into(),
            jira_issue_id: issue.into(),
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
        }
    }

    #[tokio::test]
    async fn sqlite_issue_review_roundtrip() {
        use crate::review::issue::IssuePackageStatus;
        let (_d, repo) = mk().await;
        repo.upsert_jira_issue(&mk_jira("site", "10001", "PROJ-1"))
            .await
            .unwrap();
        let stored = repo
            .upsert_issue_review_package(&mk_issue_pkg("site", "10001", "k1"))
            .await
            .unwrap();
        assert_eq!(stored.attempt, 1);
        assert_eq!(stored.status, IssuePackageStatus::Pending);
        let latest = repo
            .latest_issue_review_package("site", "10001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest, stored);
        let list = repo
            .list_issue_review_packages("site", "10001")
            .await
            .unwrap();
        assert_eq!(list, vec![stored]);
    }

    #[tokio::test]
    async fn sqlite_issue_review_supersede() {
        use crate::review::issue::IssuePackageStatus;
        let (_d, repo) = mk().await;
        repo.upsert_jira_issue(&mk_jira("site", "10001", "PROJ-1"))
            .await
            .unwrap();
        repo.upsert_issue_review_package(&mk_issue_pkg("site", "10001", "k1"))
            .await
            .unwrap();
        let second = repo
            .upsert_issue_review_package(&mk_issue_pkg("site", "10001", "k2"))
            .await
            .unwrap();
        assert_eq!(second.attempt, 2);
        let list = repo
            .list_issue_review_packages("site", "10001")
            .await
            .unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].status, IssuePackageStatus::Superseded);
        assert_eq!(list[1].status, IssuePackageStatus::Pending);
    }

    #[tokio::test]
    async fn sqlite_issue_review_dedup() {
        let (_d, repo) = mk().await;
        repo.upsert_jira_issue(&mk_jira("site", "10001", "PROJ-1"))
            .await
            .unwrap();
        let a = repo
            .upsert_issue_review_package(&mk_issue_pkg("site", "10001", "same"))
            .await
            .unwrap();
        let b = repo
            .upsert_issue_review_package(&mk_issue_pkg("site", "10001", "same"))
            .await
            .unwrap();
        assert_eq!(a.attempt, b.attempt);
        assert_eq!(
            repo.list_issue_review_packages("site", "10001")
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn sqlite_issue_review_no_collision_with_task_package() {
        // A task package with task_id == "10001" and an aggregate with
        // jira_issue_id == "10001" live in separate tables; neither leaks.
        let (_d, repo) = mk().await;
        repo.create_task(&t("10001", Estado::Fazendo))
            .await
            .unwrap();
        repo.upsert_jira_issue(&mk_jira("site", "10001", "PROJ-1"))
            .await
            .unwrap();

        let mut tp = mk_issue_pkg("site", "10001", "agg");
        tp.idempotency_key = "agg".into();
        repo.upsert_issue_review_package(&tp).await.unwrap();

        // Build a task ReviewPackage for "10001".
        let task_pkg = ReviewPackage {
            task_id: "10001".into(),
            attempt: 0,
            idempotency_key: "taskk".into(),
            status: PackageStatus::Pending,
            checks: vec![],
            groups: vec![],
            open_questions: vec![],
            summary: "task".into(),
            changed_files: vec![],
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
            risks: vec![],
            secret_matches: vec![],
            evidence_state: crate::review::EvidenceState::NoValidation,
            needs_focused_human_review: false,
            validation_scope_unknown: false,
            base_sha: None,
            head_sha: None,
            worktree_fingerprint: None,
            contract_version: None,
            reported_contract_version: None,
            risk_heuristic_version: crate::review::RISK_HEURISTIC_VERSION,
            created_at_ms: 1,
            collection_duration_ms: 0,
            collection_errors: vec![],
            truncated: false,
            uncommitted_patch: None,
        };
        repo.upsert_review_package(&task_pkg).await.unwrap();

        let task_back = repo.latest_review_package("10001").await.unwrap().unwrap();
        assert_eq!(task_back.idempotency_key, "taskk");
        let agg_back = repo
            .latest_issue_review_package("site", "10001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(agg_back.idempotency_key, "agg");
    }
}
