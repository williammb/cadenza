//! Cross-backend repository contract / parity suite.
//!
//! The same assertion bodies run against every [`Repository`] backend so the
//! file, SQLite, and PostgreSQL implementations stay behaviourally identical
//! at the trait boundary. Each scenario is written ONCE as an `async fn` that
//! takes `&dyn Repository`; per-backend wrappers construct a fresh repo and
//! invoke it.
//!
//! Backend gating:
//!   - file + SQLite always run (temp dirs, no external services);
//!   - PostgreSQL is behind `#[ignore]` and only runs when `DATABASE_URL` is
//!     set, matching the existing PG tests in `postgres.rs` and the CI
//!     `postgres` job that runs `cargo test … -- --ignored`.

use super::{
    Estado, FileRepository, NewProposta, PgConnectionParams, PgRepository, PgSslModeChoice,
    Repository, SqliteRepository, Task,
};
use tempfile::TempDir;

// ─── fixtures ──────────────────────────────────────────────────────────

fn mk_task(id: &str, estado: Estado) -> Task {
    Task {
        id: id.into(),
        titulo: format!("{id} title"),
        estado,
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

fn mk_proposta(key: &str, title: &str) -> NewProposta {
    NewProposta {
        idempotency_key: key.into(),
        parent: None,
        title: title.into(),
        repro: "repro steps".into(),
        file: "src/lib.rs".into(),
        what_failed: "it broke".into(),
        action: "fix it".into(),
        jira_site: None,
        jira_issue_id: None,
    }
}

/// Parse a `postgres://user:pass@host:port/db` DSN into `PgConnectionParams`.
/// Test-only glue (the production path builds params from config + keyring).
fn pg_params_from_url(url: &str) -> PgConnectionParams {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .expect("DATABASE_URL must start with postgres://");
    let (creds, host_db) = rest.split_once('@').expect("missing @ in DATABASE_URL");
    let (user, password) = creds.split_once(':').unwrap_or((creds, ""));
    let (hostport, database) = host_db
        .split_once('/')
        .expect("missing /db in DATABASE_URL");
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

// ─── backend-generic scenarios ─────────────────────────────────────────
//
// Each takes a `prefix` so the PostgreSQL backend (a shared, persistent DB
// in CI) gets unique ids per run and the scenarios never collide with one
// another or with leftovers from a prior run. The file/SQLite backends get a
// fresh temp dir per call so the prefix is cosmetic there.

/// task create -> read -> list -> update round-trip.
async fn scenario_task_roundtrip(repo: &dyn Repository, prefix: &str) {
    let id = format!("{prefix}-rt");
    repo.create_task(&mk_task(&id, Estado::AFazer))
        .await
        .expect("create_task");

    // read returns what we created.
    let got = repo.read_task(&id).await.expect("read_task");
    assert_eq!(got.id, id, "[{prefix}] read id");
    assert_eq!(got.titulo, format!("{id} title"), "[{prefix}] read titulo");
    assert_eq!(got.estado, Estado::AFazer, "[{prefix}] read estado");

    // list contains it.
    let listed = repo.list_tasks(None).await.expect("list_tasks");
    assert!(
        listed.iter().any(|t| t.id == id),
        "[{prefix}] list must contain created task"
    );

    // update: estado, titulo, body — then re-read reflects all three.
    repo.set_estado(&id, Estado::Fazendo)
        .await
        .expect("set_estado");
    repo.set_titulo(&id, "renamed").await.expect("set_titulo");
    repo.update_task_body(&id, "new body\n")
        .await
        .expect("update_task_body");
    let after = repo.read_task(&id).await.expect("read after update");
    assert_eq!(after.estado, Estado::Fazendo, "[{prefix}] estado updated");
    assert_eq!(after.titulo, "renamed", "[{prefix}] titulo updated");
    assert_eq!(after.body, "new body\n", "[{prefix}] body updated");

    // filter by estado picks up the moved task and excludes the original state.
    let fazendo = repo
        .list_tasks(Some(Estado::Fazendo))
        .await
        .expect("list fazendo");
    assert!(
        fazendo.iter().any(|t| t.id == id),
        "[{prefix}] filtered list (fazendo) must contain task"
    );
    let afazer = repo
        .list_tasks(Some(Estado::AFazer))
        .await
        .expect("list afazer");
    assert!(
        !afazer.iter().any(|t| t.id == id),
        "[{prefix}] filtered list (a_fazer) must NOT contain moved task"
    );

    // cleanup so the shared PG database doesn't accumulate rows.
    repo.delete_task(&id).await.expect("delete_task");
}

/// Ordering stability of `list_tasks`: two consecutive listings return the
/// SAME order, and every created id is present exactly once. (The file
/// backend's order is `read_dir`-defined and OS-dependent, so the contract is
/// stability across calls + completeness, not a specific sort.)
async fn scenario_list_ordering_stable(repo: &dyn Repository, prefix: &str) {
    let ids: Vec<String> = (0..5).map(|i| format!("{prefix}-ord-{i}")).collect();
    // Insert in a deliberately non-sorted order.
    for i in [2usize, 0, 4, 1, 3] {
        repo.create_task(&mk_task(&ids[i], Estado::AFazer))
            .await
            .expect("create_task");
    }

    let first: Vec<String> = repo
        .list_tasks(None)
        .await
        .expect("list 1")
        .into_iter()
        .map(|t| t.id)
        .filter(|id| id.starts_with(&format!("{prefix}-ord-")))
        .collect();
    let second: Vec<String> = repo
        .list_tasks(None)
        .await
        .expect("list 2")
        .into_iter()
        .map(|t| t.id)
        .filter(|id| id.starts_with(&format!("{prefix}-ord-")))
        .collect();

    assert_eq!(
        first, second,
        "[{prefix}] list ordering must be stable across calls"
    );
    // Completeness: all five present, no duplicates.
    let mut sorted = first.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        ids.len(),
        "[{prefix}] every created task present exactly once"
    );
    for id in &ids {
        assert!(first.contains(id), "[{prefix}] missing id {id}");
    }

    for id in &ids {
        repo.delete_task(id).await.expect("delete_task");
    }
}

/// propose idempotency: proposing twice with the SAME idempotency key yields
/// the same `proposta_id` (same result) and creates NO duplicate pending row.
async fn scenario_propose_idempotent(repo: &dyn Repository, prefix: &str) {
    let key = format!("{prefix}-idem-key");

    let pending_before = repo
        .list_pending_propostas()
        .await
        .expect("list pending before")
        .len();

    let first = repo
        .propose(mk_proposta(&key, "first"))
        .await
        .expect("propose 1");
    let second = repo
        .propose(mk_proposta(&key, "second-title-should-be-ignored"))
        .await
        .expect("propose 2");

    // Same idempotency key => same proposal identity.
    assert_eq!(
        first.proposta_id, second.proposta_id,
        "[{prefix}] same idempotency key must return same proposta_id"
    );
    assert_eq!(
        first.idempotency_key, key,
        "[{prefix}] proposta carries the idempotency key"
    );
    // The first write wins; the second call is a no-op replay.
    assert_eq!(
        second.title, first.title,
        "[{prefix}] replay must return the stored proposal unchanged"
    );

    // read_proposta resolves to the same record.
    let read = repo
        .read_proposta(&first.proposta_id)
        .await
        .expect("read_proposta")
        .expect("proposta present");
    assert_eq!(read.proposta_id, first.proposta_id, "[{prefix}] read id");

    // No duplicate: exactly ONE new pending proposal with this id.
    let pending_after = repo
        .list_pending_propostas()
        .await
        .expect("list pending after");
    let count_this = pending_after
        .iter()
        .filter(|p| p.proposta_id == first.proposta_id)
        .count();
    assert_eq!(
        count_this, 1,
        "[{prefix}] idempotent propose must not create a duplicate pending row"
    );
    assert_eq!(
        pending_after.len(),
        pending_before + 1,
        "[{prefix}] exactly one pending proposal added across two propose calls"
    );
}

/// Run every scenario against one constructed backend, under a backend-unique
/// prefix so concurrent test threads sharing a PG database never collide.
async fn run_all_scenarios(repo: &dyn Repository, prefix: &str) {
    scenario_task_roundtrip(repo, &format!("{prefix}-trt")).await;
    scenario_list_ordering_stable(repo, &format!("{prefix}-ord")).await;
    scenario_propose_idempotent(repo, &format!("{prefix}-prop")).await;
}

// ─── file backend ──────────────────────────────────────────────────────

#[tokio::test]
async fn file_backend_contract() {
    let dir = TempDir::new().unwrap();
    let repo = FileRepository::new(dir.path()).unwrap();
    run_all_scenarios(&repo, "T-file").await;
}

// ─── sqlite backend ────────────────────────────────────────────────────

#[tokio::test]
async fn sqlite_backend_contract() {
    let dir = TempDir::new().unwrap();
    let repo = SqliteRepository::open(&dir.path().join("contract.db"))
        .await
        .unwrap();
    run_all_scenarios(&repo, "T-sqlite").await;
}

// ─── postgres backend (gated) ──────────────────────────────────────────

/// Same contract against PostgreSQL. Behind `#[ignore]` + `DATABASE_URL`,
/// matching the CI `postgres` job (`cargo test … -- --ignored`). The prefix
/// embeds a fresh UUID so reruns against the shared CI database never collide
/// with one another or with leftovers from a crashed prior run.
#[ignore]
#[tokio::test]
async fn pg_backend_contract() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let repo = PgRepository::open(&pg_params_from_url(&url)).await.unwrap();
    let prefix = format!("T-pg-{}", uuid::Uuid::new_v4().simple());
    run_all_scenarios(&repo, &prefix).await;
}
