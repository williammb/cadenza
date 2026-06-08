//! Jira domain command handlers and orchestration — split out of the
//! `commands` god-module. Pure relocation: the analysis-run capability layer
//! (Slice 2), the Jira data layer (Slice 3), the review aggregation (Slice 5),
//! and the import/discard lifecycle (Slice 6a). Re-exported via `commands`'s
//! `pub use jira::*;` so every existing `commands::jira_*` path still resolves
//! unchanged (Tauri `generate_handler!` in lib.rs and the IPC dispatch in
//! `ipc.rs` both reference these paths).

// Bring in the parent module's imports and shared helpers (AppState,
// to_str_err, send_initial_prompt, wait_for_codex_uuid, jira_config_snapshot's
// siblings, etc.). Parent-private items are visible to this child module.
use super::*;

// ─────────────────── jira analysis runs (Slice 2) ───────────────────

use crate::jira_run::{self, RunSecret, RunSecretError, VerifiedRun};
use cadenza_proto::{ops as proto_ops, SecretStatus};

/// Epoch-ms now. Local helper (no shared `now_ms` in this module).
fn now_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Mint an analysis run: generate `analysis_run_id` + capability secret,
/// then upsert the issue record with `secret_hash` + expiry + status=Active.
/// Returns the plaintext secret EXACTLY ONCE (the caller surfaces it to the
/// operator and drops it). The plaintext is never persisted.
///
/// Requires an existing `JiraIssueRecord` for `(jira_site, jira_issue_id)`
/// (so `jira_key` and friends are preserved); absence is an error. Later
/// slices that own the import path will seed the record first.
// Minted from tests in Slice 2 and from the production import orchestration
// (`jira_import_persist`) in Slice 6a.
pub(crate) async fn create_analysis_run(
    state: &AppState,
    jira_site: &str,
    jira_issue_id: &str,
    project_id: Option<&str>,
) -> Result<(String, RunSecret), String> {
    let mut record = state
        .repo
        .read_jira_issue(jira_site, jira_issue_id)
        .await
        .map_err(to_str_err)?
        .ok_or_else(|| format!("no jira issue record for {jira_site}/{jira_issue_id}"))?;

    let analysis_run_id = format!("run-{}", Uuid::new_v4().simple());
    let secret = jira_run::generate_secret();
    let now = now_ms_i64();

    record.analysis_run_id = Some(analysis_run_id.clone());
    record.secret_hash = Some(jira_run::hash_secret(secret.expose()));
    record.secret_expiry_ms = Some(now + jira_run::RUN_SECRET_TTL_MS);
    record.secret_status = Some(SecretStatus::Active.as_str().to_string());
    if let Some(pid) = project_id {
        record.project_id = Some(pid.to_string());
    }
    record.updated_at_ms = now;

    state
        .repo
        .upsert_jira_issue(&record)
        .await
        .map_err(to_str_err)?;

    Ok((analysis_run_id, secret))
}

/// Resolve `analysis_run_id` → record by scanning `list_jira_issues`
/// (no secondary index in Slice 2; acceptable at desktop scale), then
/// verify status Active + not expired + hash match (constant-time).
pub(crate) async fn verify_run_secret(
    state: &AppState,
    analysis_run_id: &str,
    presented_secret: &str,
) -> Result<VerifiedRun, RunSecretError> {
    let records = state
        .repo
        .list_jira_issues()
        .await
        .map_err(|_| RunSecretError::NotFound)?;
    let record = records
        .into_iter()
        .find(|r| r.analysis_run_id.as_deref() == Some(analysis_run_id))
        .ok_or(RunSecretError::NotFound)?;

    let stored_hash = record
        .secret_hash
        .as_deref()
        .ok_or(RunSecretError::NotFound)?;

    // Status gate first (revoked is a definitive no), then expiry, then hash.
    match record
        .secret_status
        .as_deref()
        .and_then(SecretStatus::parse)
    {
        Some(SecretStatus::Revoked) => return Err(RunSecretError::Revoked),
        Some(SecretStatus::Expired) => return Err(RunSecretError::Expired),
        _ => {}
    }
    if let Some(expiry) = record.secret_expiry_ms {
        if now_ms_i64() > expiry {
            return Err(RunSecretError::Expired);
        }
    }
    let presented_hash = jira_run::hash_secret(presented_secret);
    if !jira_run::secret_hash_eq(stored_hash, &presented_hash) {
        return Err(RunSecretError::Invalid);
    }
    Ok(VerifiedRun {
        jira_site: record.jira_site,
        jira_issue_id: record.jira_issue_id,
        project_id: record.project_id,
    })
}

/// Set `secret_status=Revoked` via upsert. Idempotent: no-op if the record
/// is already revoked (or absent).
pub(crate) async fn revoke_run_secret(
    state: &AppState,
    analysis_run_id: &str,
) -> Result<(), String> {
    let records = state.repo.list_jira_issues().await.map_err(to_str_err)?;
    let Some(mut record) = records
        .into_iter()
        .find(|r| r.analysis_run_id.as_deref() == Some(analysis_run_id))
    else {
        return Ok(());
    };
    if record.secret_status.as_deref() == Some(SecretStatus::Revoked.as_str()) {
        return Ok(());
    }
    record.secret_status = Some(SecretStatus::Revoked.as_str().to_string());
    record.updated_at_ms = now_ms_i64();
    state
        .repo
        .upsert_jira_issue(&record)
        .await
        .map_err(to_str_err)?;
    Ok(())
}

