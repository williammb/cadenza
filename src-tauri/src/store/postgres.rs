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
    DecisaoRegistro, Estado, Ideia, IdeiaStatus, NewProposta, Proposta, Repository, Result,
    StoreError, Task,
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
            "INSERT INTO tasks (id, titulo, estado, responsavel, body, created_at_ms, updated_at_ms)
             VALUES ($1, $2, $3, $4, $5, $6, $6)",
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
        let res = sqlx::query(
            "UPDATE tasks SET estado = $1, updated_at_ms = $2 WHERE id = $3",
        )
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
        let res = sqlx::query(
            "UPDATE tasks SET titulo = $1, updated_at_ms = $2 WHERE id = $3",
        )
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
        let res = sqlx::query(
            "UPDATE tasks SET body = $1, updated_at_ms = $2 WHERE id = $3",
        )
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
        let res = sqlx::query(
            "UPDATE tasks SET body = body || $1, updated_at_ms = $2 WHERE id = $3",
        )
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
            created_at_ms,
        };

        let res = sqlx::query(
            "INSERT INTO propostas (
                proposta_id, idempotency_key, parent, title, repro, file,
                what_failed, action, created_at_ms
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
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
}
