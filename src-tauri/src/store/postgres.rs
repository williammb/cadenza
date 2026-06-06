//! PostgreSQL-backed `Repository` impl (Fase C).
//!
//! Parallel of `sqlite.rs`. Postgres uses `$N` parameter placeholders
//! (not SQLite's `?N`), so the SQL is duplicated rather than shared.
//!
//! Targets: Supabase, AWS RDS, Azure Database for PostgreSQL. The
//! password is loaded from the OS keyring at connect time — it never
//! lives on disk in cleartext (see `keyring_util.rs`).
//!
//! Schema migrations live in `migrations-pg/` (Postgres dialect). The
//! sqlite/postgres pools each have their own `_sqlx_migrations` table.

use async_trait::async_trait;
use cadenza_proto::Decisao;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

use super::{
    DecisaoRegistro, Estado, Ideia, IdeiaStatus, IssueReviewPackage, JiraIssueRecord, MemoryItem,
    MemorySuggestion, NewProposta, PackageStatus, Proposta, Repository, Result, ReviewPackage,
    StoreError, SuggestionKind, Task,
};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations-pg");

#[derive(Debug, Clone)]
pub struct PgConnectionParams {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    /// Password loaded from the OS keyring by the caller. Never read
    /// from disk; never serialized back to config.json.
    pub password: String,
    pub ssl_mode: PgSslModeChoice,
}

/// User-facing SSL mode that maps onto sqlx's `PgSslMode`. Kept as a
/// separate enum so the config layer doesn't depend on sqlx types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgSslModeChoice {
    Disable,
    Prefer,
    Require,
}

impl PgSslModeChoice {
    fn to_sqlx(self) -> PgSslMode {
        match self {
            PgSslModeChoice::Disable => PgSslMode::Disable,
            PgSslModeChoice::Prefer => PgSslMode::Prefer,
            PgSslModeChoice::Require => PgSslMode::Require,
        }
    }
}

pub struct PgRepository {
    pool: PgPool,
    waiters: Mutex<HashMap<String, Arc<Notify>>>,
}

impl PgRepository {
    /// Open the connection pool and run pending migrations. Caller is
    /// responsible for having loaded the password from the keyring;
    /// we never touch the keyring directly here so this module can be
    /// unit-tested against a throwaway Postgres without keyring setup.
    pub async fn open(params: &PgConnectionParams) -> Result<Self> {
        let opts = PgConnectOptions::new()
            .host(&params.host)
            .port(params.port)
            .database(&params.database)
            .username(&params.user)
            .password(&params.password)
            .ssl_mode(params.ssl_mode.to_sqlx());

        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(opts)
            .await
            .map_err(|e| StoreError::Other(format!("postgres pool: {e}")))?;

        MIGRATOR
            .run(&pool)
            .await
            .map_err(|e| StoreError::Other(format!("postgres migrate: {e}")))?;

        Ok(Self {
            pool,
            waiters: Mutex::new(HashMap::new()),
        })
    }