/// Failure surface for `jira_materialize_core`. Carries enough to map to the
/// right wire `ErrorBody.code` (IPC) or `String` (Tauri command).
#[derive(Debug)]
pub(crate) enum MaterializeError {
    Secret(RunSecretError),
    Decomposition(jira_run::DecompError),
    Internal(String),
}

impl MaterializeError {
    /// `(code, message)` for an `ErrorBody`.
    pub(crate) fn code_message(&self) -> (&'static str, String) {
        match self {
            MaterializeError::Secret(RunSecretError::NotFound)
            | MaterializeError::Secret(RunSecretError::Invalid) => (
                "run_secret_invalid",
                "analysis run secret is unknown or invalid".to_string(),
            ),
            MaterializeError::Secret(RunSecretError::Expired) => (
                "run_secret_expired",
                "analysis run secret has expired".to_string(),
            ),
            MaterializeError::Secret(RunSecretError::Revoked) => (
                "run_secret_revoked",
                "analysis run secret has been revoked".to_string(),
            ),
            MaterializeError::Decomposition(e) => ("invalid_decomposition", e.reason()),
            MaterializeError::Internal(m) => ("internal", m.clone()),
        }
    }
}

impl std::fmt::Display for MaterializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (code, msg) = self.code_message();
        write!(f, "[{code}] {msg}")
    }
}

/// Shared materialize logic behind both the IPC op and the Tauri command.
///
/// Verifies the capability secret, validates the decomposition, then creates
/// one proposal per subtask with the Jira identity stamped SERVER-SIDE from
/// the verified run (never from the wire args) and a deterministic,
/// app-owned idempotency key `"jira:<site>:<issue>:<run_id>:<index>"`. Re-running while the
/// secret is still active is idempotent (same keys ⇒ `propose` dedup). On
/// success the secret is revoked (best-effort; revoke failure is logged in
/// English, not fatal — the tasks already exist).
pub(crate) async fn jira_materialize_core(
    state: &AppState,
    args: &proto_ops::jira_materialize::Args,
) -> Result<proto_ops::jira_materialize::Result, MaterializeError> {
    // 1. Authorize. Do NOT log `args` — it carries the secret.
    let verified = verify_run_secret(state, &args.analysis_run_id, &args.run_secret)
        .await
        .map_err(MaterializeError::Secret)?;

    // 2. Validate payload.
    jira_run::validate_decomposition(&args.subtasks).map_err(MaterializeError::Decomposition)?;

    // 3. Create one proposal per subtask, identity stamped from `verified`.
    let mut created = Vec::with_capacity(args.subtasks.len());
    for (index, subtask) in args.subtasks.iter().enumerate() {
        // Scope the key to the analysis run, not just (site, issue, index):
        // re-running the SAME run dedups (same run_id + index), but a NEW run
        // for the same issue (e.g. after discard + re-import + re-analysis)
        // mints fresh proposals instead of colliding with the prior run's.
        let idempotency_key = format!(
            "jira:{}:{}:{}:{}",
            verified.jira_site, verified.jira_issue_id, args.analysis_run_id, index
        );
        let np = NewProposta {
            idempotency_key: idempotency_key.clone(),
            parent: None,
            title: subtask.title.clone(),
            repro: subtask.body.clone(),
            file: String::new(),
            what_failed: String::new(),
            action: String::new(),
            jira_site: Some(verified.jira_site.clone()),
            jira_issue_id: Some(verified.jira_issue_id.clone()),
        };
        let proposta = state
            .repo
            .propose(np)
            .await
            .map_err(|e| MaterializeError::Internal(e.to_string()))?;
        created.push(proto_ops::jira_materialize::MaterializedTask {
            proposta_id: proposta.proposta_id,
            idempotency_key,
            subtask_index: index as u32,
        });
    }

    // 4. Revoke the now-spent secret (best-effort).
    if let Err(e) = revoke_run_secret(state, &args.analysis_run_id).await {
        tracing::warn!(error = %e, "failed to revoke analysis run secret after materialize");
    }

    Ok(proto_ops::jira_materialize::Result {
        jira_site: verified.jira_site,
        jira_issue_id: verified.jira_issue_id,
        created,
    })
}

/// Tauri-command surface for `jira_materialize` (in-app/test parity with the
/// IPC op). Delegates to [`jira_materialize_core`].
#[tauri::command]
pub async fn jira_materialize(
    state: State<'_, Arc<AppState>>,
    args: proto_ops::jira_materialize::Args,
) -> Result<proto_ops::jira_materialize::Result, String> {
    jira_materialize_core(&state, &args)
        .await
        .map_err(|e| e.to_string())
}

// ───────────────────────── Jira data layer (Slice 3) ─────────────────────────

