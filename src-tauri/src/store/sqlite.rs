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
    DecisaoRegistro, Estado, Ideia, IdeiaStatus, MemoryItem, MemorySuggestion, NewProposta,
    Proposta, Repository, Result, StoreError, SuggestionKind, Task,
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
            "INSERT INTO tasks (id, titulo, estado, responsavel, body, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        )
        .bind(&task.id)
        .bind(&task.titulo)
        .bind(task.estado.as_str())
        .bind(&task.responsavel)
        .bind(&task.body)
        .bind(now)
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
}