    /// Best-effort liveness check: opens a pool, runs `SELECT 1`,
    /// and tears it down. Used by the `test_db_connection` Tauri
    /// command before the user commits to switching backends.
    pub async fn ping(params: &PgConnectionParams) -> Result<()> {
        let opts = PgConnectOptions::new()
            .host(&params.host)
            .port(params.port)
            .database(&params.database)
            .username(&params.user)
            .password(&params.password)
            .ssl_mode(params.ssl_mode.to_sqlx());
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(opts)
            .await
            .map_err(|e| StoreError::Other(format!("postgres connect: {e}")))?;
        sqlx::query("SELECT 1")
            .execute(&pool)
            .await
            .map_err(|e| StoreError::Other(format!("postgres ping: {e}")))?;
        pool.close().await;
        Ok(())
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

fn task_from_row(row: &sqlx::postgres::PgRow) -> Result<Task> {
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

fn jira_issue_from_row(row: &sqlx::postgres::PgRow) -> Result<JiraIssueRecord> {
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

fn proposta_from_row(row: &sqlx::postgres::PgRow) -> Result<Proposta> {
    Ok(Proposta {
        proposta_id: row.try_get("proposta_id").map_err(map_sqlx)?,
        idempotency_key: row.try_get("idempotency_key").map_err(map_sqlx)?,
        parent: row.try_get("parent").map_err(map_sqlx)?,
        title: row.try_get("title").map_err(map_sqlx)?,
        repro: row.try_get("repro").map_err(map_sqlx)?,
        file: row.try_get("file").map_err(map_sqlx)?,
        what_failed: row.try_get("what_failed").map_err(map_sqlx)?,
        action: row.try_get("action").map_err(map_sqlx)?,
        jira_site: row.try_get("jira_site").map_err(map_sqlx)?,
        jira_issue_id: row.try_get("jira_issue_id").map_err(map_sqlx)?,
        // Display key is enriched on read from the JiraIssueRecord, not stored.
        jira_key_display: None,
        created_at_ms: row.try_get("created_at_ms").map_err(map_sqlx)?,
    })
}

fn decisao_from_row(row: &sqlx::postgres::PgRow) -> Result<DecisaoRegistro> {
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

fn ideia_from_row(row: &sqlx::postgres::PgRow) -> Result<Ideia> {
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

fn memory_item_from_row(row: &sqlx::postgres::PgRow) -> Result<MemoryItem> {
    Ok(MemoryItem {
        id: row.try_get("id").map_err(map_sqlx)?,
        texto: row.try_get("texto").map_err(map_sqlx)?,
        origem_task: row.try_get("origem_task").map_err(map_sqlx)?,
        criado_em: row.try_get("criado_em").map_err(map_sqlx)?,
    })
}

fn memory_suggestion_from_row(row: &sqlx::postgres::PgRow) -> Result<MemorySuggestion> {
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

/// See `sqlite::package_status_as_str` — identical canonical mapping.
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

/// Reconstruct the package from its JSONB `payload`, overlaying the
/// authoritative `status`/`attempt` columns (see the SQLite twin).
fn review_package_from_row(row: &sqlx::postgres::PgRow) -> Result<ReviewPackage> {
    let payload: serde_json::Value = row.try_get("payload").map_err(map_sqlx)?;
    let mut pkg: ReviewPackage = serde_json::from_value(payload)
        .map_err(|e| StoreError::BadData(format!("bad review package payload: {e}")))?;
    let status: String = row.try_get("status").map_err(map_sqlx)?;
    pkg.status = package_status_parse(&status)?;
    let attempt: i64 = row.try_get("attempt").map_err(map_sqlx)?;
    pkg.attempt = attempt as u32;
    Ok(pkg)
}

/// Reconstruct the aggregate package from its JSONB `payload`, overlaying the
/// authoritative `status`/`attempt` columns (the SQLite twin reads a TEXT
/// column). The supersede UPDATE only touches the column, not the blob.
fn issue_review_package_from_row(row: &sqlx::postgres::PgRow) -> Result<IssueReviewPackage> {
    let payload: serde_json::Value = row.try_get("payload").map_err(map_sqlx)?;
    let mut pkg: IssueReviewPackage = serde_json::from_value(payload)
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
impl Repository for PgRepository {
    async fn list_tasks(&self, filter: Option<Estado>) -> Result<Vec<Task>> {
        let rows = match filter {
            Some(e) => {
                sqlx::query("SELECT * FROM tasks WHERE estado = $1 ORDER BY id")
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
        let row = sqlx::query("SELECT * FROM tasks WHERE id = $1")
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
             VALUES ($1, $2, $3, $4, $5, $6, $6, $7, $8)",
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
        let res = sqlx::query("UPDATE tasks SET estado = $1, updated_at_ms = $2 WHERE id = $3")
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
        let res = sqlx::query("UPDATE tasks SET titulo = $1, updated_at_ms = $2 WHERE id = $3")
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
        let res = sqlx::query("UPDATE tasks SET body = $1, updated_at_ms = $2 WHERE id = $3")
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
        let res = sqlx::query("DELETE FROM tasks WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn append_log(&self, id: &str, text: &str) -> Result<()> {
        // Postgres can do this in a single statement with `||`. The
        // trailing-newline behavior matches the file + sqlite backends:
        // append a `\n` only when the caller's text doesn't end in one.
        let needs_newline = !text.ends_with('\n');
        let suffix = if needs_newline {
            let mut s = text.to_string();
            s.push('\n');
            s
        } else {
            text.to_string()
        };
        let res =
            sqlx::query("UPDATE tasks SET body = body || $1, updated_at_ms = $2 WHERE id = $3")
                .bind(suffix)
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

    async fn propose(&self, args: NewProposta) -> Result<Proposta> {
        if let Some(row) = sqlx::query("SELECT * FROM propostas WHERE idempotency_key = $1")
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
                what_failed, action, created_at_ms, jira_site, jira_issue_id
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
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
        .bind(&proposta.jira_site)
        .bind(&proposta.jira_issue_id)
        .execute(&self.pool)
        .await;

        match res {
            Ok(_) => Ok(proposta),
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                let row = sqlx::query("SELECT * FROM propostas WHERE idempotency_key = $1")
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
        let row = sqlx::query("SELECT * FROM propostas WHERE proposta_id = $1")
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
        let row = sqlx::query("SELECT * FROM decisoes WHERE proposta_id = $1")
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
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (proposta_id) DO UPDATE SET
                decisao = EXCLUDED.decisao,
                task_id = EXCLUDED.task_id,
                autor = EXCLUDED.autor,
                decided_at_ms = EXCLUDED.decided_at_ms",
        )
        .bind(&registro.proposta_id)
        .bind(decisao_as_str(registro.decisao.clone()))
        .bind(&registro.task_id)
        .bind(&registro.autor)
        .bind(registro.decided_at_ms)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;

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
        if let Some(d) = self.read_decisao(proposta_id).await? {
            return Ok(Some(d));
        }
        let notify = {
            let mut waiters = self.waiters.lock().await;
            waiters
                .entry(proposta_id.to_string())
                .or_insert_with(|| Arc::new(Notify::new()))
                .clone()
        };
        // Arm the Notified future before the second disk check — see
        // sqlite.rs::await_decisao for the missed-wakeup rationale.
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
        let row = sqlx::query("SELECT * FROM ideias WHERE id = $1")
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
             VALUES ($1, $2, $3, $4, $5, $6)",
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
        let res = sqlx::query("DELETE FROM ideias WHERE id = $1")
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
        let res = sqlx::query("UPDATE ideias SET status = $1 WHERE id = $2")
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
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
             ON CONFLICT (jira_site, jira_issue_id) DO UPDATE SET
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
            sqlx::query("SELECT * FROM jira_issues WHERE jira_site = $1 AND jira_issue_id = $2")
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
            sqlx::query("DELETE FROM jira_issues WHERE jira_site = $1 AND jira_issue_id = $2")
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
            sqlx::query("SELECT * FROM memory_items WHERE project_id = $1 ORDER BY criado_em, id")
                .bind(project_id)
                .fetch_all(&self.pool)
                .await
                .map_err(map_sqlx)?;
        rows.iter().map(memory_item_from_row).collect()
    }

    async fn add_memory_item(&self, project_id: &str, item: &MemoryItem) -> Result<()> {
        let res = sqlx::query(
            "INSERT INTO memory_items (id, project_id, texto, origem_task, criado_em)
             VALUES ($1, $2, $3, $4, $5)",
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
            sqlx::query("UPDATE memory_items SET texto = $1 WHERE id = $2 AND project_id = $3")
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
        let res = sqlx::query("DELETE FROM memory_items WHERE id = $1 AND project_id = $2")
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
            "SELECT * FROM memory_suggestions WHERE project_id = $1 ORDER BY criado_em, id",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        rows.iter().map(memory_suggestion_from_row).collect()
    }

    async fn read_memory_suggestion(&self, id: &str) -> Result<Option<MemorySuggestion>> {
        let row = sqlx::query("SELECT * FROM memory_suggestions WHERE id = $1")
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
             VALUES ($1, $2, $3, $4)",
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
        let res = sqlx::query("DELETE FROM memory_suggestions WHERE id = $1")
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
        let rows = sqlx::query("SELECT * FROM review_packages WHERE task_id = $1 ORDER BY attempt")
            .bind(task_id)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
        rows.iter().map(review_package_from_row).collect()
    }

    async fn latest_review_package(&self, task_id: &str) -> Result<Option<ReviewPackage>> {
        let row = sqlx::query(
            "SELECT * FROM review_packages WHERE task_id = $1 ORDER BY attempt DESC LIMIT 1",
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

    /// Single transaction mirroring the SQLite upsert (`$N` params, JSONB
    /// payload). Idempotent on `(task_id, idempotency_key)`.
    async fn upsert_review_package(&self, pkg: &ReviewPackage) -> Result<ReviewPackage> {
        super::validate_idempotency_key(&pkg.idempotency_key)?;
        let now = now_ms();
        let mut tx = self.pool.begin().await.map_err(map_sqlx)?;

        if let Some(row) =
            sqlx::query("SELECT * FROM review_packages WHERE task_id = $1 AND idempotency_key = $2")
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
            "SELECT COALESCE(MAX(attempt), 0) + 1 FROM review_packages WHERE task_id = $1",
        )
        .bind(&pkg.task_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        sqlx::query(
            "UPDATE review_packages SET status = 'superseded'
             WHERE task_id = $1 AND status = 'pending'",
        )
        .bind(&pkg.task_id)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        let mut stored = pkg.clone();
        stored.attempt = next as u32;
        stored.status = PackageStatus::Pending;
        let payload =
            serde_json::to_value(&stored).map_err(|e| StoreError::BadData(e.to_string()))?;

        let res = sqlx::query(
            "INSERT INTO review_packages
                (task_id, attempt, idempotency_key, status, payload, created_at_ms)
             VALUES ($1, $2, $3, 'pending', $4, $5)",
        )
        .bind(&stored.task_id)
        .bind(next)
        .bind(&stored.idempotency_key)
        .bind(payload)
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
                    "SELECT * FROM review_packages WHERE task_id = $1 AND idempotency_key = $2",
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
             WHERE task_id = $1 AND attempt <> $2 AND status = 'pending'",
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
            "UPDATE review_packages SET status = $1 WHERE task_id = $2 AND attempt = $3",
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
        sqlx::query("DELETE FROM review_packages WHERE task_id = $1")
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

        if let Some(row) =
            sqlx::query("SELECT * FROM review_packages WHERE task_id = $1 AND idempotency_key = $2")
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
            "SELECT COALESCE(MAX(attempt), 0) + 1 FROM review_packages WHERE task_id = $1",
        )
        .bind(&pkg.task_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        sqlx::query(
            "UPDATE review_packages SET status = 'superseded'
             WHERE task_id = $1 AND status = 'pending'",
        )
        .bind(&pkg.task_id)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        let mut stored = pkg.clone();
        stored.attempt = next as u32;
        stored.status = PackageStatus::Pending;
        let payload =
            serde_json::to_value(&stored).map_err(|e| StoreError::BadData(e.to_string()))?;

        sqlx::query(
            "INSERT INTO review_packages
                (task_id, attempt, idempotency_key, status, payload, created_at_ms)
             VALUES ($1, $2, $3, 'pending', $4, $5)",
        )
        .bind(&stored.task_id)
        .bind(next)
        .bind(&stored.idempotency_key)
        .bind(payload)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        let body_row = sqlx::query("SELECT body FROM tasks WHERE id = $1")
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
        sqlx::query("UPDATE tasks SET body = $1, estado = $2, updated_at_ms = $3 WHERE id = $4")
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
             WHERE jira_site = $1 AND jira_issue_id = $2 AND idempotency_key = $3",
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
             WHERE jira_site = $1 AND jira_issue_id = $2",
        )
        .bind(&pkg.jira_site)
        .bind(&pkg.jira_issue_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx)?;

        sqlx::query(
            "UPDATE jira_review_packages SET status = 'superseded'
             WHERE jira_site = $1 AND jira_issue_id = $2 AND status = 'pending'",
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
            serde_json::to_value(&stored).map_err(|e| StoreError::BadData(e.to_string()))?;

        let res = sqlx::query(
            "INSERT INTO jira_review_packages
                (jira_site, jira_issue_id, attempt, idempotency_key, status, payload, created_at_ms)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&stored.jira_site)
        .bind(&stored.jira_issue_id)
        .bind(next)
        .bind(&stored.idempotency_key)
        .bind(&status)
        .bind(payload)
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
                     WHERE jira_site = $1 AND jira_issue_id = $2 AND idempotency_key = $3",
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
             WHERE jira_site = $1 AND jira_issue_id = $2 ORDER BY attempt DESC LIMIT 1",
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
             WHERE jira_site = $1 AND jira_issue_id = $2 ORDER BY attempt",
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
/// ignored). Shared dedup for the atomic `done` log append.
fn body_ends_with_line(body: &str, line: &str) -> bool {
    body.lines().last().map(str::trim_end) == Some(line.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only glue: parse a `postgres://user:pass@host:port/db` DSN into
    /// `PgConnectionParams`. Net-new and test-scoped (the production path
    /// builds params from config + keyring, never a URL).
    fn params_from_database_url(url: &str) -> PgConnectionParams {
        // postgres://user:pass@host:port/db
        let rest = url
            .strip_prefix("postgres://")
            .or_else(|| url.strip_prefix("postgresql://"))
            .expect("DATABASE_URL must start with postgres://");
        let (creds, host_db) = rest.split_once('@').expect("missing @ in DATABASE_URL");
        let (user, password) = creds.split_once(':').unwrap_or((creds, ""));
        let (hostport, database) = host_db
            .split_once('/')
            .expect("missing /db in DATABASE_URL");
        // Strip any ?query suffix on the database segment.
        let database = database.split('?').next().unwrap_or(database);
        let (host, port) = match hostport.split_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().unwrap_or(5432)),
            None => (hostport.to_string(), 5432),
        };
        PgConnectionParams {
            host,
            port,
            database: database.to_string(),
            user: user.to_string(),
            password: password.to_string(),
            ssl_mode: PgSslModeChoice::Prefer,
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

    #[ignore]
    #[tokio::test]
    async fn pg_task_roundtrip_carries_jira_identity() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let repo = PgRepository::open(&params_from_database_url(&url))
            .await
            .unwrap();
        let id = format!("T-jira-{}", Uuid::new_v4().simple());
        let mut task = t(&id, Estado::AFazer);
        task.jira_site = Some("https://x.atlassian.net".into());
        task.jira_issue_id = Some("10001".into());
        repo.create_task(&task).await.unwrap();
        let got = repo.read_task(&id).await.unwrap();
        assert_eq!(got.jira_site.as_deref(), Some("https://x.atlassian.net"));
        assert_eq!(got.jira_issue_id.as_deref(), Some("10001"));
        // Cleanup.
        repo.delete_task(&id).await.unwrap();
    }

    #[ignore]
    #[tokio::test]
    async fn pg_jira_issue_upsert_read_delete() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let repo = PgRepository::open(&params_from_database_url(&url))
            .await
            .unwrap();
        let site = format!("site-{}", Uuid::new_v4().simple());
        repo.upsert_jira_issue(&mk_jira(&site, "1", "PROJ-1"))
            .await
            .unwrap();
        assert_eq!(
            repo.read_jira_issue(&site, "1")
                .await
                .unwrap()
                .unwrap()
                .jira_key,
            "PROJ-1"
        );
        repo.upsert_jira_issue(&mk_jira(&site, "1", "PROJ-2"))
            .await
            .unwrap();
        assert_eq!(
            repo.read_jira_issue(&site, "1")
                .await
                .unwrap()
                .unwrap()
                .jira_key,
            "PROJ-2"
        );
        repo.delete_jira_issue(&site, "1").await.unwrap();
        assert!(repo.read_jira_issue(&site, "1").await.unwrap().is_none());
        assert!(matches!(
            repo.delete_jira_issue(&site, "1").await,
            Err(StoreError::NotFound(_))
        ));
    }

    #[ignore]
    #[tokio::test]
    async fn pg_migrate_is_idempotent() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let params = params_from_database_url(&url);
        // Opening twice re-runs the migrator; the sqlx ledger no-ops.
        let _r1 = PgRepository::open(&params).await.unwrap();
        let _r2 = PgRepository::open(&params).await.unwrap();
    }
}