/// Clone `config.jira` out of the lock, dropping the guard before any
/// `.await` (we never hold the sync Mutex across an await — commands.rs
/// state-doc rule). Returns a `JiraError::Config` if Jira is not configured.
fn jira_config_snapshot(
    state: &AppState,
) -> Result<crate::config::JiraConfig, crate::jira::JiraError> {
    let cfg = state
        .config
        .lock()
        .map_err(|e| crate::jira::JiraError::Config(format!("config lock poisoned: {e}")))?;
    cfg.jira
        .clone()
        .ok_or_else(|| crate::jira::JiraError::Config("Jira is not configured".to_string()))
    // guard drops here, before the caller awaits
}

/// Shared `jira_test_connection` logic behind the Tauri command and IPC op.
/// Fetches `/myself`; returns data only (no persistence).
pub(crate) async fn jira_test_connection_core(
    state: &AppState,
) -> Result<proto_ops::jira_test_connection::Result, crate::jira::JiraError> {
    let cfg = jira_config_snapshot(state)?;
    let client = crate::jira::JiraClient::from_config(&cfg)?;
    let cancel = crate::jira::CancelToken::new();
    let me = client.test_connection(&cancel).await?;
    Ok(proto_ops::jira_test_connection::Result {
        account_id: me.account_id,
        display_name: me.display_name,
    })
}

/// Shared `jira_fetch_issue` logic. Fetches+parses one issue; returns data
/// only (does NOT persist a `JiraIssueRecord`).
pub(crate) async fn jira_fetch_issue_core(
    state: &AppState,
    args: &proto_ops::jira_fetch_issue::Args,
) -> Result<proto_ops::jira_fetch_issue::Result, crate::jira::JiraError> {
    let key = args.key.trim();
    if key.is_empty() {
        return Err(crate::jira::JiraError::Config(
            "issue key is required".to_string(),
        ));
    }
    let cfg = jira_config_snapshot(state)?;
    let client = crate::jira::JiraClient::from_config(&cfg)?;
    let cancel = crate::jira::CancelToken::new();
    let issue = client.fetch_issue(key, &cancel).await?;
    Ok(proto_ops::jira_fetch_issue::Result {
        jira_issue_id: issue.jira_issue_id,
        jira_key: issue.jira_key,
        summary: issue.summary,
        description_markdown: issue.description_markdown,
        raw_adf: issue.raw_adf,
    })
}

/// Shared `jira_list_assigned` logic. Lists the caller's open issues with a
/// page cap; returns data only.
pub(crate) async fn jira_list_assigned_core(
    state: &AppState,
) -> Result<proto_ops::jira_list_assigned::Result, crate::jira::JiraError> {
    let cfg = jira_config_snapshot(state)?;
    let client = crate::jira::JiraClient::from_config(&cfg)?;
    let cancel = crate::jira::CancelToken::new();
    let res = client.list_assigned(&cancel).await?;
    Ok(proto_ops::jira_list_assigned::Result {
        issues: res
            .issues
            .into_iter()
            .map(|i| proto_ops::jira_list_assigned::Issue {
                key: i.key,
                id: i.id,
                summary: i.summary,
            })
            .collect(),
        partial: res.partial,
    })
}

#[tauri::command]
pub async fn jira_test_connection(
    state: State<'_, Arc<AppState>>,
) -> Result<proto_ops::jira_test_connection::Result, String> {
    jira_test_connection_core(&state)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn jira_fetch_issue(
    state: State<'_, Arc<AppState>>,
    args: proto_ops::jira_fetch_issue::Args,
) -> Result<proto_ops::jira_fetch_issue::Result, String> {
    jira_fetch_issue_core(&state, &args)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn jira_list_assigned(
    state: State<'_, Arc<AppState>>,
) -> Result<proto_ops::jira_list_assigned::Result, String> {
    jira_list_assigned_core(&state)
        .await
        .map_err(|e| e.to_string())
}

// ───────────────────────── Jira import (Slice 6a) ─────────────────────────

/// Failure surface for the import orchestration. Carries enough to map to the
/// right wire `ErrorBody.code` (IPC) or `String` (Tauri command). The
/// capability secret NEVER appears in any variant.
#[derive(Debug)]
pub(crate) enum ImportError {
    /// Bad usage / misconfiguration (empty issue_ref, bad analyst_kind, Jira
    /// not configured). Maps to `jira_config` (exit 2).
    Config(String),
    /// The target project id is not in config.projects. Maps to
    /// `unknown_project` (exit 30).
    UnknownProject(String),
    /// The fetch leg failed; passthrough of the Jira data-layer error so its
    /// own stable code (`jira_auth`/`jira_not_found`/`jira_http`/…) is kept.
    Fetch(crate::jira::JiraError),
    /// Minting the analysis run / persisting the seed record failed. Maps to
    /// `jira_import_failed` (exit 1).
    Mint(String),
    /// The analyst PTY spawn failed. Maps to `jira_import_failed` (exit 1).
    Spawn(String),
    /// Any other store/internal failure. Maps to `jira_import_failed` (exit 1).
    Internal(String),
}

impl ImportError {
    /// `(wire code, message)` for the IPC `ErrorBody`.
    pub(crate) fn code_message(&self) -> (&'static str, String) {
        match self {
            ImportError::Config(m) => ("jira_config", m.clone()),
            ImportError::UnknownProject(p) => {
                ("unknown_project", format!("unknown project_id: {p}"))
            }
            // Preserve the fetch error's own stable code/message.
            ImportError::Fetch(e) => {
                let (code, msg) = e.code_message();
                (code, msg)
            }
            ImportError::Mint(m) => ("jira_import_failed", m.clone()),
            ImportError::Spawn(m) => ("jira_import_failed", m.clone()),
            ImportError::Internal(m) => ("jira_import_failed", m.clone()),
        }
    }

    /// Build an `ErrorBody` for the IPC surface.
    pub(crate) fn to_error_body(&self) -> cadenza_proto::wire::ErrorBody {
        let (code, message) = self.code_message();
        cadenza_proto::wire::ErrorBody::new(code, message)
    }
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (code, msg) = self.code_message();
        write!(f, "[{code}] {msg}")
    }
}

/// Internal outcome of [`jira_import_persist`]. Holds the `RunSecret`
/// in-memory for the spawn tail ONLY; it is intentionally NOT `Serialize` and
/// NEVER logged so the capability secret cannot leak through the wire. The
/// derived `Debug` is safe — `RunSecret`'s own `Debug` redacts the plaintext.
#[derive(Debug)]
pub(crate) enum ImportPersistOutcome {
    New {
        record: cadenza_proto::JiraIssueRecord,
        analysis_run_id: String,
        secret: RunSecret,
        summary: String,
    },
    ExistingActive {
        record: cadenza_proto::JiraIssueRecord,
    },
}

/// "Active work" predicate for the reimport-idempotency check: an Active
/// analysis run, OR a live worktree (`Ready` with the dir on disk), OR a
/// worktree mid-creation (`creating`). An inactive record (revoked/expired
/// secret, no worktree) falls through to a fresh re-mint.
fn issue_has_active_work(rec: &cadenza_proto::JiraIssueRecord) -> bool {
    let active_secret = rec.secret_status.as_deref() == Some(SecretStatus::Active.as_str());
    let live_worktree = crate::jira::worktree::ready_if_valid(rec).is_some();
    let creating = rec.worktree_state.as_deref()
        == Some(cadenza_proto::jira::WorktreeState::Creating.as_str());
    active_secret || live_worktree || creating
}

/// Steps 1-5 of import, pure & unit-testable: validate project, idempotency
/// check, upsert seed record, mint run+secret. Takes an ALREADY-FETCHED issue
/// so the transport/keyring/network is out of the unit-test path. Returns the
/// new-vs-existing decision plus the minted secret (caller-only; NEVER goes
/// into the proto `Result`, NEVER logged).
pub(crate) async fn jira_import_persist(
    state: &AppState,
    jira_site: &str,
    fetched: &crate::jira::FetchedIssue,
    project_id: &str,
) -> Result<ImportPersistOutcome, ImportError> {
    // 1. Validate project.
    let pid = project_id.trim();
    if pid.is_empty() {
        return Err(ImportError::Config("project_id is required".to_string()));
    }
    {
        let cfg = state
            .config
            .lock()
            .map_err(|e| ImportError::Internal(format!("config lock poisoned: {e}")))?;
        if !cfg.projects.iter().any(|p| p.id == pid) {
            return Err(ImportError::UnknownProject(pid.to_string()));
        }
    }

    // 2. Derive identity.
    let issue_id = fetched.jira_issue_id.as_str();

    // 3. Reimport idempotency: an existing record with active work is reopened
    //    WITHOUT re-minting/spawning. (Note: in the production path the fetch
    //    already happened; the "no second fetch" guarantee for the active case
    //    is enforced by the test-only `jira_import_via` orchestrator, which is
    //    the seam the contract specifies.)
    let existing = state
        .repo
        .read_jira_issue(jira_site, issue_id)
        .await
        .map_err(|e| ImportError::Internal(e.to_string()))?;
    if let Some(rec) = &existing {
        if issue_has_active_work(rec) {
            return Ok(ImportPersistOutcome::ExistingActive {
                record: rec.clone(),
            });
        }
    }

    // 4. Upsert the seed record. Preserve `created_at_ms` when re-using an
    //    existing inactive record; refresh `raw_adf`/`jira_key`/`project_id`.
    let now = now_ms_i64();
    let raw_adf = if fetched.raw_adf.is_null() {
        None
    } else {
        Some(
            serde_json::to_string(&fetched.raw_adf)
                .map_err(|e| ImportError::Internal(format!("serialize raw_adf: {e}")))?,
        )
    };
    let created_at_ms = existing.as_ref().map(|r| r.created_at_ms).unwrap_or(now);
    let record = cadenza_proto::JiraIssueRecord {
        jira_site: jira_site.to_string(),
        jira_issue_id: fetched.jira_issue_id.clone(),
        jira_key: fetched.jira_key.clone(),
        project_id: Some(pid.to_string()),
        analysis_run_id: None,
        secret_hash: None,
        secret_expiry_ms: None,
        secret_status: None,
        raw_adf,
        branch_name: None,
        worktree_path: None,
        base_sha: None,
        worktree_state: None,
        created_at_ms,
        updated_at_ms: now,
    };
    state
        .repo
        .upsert_jira_issue(&record)
        .await
        .map_err(|e| ImportError::Mint(e.to_string()))?;

    // 5. Mint run + secret (stamps secret_hash/expiry/status/project on the
    //    record; re-read so the returned record reflects that).
    let (analysis_run_id, secret) = create_analysis_run(state, jira_site, issue_id, Some(pid))
        .await
        .map_err(ImportError::Mint)?;
    let record = state
        .repo
        .read_jira_issue(jira_site, issue_id)
        .await
        .map_err(|e| ImportError::Internal(e.to_string()))?
        .ok_or_else(|| ImportError::Internal("record vanished after mint".to_string()))?;

    Ok(ImportPersistOutcome::New {
        record,
        analysis_run_id,
        secret,
        summary: fetched.summary.clone(),
    })
}

/// Parse the wire analyst-kind string into an [`AgenteKind`]. Accepts the
/// canonical serde forms (`claude_code`, `codex`, `copilot`, `antigravity`,
/// `opencode`) and the hyphenated CLI alias `claude-code`.
fn parse_analyst_kind(s: &str) -> Result<AgenteKind, ImportError> {
    match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "claude_code" | "claudecode" | "claude" => Ok(AgenteKind::ClaudeCode),
        "codex" => Ok(AgenteKind::Codex),
        "copilot" => Ok(AgenteKind::Copilot),
        "antigravity" | "agy" => Ok(AgenteKind::Antigravity),
        "opencode" => Ok(AgenteKind::OpenCode),
        other => Err(ImportError::Config(format!(
            "unknown analyst_kind: {other}"
        ))),
    }
}

/// Derive the canonical `jira_site` for a record key from configured Jira
/// base_url (origin/host). Mirrors the host-rule guard used by the client.
fn jira_site_from_config(state: &AppState) -> Result<String, ImportError> {
    let cfg = jira_config_snapshot(state).map_err(ImportError::Fetch)?;
    let url = crate::jira::config::validate_base_url(&cfg.base_url)
        .map_err(|e| ImportError::Config(format!("base_url: {e}")))?;
    Ok(url.origin().ascii_serialization())
}

/// Localized initial prompt sent to the analyst when decomposing a Jira issue.
/// MUST NOT contain the capability secret — the agent reads it from
/// `$CADENZA_RUN_SECRET` (injected by `jira_analyst_env`).
fn render_initial_jira_prompt(
    i18n_slot: &Mutex<I18n>,
    jira_key: &str,
    summary: &str,
    issue_id: &str,
) -> String {
    let mut args = FluentArgs::new();
    args.set("jira_key", jira_key.to_string());
    args.set("summary", summary.to_string());
    args.set("issue_id", issue_id.to_string());
    match i18n_slot.lock() {
        Ok(i18n) => i18n.t_with("agent-initial-prompt-jira", Some(&args)),
        Err(_) => format!(
            "Use the `cadenza` skill to decompose Jira issue {jira_key} ({summary}) into subtasks. Read $CADENZA_RUN_SECRET and submit via jira-materialize."
        ),
    }
}

/// Full production import: fetch (real client) -> persist (steps 1-5) ->
/// spawn the analyst (step 6, thin tail). The capability secret reaches the
/// analyst via ENV only and is never logged.
pub(crate) async fn jira_import_core(
    state: &AppState,
    args: &proto_ops::jira_import::Args,
) -> Result<proto_ops::jira_import::Result, ImportError> {
    let issue_ref = args.issue_ref.trim();
    if issue_ref.is_empty() {
        return Err(ImportError::Config("issue_ref is required".to_string()));
    }
    // Parse the analyst kind up front so a bad kind fails before any fetch.
    let kind = parse_analyst_kind(&args.analyst_kind)?;

    let jira_site = jira_site_from_config(state)?;

    // Reimport short-circuit BEFORE any network fetch: if a record for this
    // site already has active work (matched by display key OR durable id),
    // reopen it without a second fetch. This makes "open existing" work
    // offline and survive the issue being renamed/deleted on the Jira side
    // (a post-fetch check would wrongly fail with jira_not_found/jira_http).
    {
        let existing = state
            .repo
            .list_jira_issues()
            .await
            .map_err(|e| ImportError::Internal(e.to_string()))?
            .into_iter()
            .find(|r| {
                r.jira_site == jira_site
                    && (r.jira_key == issue_ref || r.jira_issue_id == issue_ref)
                    && issue_has_active_work(r)
            });
        if let Some(record) = existing {
            return Ok(proto_ops::jira_import::Result::ExistingActive {
                jira_site: record.jira_site,
                jira_issue_id: record.jira_issue_id,
                jira_key: record.jira_key,
                project_id: record.project_id,
                analysis_run_id: record.analysis_run_id,
            });
        }
    }

    // Fetch (real client) — keeps the transport/keyring/network leg here, out
    // of the unit-test path (which drives `jira_import_persist` directly).
    let fetch_args = proto_ops::jira_fetch_issue::Args {
        key: issue_ref.to_string(),
    };
    let fetched = {
        let r = jira_fetch_issue_core(state, &fetch_args)
            .await
            .map_err(ImportError::Fetch)?;
        crate::jira::FetchedIssue {
            jira_issue_id: r.jira_issue_id,
            jira_key: r.jira_key,
            summary: r.summary,
            description_markdown: r.description_markdown,
            raw_adf: r.raw_adf,
        }
    };

    match jira_import_persist(state, &jira_site, &fetched, &args.project_id).await? {
        ImportPersistOutcome::ExistingActive { record } => {
            Ok(proto_ops::jira_import::Result::ExistingActive {
                jira_site: record.jira_site,
                jira_issue_id: record.jira_issue_id,
                jira_key: record.jira_key,
                project_id: record.project_id,
                analysis_run_id: record.analysis_run_id,
            })
        }
        ImportPersistOutcome::New {
            record,
            analysis_run_id,
            secret,
            summary,
        } => {
            // Step 6 — analyst spawn (thin tail, mirrors `destrinchar_ideia`).
            // The capability secret is already minted + persisted (status=Active)
            // by `jira_import_persist`. If ANY step below fails we MUST revoke it,
            // otherwise the record keeps `issue_has_active_work` true and the early
            // short-circuit would forever return `ExistingActive` — the issue would
            // be stuck and un-re-importable. So run the spawn tail in an inner block
            // and revoke the secret on error before propagating.
            let spawned: Result<proto_ops::jira_import::Result, ImportError> = async {
                let pid = record.project_id.clone().ok_or_else(|| {
                    ImportError::Internal("record missing project_id".to_string())
                })?;
                let (cwd, command_override) = {
                    let cfg = state
                        .config
                        .lock()
                        .map_err(|e| ImportError::Internal(format!("config lock poisoned: {e}")))?;
                    let project = cfg
                        .projects
                        .iter()
                        .find(|p| p.id == pid)
                        .ok_or_else(|| ImportError::UnknownProject(pid.clone()))?;
                    let cmd = project
                        .agente
                        .as_ref()
                        .filter(|a| a.kind == kind)
                        .and_then(|a| a.command.clone())
                        .or_else(|| {
                            cfg.agente
                                .as_ref()
                                .filter(|a| a.kind == kind)
                                .and_then(|a| a.command.clone())
                        });
                    (project.path.clone(), cmd)
                };
                if !cwd.exists() {
                    return Err(ImportError::Spawn(format!(
                        "project path does not exist: {} — fix it in Settings → Projetos",
                        cwd.display()
                    )));
                }

                // jira_site is a full origin ("https://acme.atlassian.net"); strip
                // the scheme and sanitize so the synthetic id (exported as
                // TASKAI_TASK_ID) is safe to use verbatim in paths/argv.
                let host = jira_site.rsplit("://").next().unwrap_or(jira_site.as_str());
                let site_token: String = host
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                    .collect();
                let synthetic_task_id = format!("JIRA-{}-{}", site_token, fetched.jira_issue_id);
                let prompt = render_initial_jira_prompt(
                    &state.i18n,
                    &fetched.jira_key,
                    &summary,
                    &fetched.jira_issue_id,
                );
                let model = String::new();
                let plan: LaunchPlan = agent::plan_launch(
                    kind,
                    &model,
                    command_override.as_deref(),
                    &cwd,
                    &synthetic_task_id,
                    &pid,
                    None,
                    Some(&prompt),
                );
                let LaunchPlan {
                    spawn,
                    conversation_id_known: _,
                    pending_codex_capture,
                    pending_opencode_capture: _,
                    prompt_delivery,
                } = plan;
                // The capability secret reaches the analyst here via ENV ONLY.
                let spawn = spawn.jira_analyst_env(
                    &analysis_run_id,
                    secret.expose(),
                    &jira_site,
                    &fetched.jira_issue_id,
                    &fetched.jira_key,
                );

                let pty = PtyHandle::spawn(spawn).map_err(|e| ImportError::Spawn(e.to_string()))?;
                let session_id = format!("S-{}", Uuid::new_v4().simple());
                let session = TerminalSession::start(session_id.clone(), pty)
                    .map_err(|e| ImportError::Spawn(e.to_string()))?;
                state
                    .sessions
                    .lock()
                    .map_err(|e| ImportError::Internal(e.to_string()))?
                    .insert(session_id.clone(), session.clone());
                // Log identity only — NEVER the secret.
                tracing::info!(
                    analysis_run_id = %analysis_run_id,
                    jira_key = %fetched.jira_key,
                    session = %session_id,
                    "jira analyst started"
                );

                if prompt_delivery == PromptDelivery::TypeIn {
                    let session_for_prompt = session.clone();
                    tauri::async_runtime::spawn(async move {
                        send_initial_prompt(&session_for_prompt, &prompt).await;
                    });
                }
                if let Some(capture) = pending_codex_capture {
                    tauri::async_runtime::spawn(async move {
                        let _ = wait_for_codex_uuid(capture).await;
                    });
                }

                Ok(proto_ops::jira_import::Result::Imported {
                    jira_site,
                    jira_issue_id: fetched.jira_issue_id,
                    jira_key: fetched.jira_key,
                    summary,
                    project_id: pid,
                    analysis_run_id: analysis_run_id.clone(),
                    session_id,
                })
            }
            .await;
            match spawned {
                Ok(result) => Ok(result),
                Err(e) => {
                    // Roll back the just-minted capability so the issue stays
                    // re-importable. Best-effort: a revoke failure is logged but
                    // must not mask the original spawn error.
                    if let Err(re) = revoke_run_secret(state, &analysis_run_id).await {
                        tracing::warn!(
                            error = %re,
                            analysis_run_id = %analysis_run_id,
                            "failed to revoke run secret after jira analyst spawn failure"
                        );
                    }
                    Err(e)
                }
            }
        }
    }
}

#[tauri::command]
pub async fn jira_import(
    state: State<'_, Arc<AppState>>,
    args: proto_ops::jira_import::Args,
) -> Result<proto_ops::jira_import::Result, String> {
    jira_import_core(&state, &args)
        .await
        .map_err(|e| e.to_string())
}

// ───────────────────────── Jira discard (Slice 6a) ─────────────────────────

/// Failure surface for the discard lifecycle.
#[derive(Debug)]
pub(crate) enum DiscardError {
    /// No record for `(jira_site, jira_issue_id)`. Maps to `jira_not_found`
    /// (exit 30).
    NotFound,
    /// A subtask agent is live for this issue. Maps to `jira_worktree_busy`
    /// (exit 1).
    Busy,
    /// The worktree has uncommitted/untracked changes and `force` was not
    /// set. Carries the COUNT only — never file names. Maps to
    /// `jira_worktree_dirty` (exit 1).
    WorktreeDirty { changed_files: u32 },
    /// `git worktree remove` failed. Maps to `jira_worktree_failed` (exit 1).
    RemoveFailed(String),
    /// Any other store/internal failure. Maps to `jira_worktree_failed`
    /// (exit 1).
    Internal(String),
}

impl DiscardError {
    pub(crate) fn code_message(&self) -> (&'static str, String) {
        match self {
            DiscardError::NotFound => (
                "jira_not_found",
                "no jira issue record to discard".to_string(),
            ),
            DiscardError::Busy => (
                "jira_worktree_busy",
                "a subtask agent is still running for this Jira issue".to_string(),
            ),
            DiscardError::WorktreeDirty { changed_files } => (
                "jira_worktree_dirty",
                format!(
                    "worktree has {changed_files} uncommitted/untracked change(s); pass --force to discard"
                ),
            ),
            DiscardError::RemoveFailed(m) => ("jira_worktree_failed", m.clone()),
            DiscardError::Internal(m) => ("jira_worktree_failed", m.clone()),
        }
    }

    pub(crate) fn to_error_body(&self) -> cadenza_proto::wire::ErrorBody {
        let (code, message) = self.code_message();
        cadenza_proto::wire::ErrorBody::new(code, message)
    }
}

impl std::fmt::Display for DiscardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (code, msg) = self.code_message();
        write!(f, "[{code}] {msg}")
    }
}

/// Discard an imported Jira issue: refuse a dirty worktree unless forced,
/// remove the worktree, revoke the run secret, delete the record, and forget
/// subtask sidecars. RETAINS the branch, the produced subtask Tasks, and any
/// aggregate review packages (audit trail). Keyed by `(site, issue_id)`; the
/// `delete_task` path never calls this.
pub(crate) async fn jira_discard_core(
    state: &AppState,
    args: &proto_ops::jira_discard::Args,
) -> Result<proto_ops::jira_discard::Result, DiscardError> {
    let site = args.jira_site.as_str();
    let issue = args.jira_issue_id.as_str();

    // 1. Read record.
    let record = state
        .repo
        .read_jira_issue(site, issue)
        .await
        .map_err(|e| DiscardError::Internal(e.to_string()))?
        .ok_or(DiscardError::NotFound)?;

    // 2. Busy check — refuse if a subtask agent is live for this issue.
    {
        let active = state
            .jira_active_executors
            .lock()
            .map_err(|e| DiscardError::Internal(e.to_string()))?;
        let sessions = state
            .sessions
            .lock()
            .map_err(|e| DiscardError::Internal(e.to_string()))?;
        let key = (site.to_string(), issue.to_string());
        if crate::jira::worktree::issue_executor_busy(&active, &sessions, &key) {
            return Err(DiscardError::Busy);
        }
    }

    // 3. Dirty check + 4. remove worktree.
    let mut worktree_removed = false;
    if let Some(wt) = record.worktree_path.as_deref() {
        let wt_path = Path::new(wt);
        if wt_path.exists() {
            let dirty = crate::git::worktree_dirty_files(wt_path)
                .await
                .map_err(|e| DiscardError::RemoveFailed(e.to_string()))?;
            if !dirty.is_empty() && !args.force {
                // Count only — never the file names (no sensitive paths on the
                // wire). The caller learns work would be lost.
                return Err(DiscardError::WorktreeDirty {
                    changed_files: dirty.len() as u32,
                });
            }
            // Resolve the repo path from the record's project_id.
            let repo = record
                .project_id
                .as_deref()
                .and_then(|pid| {
                    state.config.lock().ok().and_then(|cfg| {
                        cfg.projects
                            .iter()
                            .find(|p| p.id == pid)
                            .map(|p| p.path.clone())
                    })
                })
                .ok_or_else(|| {
                    DiscardError::RemoveFailed(
                        "cannot resolve repo path for worktree removal".to_string(),
                    )
                })?;
            crate::git::remove_worktree(&repo, wt_path, args.force)
                .await
                .map_err(|e| DiscardError::RemoveFailed(e.to_string()))?;
            if let Err(e) = crate::git::worktree_prune(&repo).await {
                tracing::warn!(error = %e, "worktree_prune after discard failed (advisory)");
            }
            worktree_removed = true;
        }
    }

    // 5. Revoke the run secret (idempotent, best-effort).
    if let Some(run_id) = record.analysis_run_id.as_deref() {
        if let Err(e) = revoke_run_secret(state, run_id).await {
            tracing::warn!(error = %e, "failed to revoke run secret during discard");
        }
    }

    // 6. Delete the record (drops raw_adf + secret columns with the row).
    state
        .repo
        .delete_jira_issue(site, issue)
        .await
        .map_err(|e| DiscardError::Internal(e.to_string()))?;

    // 7. Cascade sidecars (best-effort, warn-on-err). Enumerate subtask task
    //    ids bound to this issue from the task store (no reverse index on
    //    TaskWorktrees), then forget each task_worktrees entry.
    let mut forgotten_task_worktrees = 0u32;
    match state.repo.list_tasks(None).await {
        Ok(tasks) => {
            for task in tasks {
                let enriched = state.task_jira.enrich(task);
                let belongs = enriched.jira_site.as_deref() == Some(site)
                    && enriched.jira_issue_id.as_deref() == Some(issue);
                if belongs && state.task_worktrees.get(&enriched.id).is_some() {
                    if let Err(e) = state.task_worktrees.forget(&enriched.id) {
                        tracing::warn!(error = ?e, task = %enriched.id, "task_worktrees.forget during discard failed");
                    } else {
                        forgotten_task_worktrees += 1;
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = ?e, "list_tasks during discard cascade failed");
        }
    }

    // Drop the in-process lock + executor slot for this issue.
    if let Ok(mut locks) = state.jira_worktree_locks.lock() {
        locks.remove(&(site.to_string(), issue.to_string()));
    }
    if let Ok(mut active) = state.jira_active_executors.lock() {
        active.remove(&(site.to_string(), issue.to_string()));
    }

    Ok(proto_ops::jira_discard::Result {
        jira_site: site.to_string(),
        jira_issue_id: issue.to_string(),
        worktree_removed,
        forgotten_task_worktrees,
    })
}

#[tauri::command]
pub async fn jira_discard(
    state: State<'_, Arc<AppState>>,
    args: proto_ops::jira_discard::Args,
) -> Result<proto_ops::jira_discard::Result, String> {
    jira_discard_core(&state, &args)
        .await
        .map_err(|e| e.to_string())
}

/// Deterministic content key for an aggregate review attempt: a repeat build on
/// the SAME branch state (same `base_sha`/`head_sha`) dedups to a no-op, while
/// a new branch HEAD yields a new attempt. Hashed so the raw site/issue is
/// never a path/key component on the file backend.
fn issue_review_idempotency_key(
    site: &str,
    issue_id: &str,
    base_sha: &str,
    head: Option<&str>,
) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(site.as_bytes());
    h.update([0]);
    h.update(issue_id.as_bytes());
    h.update([0]);
    h.update(base_sha.as_bytes());
    h.update([0]);
    h.update(head.unwrap_or("").as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Shared `jira_review` logic (Slice 5): build the aggregate (issue-owned)
/// branch-diff review, stamp the deterministic idempotency key, and persist it.
///
/// STATE-NEUTRAL: this builds the committed branch diff via the hardened,
/// read-only git layer and persists ONLY the aggregate package — it NEVER
/// calls `set_estado`/`done`/`apply_review_decision`, never appends a task log,
/// and never reads or mutates any subtask estado.
pub(crate) async fn jira_review_core(
    state: &AppState,
    jira_site: &str,
    jira_issue_id: &str,
) -> Result<crate::store::IssueReviewPackage, crate::review::issue::IssueReviewError> {
    use crate::review::issue::IssueReviewError;
    let mut pkg =
        crate::review::issue::build_issue_review(state.repo.as_ref(), jira_site, jira_issue_id)
            .await?;
    pkg.idempotency_key = issue_review_idempotency_key(
        jira_site,
        jira_issue_id,
        &pkg.base_sha,
        pkg.head_sha.as_deref(),
    );
    let stored = state
        .repo
        .upsert_issue_review_package(&pkg)
        .await
        .map_err(|e| IssueReviewError::DiffFailed(e.to_string()))?;
    Ok(stored)
}

#[tauri::command]
pub async fn jira_review(
    state: State<'_, Arc<AppState>>,
    jira_site: String,
    jira_issue_id: String,
) -> Result<crate::store::IssueReviewPackage, String> {
    jira_review_core(&state, &jira_site, &jira_issue_id)
        .await
        .map_err(|e| e.to_string())
}
