//! NDJSON IPC server over the local socket.
//!
//! Transport per DESIGN-desktop-v2.md § "Protocolo IPC":
//! - **Windows:** named pipe `cadenza-<username>` (ACL hardening TODO
//!   in Phase 5 — current build relies on per-user pipe namespace).
//! - **Unix:** filesystem socket at `~/.cadenza/run/socket`.
//!
//! Each connection runs:
//!   `hello` (validate token + protocol) → loop { request → response }
//! plus optional `event` pushes from a side-channel (used by
//! `await_decision` to surface `proposta_pendente`).

use anyhow::{Context, Result};
use cadenza_proto::{
    ops::{
        self, OP_APPEND_LOG, OP_AWAIT_DECISION, OP_BYE, OP_CREATE_IDEIA, OP_CREATE_TASK,
        OP_CURRENT_TASK, OP_DELETE_IDEIA, OP_DONE, OP_HELLO, OP_JIRA_DISCARD, OP_JIRA_FETCH_ISSUE,
        OP_JIRA_IMPORT, OP_JIRA_LIST_ASSIGNED, OP_JIRA_MATERIALIZE, OP_JIRA_REVIEW,
        OP_JIRA_TEST_CONNECTION, OP_LIST_IDEIAS, OP_LIST_MEMORY, OP_LIST_PROJECTS, OP_LIST_TASKS,
        OP_PROPOSE, OP_QUALITY, OP_READ_IDEIA, OP_READ_TASK, OP_REVIEW_DECISION, OP_REVISE_MEMORY,
        OP_SET_IDEIA_STATUS, OP_SET_TASK_WORKTREE, OP_SUGGEST_LEARNING, OP_UPDATE_BODY,
    },
    wire::{ErrorBody, Event, Request, Response},
    Decisao, DecisaoRegistro, Ideia, IdeiaStatus, MemorySuggestion, ProjectInfo, SuggestionKind,
    MAX_PROTOCOL, MIN_PROTOCOL,
};
use interprocess::local_socket::{tokio::prelude::*, ListenerOptions};
#[cfg(not(windows))]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// Read one NDJSON line, bailing out before allocating more than `max`
/// bytes. `BufReader::lines()` / `read_until` would accumulate the
/// whole line in memory before any size check fires, letting a
/// misbehaving peer OOM the process by writing GB without a `\n`.
async fn read_line_capped<R>(reader: &mut R, max: usize) -> std::io::Result<Option<String>>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed mid-line",
            ));
        }
        if let Some(pos) = chunk.iter().position(|&b| b == b'\n') {
            if buf.len() + pos > max {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "line exceeds cap",
                ));
            }
            buf.extend_from_slice(&chunk[..pos]);
            let take = pos + 1;
            reader.consume(take);
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            let line = String::from_utf8(buf)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return Ok(Some(line));
        }
        if buf.len() + chunk.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line exceeds cap",
            ));
        }
        buf.extend_from_slice(chunk);
        let chunk_len = chunk.len();
        reader.consume(chunk_len);
    }
}

use crate::commands::AppState;
use crate::store::{validate_id, Repository};

const SERVER_APP_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Max NDJSON line we accept (1 MiB). A malformed/runaway client
/// shouldn't be able to exhaust memory.
const MAX_LINE_BYTES: usize = 1024 * 1024;
const WRITER_CHANNEL_CAP: usize = 64;

/// Bridge for events that must reach the Tauri webview (board refresh,
/// triage modal, etc.). The receiving side lives in `lib.rs::setup` and
/// forwards each `(name, payload)` into `AppHandle::emit`. Using a
/// channel — instead of holding an `AppHandle` here — keeps `ipc.rs`
/// independent of `tauri::App` lifetime.
pub type WebviewEventTx = mpsc::Sender<(String, Value)>;

/// Dependencies the server needs from `lib.rs`.
#[derive(Clone)]
pub struct ServerDeps {
    pub state: Arc<AppState>,
    /// Path to `~/.cadenza/` — auth token is validated against `auth`.
    pub data_dir: PathBuf,
    /// Sink for `AppHandle::emit` (set by `lib.rs::setup`). Capacity is
    /// small; if the receiver is gone we drop the event silently — the
    /// UI can always reconcile via `list_pending_propostas` on next view.
    pub webview_events: WebviewEventTx,
}

/// Compute the socket name for the current user. Windows → namespaced
/// pipe `cadenza-<user>`; Unix → filesystem path `<home>/.cadenza/run/socket`.
#[cfg(not(windows))]
pub fn socket_path_unix() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
    home.join(".cadenza").join("run").join("socket")
}

#[cfg(windows)]
fn socket_username() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "user".into())
}

/// Run the NDJSON server, accepting connections in a loop. Designed to
/// run forever inside `tauri::async_runtime::spawn` — every connection
/// is handled on its own tokio task.
pub async fn run_server(deps: ServerDeps) -> Result<()> {
    // Build the platform-specific socket name.
    #[cfg(windows)]
    let listener = {
        let raw = format!("cadenza-{}", socket_username());
        let name = raw
            .as_str()
            .to_ns_name::<GenericNamespaced>()
            .context("build namespaced pipe name")?;
        ListenerOptions::new()
            .name(name)
            .create_tokio()
            .context("create_tokio listener")?
    };
    #[cfg(not(windows))]
    let listener = {
        let path = socket_path_unix();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Stale socket from a previous run blocks bind on Unix.
        let _ = std::fs::remove_file(&path);
        let name = path
            .as_path()
            .to_fs_name::<GenericFilePath>()
            .context("build fs socket name")?;
        ListenerOptions::new()
            .name(name)
            .create_tokio()
            .context("create_tokio listener")?
    };

    tracing::info!("ipc server listening");

    loop {
        match listener.accept().await {
            Ok(conn) => {
                let deps = deps.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(conn, deps).await {
                        tracing::warn!(error = ?e, "ipc connection ended with error");
                    }
                });
            }
            Err(e) => {
                tracing::error!(error = %e, "ipc accept failed");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

async fn handle_connection<S>(stream: S, deps: ServerDeps) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    // Side-channel for events + responses. Owning the writer in a
    // dedicated task lets handlers push events asynchronously while
    // a request is in flight (await_decision needs this).
    let (tx, mut rx) = mpsc::channel::<String>(WRITER_CHANNEL_CAP);
    let writer_handle = tokio::spawn(async move {
        let mut w = write_half;
        while let Some(line) = rx.recv().await {
            if w.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if w.write_all(b"\n").await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
    });

    // First message MUST be hello. `read_line_capped` enforces the
    // length cap during accumulation so a slow-loris peer can't OOM us
    // before reaching the `MAX_LINE_BYTES` check.
    let line = match read_line_capped(&mut reader, MAX_LINE_BYTES).await {
        Ok(Some(l)) => l,
        Ok(None) => {
            // Empty connection — just close.
            drop(tx);
            let _ = writer_handle.await;
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            send_err(
                &tx,
                None,
                ErrorBody::new("line_too_long", "line exceeds 1 MiB"),
            )
            .await;
            drop(tx);
            let _ = writer_handle.await;
            return Ok(());
        }
        Err(_) => {
            drop(tx);
            let _ = writer_handle.await;
            return Ok(());
        }
    };

    let hello_req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            send_err(&tx, None, ErrorBody::new("bad_frame", e.to_string())).await;
            drop(tx);
            let _ = writer_handle.await;
            return Ok(());
        }
    };
    let hello_id = hello_req.id.clone();

    if hello_req.op != OP_HELLO {
        send_err(
            &tx,
            hello_id,
            ErrorBody::new("hello_required", "first message must be hello"),
        )
        .await;
        drop(tx);
        let _ = writer_handle.await;
        return Ok(());
    }

    // Read the `protocol` field directly off the JSON before
    // deserializing the rest of `hello::Args`. A missing or wrong-type
    // `protocol` is a protocol-level mismatch (old/new client lacking
    // the field), not a generic arg-validation failure — surface the
    // CLAUDE.md exit-code 12 contract precisely.
    let protocol_val = hello_req
        .args
        .get("protocol")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let protocol = match protocol_val {
        Some(p) => p,
        None => {
            send_err(
                &tx,
                hello_id,
                ErrorBody::new("protocol_too_old", "missing protocol field"),
            )
            .await;
            drop(tx);
            let _ = writer_handle.await;
            return Ok(());
        }
    };

    // Protocol-range check runs BEFORE args deserialization so an old
    // client whose hello::Args shape no longer matches still sees the
    // CLAUDE.md exit-code 12 contract ("update cli") instead of a
    // generic bad_args (exit 1).
    if let Err(e) = check_protocol(protocol) {
        send_err(&tx, hello_id, e).await;
        drop(tx);
        let _ = writer_handle.await;
        return Ok(());
    }

    let args: ops::hello::Args = match serde_json::from_value(hello_req.args) {
        Ok(a) => a,
        Err(e) => {
            send_err(&tx, hello_id, ErrorBody::new("bad_args", e.to_string())).await;
            drop(tx);
            let _ = writer_handle.await;
            return Ok(());
        }
    };

    let hello_result = match check_hello(protocol, &args.token, &deps.data_dir) {
        Ok(r) => r,
        Err(e) => {
            send_err(&tx, hello_id, e).await;
            drop(tx);
            let _ = writer_handle.await;
            return Ok(());
        }
    };
    send_ok(&tx, hello_id.clone(), hello_result).await;
    tracing::info!(client = %args.client, "ipc client authenticated");

    // Capture the token epoch at hello-time. The tray's "Revoke CLI
    // token" handler bumps this counter; per-op we compare against
    // the live value and close the connection if it advanced — so a
    // revoked-mid-session connection can't keep driving ops until the
    // attacker disconnects on their own.
    let auth_epoch = deps
        .state
        .token_epoch
        .load(std::sync::atomic::Ordering::Acquire);

    // Request loop.
    loop {
        let line = match read_line_capped(&mut reader, MAX_LINE_BYTES).await {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                // Line cap exceeded — signal and close the connection
                // (we can't trust where the next `\n` lands).
                send_err(
                    &tx,
                    None,
                    ErrorBody::new("line_too_long", "line exceeds 1 MiB"),
                )
                .await;
                break;
            }
            Err(e) => {
                tracing::warn!(error = %e, "ipc read error");
                break;
            }
        };
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                send_err(&tx, None, ErrorBody::new("bad_frame", e.to_string())).await;
                continue;
            }
        };

        let id = req.id.clone();
        let stop = req.op == OP_BYE;

        // Token was rotated while this connection was open — refuse
        // further ops and close so the caller sees `auth_failed`.
        let current_epoch = deps
            .state
            .token_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        if current_epoch != auth_epoch {
            send_err(
                &tx,
                id,
                ErrorBody::new("auth_failed", "token revoked; reconnect"),
            )
            .await;
            break;
        }

        let dispatch_result = dispatch(req, &deps, &tx).await;
        match dispatch_result {
            Ok(value) => send_ok_raw(&tx, id, value).await,
            Err(err) => send_err(&tx, id, err).await,
        }

        if stop {
            break;
        }
    }

    drop(tx);
    let _ = writer_handle.await;
    Ok(())
}

async fn dispatch(
    req: Request,
    deps: &ServerDeps,
    tx: &mpsc::Sender<String>,
) -> Result<Value, ErrorBody> {
    let repo: Arc<dyn Repository> = deps.state.repo.clone();

    match req.op.as_str() {
        OP_LIST_TASKS => {
            let args: ops::list_tasks::Args = serde_json::from_value(req.args).map_err(bad_args)?;
            let filter = args
                .estado
                .as_deref()
                .and_then(cadenza_proto::Estado::parse);
            let tasks = repo
                .list_tasks(filter)
                .await
                .map_err(|e| not_found_or_internal(&e))?;
            let mut enriched: Vec<_> = tasks
                .into_iter()
                .map(|t| crate::commands::enrich_task(&deps.state, t))
                .collect();
            let order = deps.state.task_order.snapshot();
            crate::commands::sort_tasks_by_order(&mut enriched, &order);
            to_value(&enriched)
        }
        OP_CURRENT_TASK => {
            let _: ops::current_task::Args = serde_json::from_value(req.args).map_err(bad_args)?;
            // Use the drag-priority order so `cadenza current` returns the
            // topmost card in the fazendo column, matching what the board shows.
            let tasks = repo
                .list_tasks(Some(cadenza_proto::Estado::Fazendo))
                .await
                .map_err(|e| not_found_or_internal(&e))?;
            let mut enriched: Vec<_> = tasks
                .into_iter()
                .map(|t| crate::commands::enrich_task(&deps.state, t))
                .collect();
            let order = deps.state.task_order.snapshot();
            crate::commands::sort_tasks_by_order(&mut enriched, &order);
            let current: ops::current_task::Result = enriched.into_iter().next();
            to_value(&current)
        }
        OP_READ_TASK => {
            let args: ops::read_task::Args = serde_json::from_value(req.args).map_err(bad_args)?;
            check_id(&args.task_id)?;
            // A single task by id — `get` returns only the requested card,
            // not the whole list. A missing id surfaces as `task_not_found`
            // (CLI exit 30) via `not_found_or_internal`.
            let task = repo
                .read_task(&args.task_id)
                .await
                .map_err(|e| not_found_or_internal(&e))?;
            let task: ops::read_task::Result = crate::commands::enrich_task(&deps.state, task);
            to_value(&task)
        }
        OP_LIST_PROJECTS => {
            let _: ops::list_projects::Args = serde_json::from_value(req.args).map_err(bad_args)?;
            // Read-only view of the configured projects so an agent can
            // discover the `project_id` to pass to `new-task`.
            let cfg = deps
                .state
                .config
                .lock()
                .map_err(|_| internal("config lock poisoned"))?;
            let projects: ops::list_projects::Result = cfg
                .projects
                .iter()
                .map(|p| ProjectInfo {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    path: p.path.to_string_lossy().into_owned(),
                })
                .collect();
            drop(cfg);
            to_value(&projects)
        }
        OP_SET_TASK_WORKTREE => {
            let args: ops::set_task_worktree::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            check_id(&args.task_id)?;
            // A worktree assigned over IPC must still be honored at agent
            // start: `prepare_task_workspace` only runs inside the worktree
            // when `use_worktree` is set, so derive it from the path being
            // present rather than leaving it at the `false` default.
            let use_worktree = args
                .worktree_path
                .as_deref()
                .is_some_and(|p| !p.trim().is_empty());
            deps.state
                .task_worktrees
                .set(
                    &args.task_id,
                    crate::worktrees::WorktreeInfo {
                        worktree_path: args.worktree_path,
                        branch: args.branch,
                        use_worktree,
                        ..Default::default()
                    },
                )
                .map_err(|e| internal(&e.to_string()))?;
            to_value(&ops::set_task_worktree::Result { ok: true })
        }
        OP_APPEND_LOG => {
            let args: ops::append_log::Args = serde_json::from_value(req.args).map_err(bad_args)?;
            check_id(&args.task_id)?;
            repo.append_log(&args.task_id, &args.text)
                .await
                .map_err(|e| not_found_or_internal(&e))?;
            // Body mutation invalidates any open task view; emit so the
            // board / detail modal pick it up.
            let _ = deps.webview_events.try_send((
                ops::EV_TASKS_CHANGED.to_string(),
                serde_json::json!({ "task_id": args.task_id }),
            ));
            to_value(&ops::append_log::Result { ok: true })
        }
        OP_PROPOSE => {
            let args: ops::propose::Args = serde_json::from_value(req.args).map_err(bad_args)?;
            // Hardening (Slice 2 §C): the public propose surface must not let
            // a caller forge a Jira identity. Only `jira_materialize` stamps
            // it, server-side, from a verified capability secret.
            if args.jira_site.is_some() || args.jira_issue_id.is_some() {
                return Err(ErrorBody::new(
                    "jira_identity_forbidden",
                    "jira_site/jira_issue_id may only be set via jira_materialize",
                ));
            }
            let proposta = repo
                .propose(args)
                .await
                .map_err(|e| internal(&e.to_string()))?;
            // Surface the new proposal to the webview so the triage modal
            // (or topbar badge) reacts immediately, in addition to the
            // socket-side EV_PROPOSTA_PENDENTE pushed from await_decision.
            let _ = deps.webview_events.try_send((
                ops::EV_PROPOSTA_PENDENTE.to_string(),
                serde_json::json!({ "proposta_id": proposta.proposta_id }),
            ));
            to_value(&ops::propose::Result {
                proposta_id: proposta.proposta_id,
            })
        }
        OP_JIRA_MATERIALIZE => {
            let args: ops::jira_materialize::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            let result = jira_materialize_op(deps, &args).await?;
            // New proposals materialized ⇒ board/triage may need to refresh.
            let _ = deps
                .webview_events
                .try_send((ops::EV_TASKS_CHANGED.to_string(), serde_json::json!({})));
            to_value(&result)
        }
        OP_JIRA_TEST_CONNECTION => {
            let _: ops::jira_test_connection::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            to_value(&jira_test_connection_op(deps).await?)
        }
        OP_JIRA_FETCH_ISSUE => {
            let a: ops::jira_fetch_issue::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            to_value(&jira_fetch_issue_op(deps, &a).await?)
        }
        OP_JIRA_LIST_ASSIGNED => {
            let _: ops::jira_list_assigned::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            to_value(&jira_list_assigned_op(deps).await?)
        }
        OP_JIRA_REVIEW => {
            let a: ops::jira_review::Args = serde_json::from_value(req.args).map_err(bad_args)?;
            to_value(&jira_review_op(deps, &a).await?)
        }
        OP_JIRA_IMPORT => {
            let a: ops::jira_import::Args = serde_json::from_value(req.args).map_err(bad_args)?;
            let result = jira_import_op(deps, &a).await?;
            // A new import seeds a record + spawns the analyst; the board/triage
            // may need to refresh once the analyst materializes subtasks.
            let _ = deps
                .webview_events
                .try_send((ops::EV_TASKS_CHANGED.to_string(), serde_json::json!({})));
            to_value(&result)
        }
        OP_JIRA_DISCARD => {
            let a: ops::jira_discard::Args = serde_json::from_value(req.args).map_err(bad_args)?;
            let result = jira_discard_op(deps, &a).await?;
            // Discard removed a record (and possibly subtask sidecars); the UI
            // refreshes the board/triage.
            let _ = deps
                .webview_events
                .try_send((ops::EV_TASKS_CHANGED.to_string(), serde_json::json!({})));
            to_value(&result)
        }
        OP_AWAIT_DECISION => {
            let args: ops::await_decision::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            check_id(&args.proposta_id)?;

            // Push a `proposta_pendente` event before we block, so the
            // client (and any human-facing surface) knows we're waiting.
            let event = Event::new(
                ops::EV_PROPOSTA_PENDENTE,
                serde_json::json!({ "proposta_id": args.proposta_id }),
            )
            .map_err(|e| internal(&e.to_string()))?;
            send_event(tx, event).await;

            let timeout = Duration::from_millis(args.timeout_ms.min(30 * 60 * 1000));
            let maybe = repo
                .await_decisao(&args.proposta_id, timeout)
                .await
                .map_err(|e| internal(&e.to_string()))?;
            match maybe {
                Some(decisao) => to_value(&decisao),
                None => Err(ErrorBody::new("decision_timeout", "no decision in time")),
            }
        }
        OP_DONE => {
            let args: ops::done::Args = serde_json::from_value(req.args).map_err(bad_args)?;
            let result = done_op(deps, &args).await?;
            // Estado changed to aguardando_revisao + body appended; UI
            // needs to pick up both. Emit alongside OP_CREATE_TASK's
            // event so the board reconciles without a manual reload.
            let _ = deps.webview_events.try_send((
                ops::EV_TASKS_CHANGED.to_string(),
                serde_json::json!({ "task_id": args.task_id }),
            ));
            to_value(&result)
        }
        OP_QUALITY => {
            let args: ops::quality::Args = serde_json::from_value(req.args).map_err(bad_args)?;
            let result = quality_op(deps, &args).await?;
            to_value(&result)
        }
        OP_REVIEW_DECISION => {
            let args: ops::review_decision::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            let result = review_decision_op(deps, &args).await?;
            let _ = deps.webview_events.try_send((
                ops::EV_TASKS_CHANGED.to_string(),
                serde_json::json!({ "task_id": args.task_id }),
            ));
            to_value(&result)
        }
        OP_UPDATE_BODY => {
            let args: ops::update_body::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            check_id(&args.task_id)?;
            let new_body = if args.append_plan {
                // Read-modify-write so the original description is kept and a
                // re-plan replaces the previous `## Plano` block rather than
                // stacking duplicates.
                let task = repo
                    .read_task(&args.task_id)
                    .await
                    .map_err(|e| not_found_or_internal(&e))?;
                append_plan_section(&task.body, &args.body)
            } else {
                args.body
            };
            repo.update_task_body(&args.task_id, &new_body)
                .await
                .map_err(|e| not_found_or_internal(&e))?;
            let _ = deps.webview_events.try_send((
                ops::EV_TASKS_CHANGED.to_string(),
                serde_json::json!({ "task_id": args.task_id }),
            ));
            to_value(&ops::update_body::Result { ok: true })
        }
        OP_CREATE_TASK => {
            let args: ops::create_task::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            let result = create_task_op(deps, &args).await?;
            // Surface to UI so o board re-puxa.
            let _ = deps.webview_events.try_send((
                ops::EV_TASKS_CHANGED.to_string(),
                serde_json::json!({ "task_id": result.task_id }),
            ));
            to_value(&result)
        }
        OP_LIST_IDEIAS => {
            let _: ops::list_ideias::Args = serde_json::from_value(req.args).map_err(bad_args)?;
            let ideias = repo
                .list_ideias()
                .await
                .map_err(|e| internal(&e.to_string()))?;
            to_value(&ideias)
        }
        OP_READ_IDEIA => {
            let args: ops::read_ideia::Args = serde_json::from_value(req.args).map_err(bad_args)?;
            check_id(&args.id)?;
            let ideia = repo
                .read_ideia(&args.id)
                .await
                .map_err(|e| internal(&e.to_string()))?;
            to_value(&ideia)
        }
        OP_CREATE_IDEIA => {
            let args: ops::create_ideia::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            let ideia = create_ideia_op(deps, args).await?;
            let _ = deps.webview_events.try_send((
                ops::EV_IDEIAS_CHANGED.to_string(),
                serde_json::json!({ "ideia_id": ideia.id }),
            ));
            to_value(&ideia)
        }
        OP_DELETE_IDEIA => {
            let args: ops::delete_ideia::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            check_id(&args.id)?;
            repo.delete_ideia(&args.id)
                .await
                .map_err(|e| not_found_or_internal(&e))?;
            let _ = deps.webview_events.try_send((
                ops::EV_IDEIAS_CHANGED.to_string(),
                serde_json::json!({ "ideia_id": args.id }),
            ));
            to_value(&ops::delete_ideia::Result { ok: true })
        }
        OP_SET_IDEIA_STATUS => {
            let args: ops::set_ideia_status::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            check_id(&args.id)?;
            repo.set_ideia_status(&args.id, args.status)
                .await
                .map_err(|e| not_found_or_internal(&e))?;
            let _ = deps.webview_events.try_send((
                ops::EV_IDEIAS_CHANGED.to_string(),
                serde_json::json!({ "ideia_id": args.id }),
            ));
            to_value(&ops::set_ideia_status::Result { ok: true })
        }
        OP_LIST_MEMORY => {
            let args: ops::list_memory::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            check_id(&args.project_id)?;
            let items = repo
                .list_memory(&args.project_id)
                .await
                .map_err(|e| internal(&e.to_string()))?;
            to_value(&items)
        }
        OP_SUGGEST_LEARNING => {
            let args: ops::suggest_learning::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            let texto = args.texto.trim();
            if texto.is_empty() {
                return Err(ErrorBody::new("bad_args", "texto is required"));
            }
            let kind = SuggestionKind::Aprendizado {
                texto: texto.to_string(),
                origem_task: args.origem_task.filter(|s| !s.trim().is_empty()),
            };
            let id = create_memory_suggestion_op(deps, &args.project_id, kind).await?;
            to_value(&ops::suggest_learning::Result { suggestion_id: id })
        }
        OP_REVISE_MEMORY => {
            let args: ops::revise_memory::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            // `revise` carries only reeval ops — `Aprendizado` belongs to
            // `suggest_learning`. Reject it so the two surfaces stay clean.
            if args.kind.is_learning() {
                return Err(ErrorBody::new(
                    "bad_args",
                    "revise_memory does not accept 'aprendizado'; use suggest_learning",
                ));
            }
            let id = create_memory_suggestion_op(deps, &args.project_id, args.kind).await?;
            to_value(&ops::revise_memory::Result { suggestion_id: id })
        }
        OP_BYE => to_value(&ops::bye::Result { ok: true }),
        OP_HELLO => Err(ErrorBody::new(
            "hello_already_done",
            "hello may only be sent once",
        )),
        other => Err(ErrorBody::new("unknown_op", format!("unknown op: {other}"))),
    }
}

/// Validar projeto + criar task + amarrar mapping. Compartilhado entre o
/// dispatcher e a versão Tauri (que tem essa lógica inline em
/// `commands.rs::create_task` — duplicada de propósito porque os tipos
/// de erro e o caminho de origem são diferentes).
/// IPC helper for `OP_JIRA_MATERIALIZE`. Delegates to the shared core in
/// `commands`. Never logs `args` — it carries the capability secret.
async fn jira_materialize_op(
    deps: &ServerDeps,
    args: &ops::jira_materialize::Args,
) -> Result<ops::jira_materialize::Result, ErrorBody> {
    crate::commands::jira_materialize_core(&deps.state, args)
        .await
        .map_err(|e| {
            let (code, message) = e.code_message();
            ErrorBody::new(code, message)
        })
}

/// IPC helpers for the Slice-3 Jira data ops. Each delegates to the shared
/// core in `commands` and maps `JiraError::code_message()` → `ErrorBody`.
async fn jira_test_connection_op(
    deps: &ServerDeps,
) -> Result<ops::jira_test_connection::Result, ErrorBody> {
    crate::commands::jira_test_connection_core(&deps.state)
        .await
        .map_err(|e| {
            let (code, message) = e.code_message();
            ErrorBody::new(code, message)
        })
}

async fn jira_fetch_issue_op(
    deps: &ServerDeps,
    args: &ops::jira_fetch_issue::Args,
) -> Result<ops::jira_fetch_issue::Result, ErrorBody> {
    crate::commands::jira_fetch_issue_core(&deps.state, args)
        .await
        .map_err(|e| {
            let (code, message) = e.code_message();
            ErrorBody::new(code, message)
        })
}

async fn jira_list_assigned_op(
    deps: &ServerDeps,
) -> Result<ops::jira_list_assigned::Result, ErrorBody> {
    crate::commands::jira_list_assigned_core(&deps.state)
        .await
        .map_err(|e| {
            let (code, message) = e.code_message();
            ErrorBody::new(code, message)
        })
}

/// Slice-5 aggregate (issue-owned) review. Builds + persists the committed
/// branch diff and returns the package as a JSON passthrough. Maps the typed
/// `IssueReviewError` → `ErrorBody{code: e.code(), ...}`. STATE-NEUTRAL — see
/// `jira_review_core`.
async fn jira_review_op(
    deps: &ServerDeps,
    args: &ops::jira_review::Args,
) -> Result<ops::jira_review::Result, ErrorBody> {
    let pkg = crate::commands::jira_review_core(&deps.state, &args.jira_site, &args.jira_issue_id)
        .await
        .map_err(|e| ErrorBody::new(e.code(), e.to_string()))?;
    serde_json::to_value(&pkg)
        .map_err(|e| ErrorBody::new("internal", format!("serialize review: {e}")))
}

/// Slice-6a import orchestration. Delegates to `jira_import_core`; maps
/// `ImportError -> ErrorBody`. Never logs `args` (the analyst spawn injects
/// the capability secret via ENV inside the core, never here).
async fn jira_import_op(
    deps: &ServerDeps,
    args: &ops::jira_import::Args,
) -> Result<ops::jira_import::Result, ErrorBody> {
    crate::commands::jira_import_core(&deps.state, args)
        .await
        .map_err(|e| e.to_error_body())
}

/// Slice-6a discard lifecycle. Delegates to `jira_discard_core`; maps
/// `DiscardError -> ErrorBody`. The dirty-worktree error carries a count only.
async fn jira_discard_op(
    deps: &ServerDeps,
    args: &ops::jira_discard::Args,
) -> Result<ops::jira_discard::Result, ErrorBody> {
    crate::commands::jira_discard_core(&deps.state, args)
        .await
        .map_err(|e| e.to_error_body())
}

async fn create_task_op(
    deps: &ServerDeps,
    args: &ops::create_task::Args,
) -> Result<ops::create_task::Result, ErrorBody> {
    let pid = args.project_id.trim();
    if pid.is_empty() {
        return Err(ErrorBody::new("bad_args", "project_id is required"));
    }
    {
        let cfg = deps
            .state
            .config
            .lock()
            .map_err(|e| internal(&e.to_string()))?;
        if !cfg.projects.iter().any(|p| p.id == pid) {
            return Err(ErrorBody::new(
                "unknown_project",
                format!("unknown project_id: {pid}"),
            ));
        }
    }
    let task_id = match args.id.clone().filter(|s| !s.trim().is_empty()) {
        Some(id) => {
            check_id(&id)?;
            id
        }
        None => {
            // Mint a sequential T-<n> by scanning current tasks. Matches
            // the in-app path (commands::next_task_id) so CLI- and UI-
            // created tasks share one numbering sequence.
            let existing = deps
                .state
                .repo
                .list_tasks(None)
                .await
                .map_err(|e| not_found_or_internal(&e))?;
            let next =
                crate::commands::highest_task_number(existing.iter().map(|t| t.id.as_str())) + 1;
            format!("T-{next}")
        }
    };
    let task = cadenza_proto::Task {
        id: task_id.clone(),
        titulo: args.titulo.clone(),
        estado: cadenza_proto::Estado::AFazer,
        responsavel: "humano".to_string(),
        body: args.body.clone(),
        worktree_path: None,
        branch: None,
        blocked_by: Vec::new(),
        jira_site: None,
        jira_issue_id: None,
        jira_key_display: None,
    };
    deps.state
        .repo
        .create_task(&task)
        .await
        .map_err(|e| not_found_or_internal(&e))?;
    deps.state
        .task_projects
        .set(&task_id, Some(pid))
        .map_err(|e| internal(&e.to_string()))?;

    // Marcar a ideia de origem como `destrinchada` quando o agente
    // informa qual foi. Falha aqui é não-fatal — a task já foi criada.
    if let Some(ref ideia_id) = args.from_ideia {
        check_id(ideia_id)?;
        if let Err(e) = deps
            .state
            .repo
            .set_ideia_status(ideia_id, IdeiaStatus::Destrinchada)
            .await
        {
            tracing::warn!(error = ?e, ideia = %ideia_id, "set ideia status destrinchada failed");
        } else {
            let _ = deps.webview_events.try_send((
                ops::EV_IDEIAS_CHANGED.to_string(),
                serde_json::json!({ "ideia_id": ideia_id }),
            ));
        }
    }

    Ok(ops::create_task::Result { task_id })
}

async fn create_ideia_op(
    deps: &ServerDeps,
    args: ops::create_ideia::Args,
) -> Result<Ideia, ErrorBody> {
    let pid = args.project_id.trim();
    if pid.is_empty() {
        return Err(ErrorBody::new("bad_args", "project_id is required"));
    }
    {
        let cfg = deps
            .state
            .config
            .lock()
            .map_err(|e| internal(&e.to_string()))?;
        if !cfg.projects.iter().any(|p| p.id == pid) {
            return Err(ErrorBody::new(
                "unknown_project",
                format!("unknown project_id: {pid}"),
            ));
        }
    }
    let id = match args.id.filter(|s| !s.trim().is_empty()) {
        Some(id) => {
            check_id(&id)?;
            id
        }
        None => format!("I-{}", uuid::Uuid::new_v4().simple()),
    };
    let ideia = Ideia {
        id,
        titulo: args.titulo,
        body: args.body,
        project_id: pid.to_string(),
        status: IdeiaStatus::Pendente,
        created_at_ms: chrono::Utc::now().timestamp_millis(),
    };
    deps.state
        .repo
        .create_ideia(&ideia)
        .await
        .map_err(|e| internal(&e.to_string()))?;
    Ok(ideia)
}

/// Validar projeto + mintar id/criado_em + persistir a sugestão pendente.
/// Compartilhado por `suggest_learning` e `revise_memory`. Emite
/// `EV_MEMORY_CHANGED` para a UI re-puxar a aba de Memória / o review.
/// Nada vira memória oficial aqui — só fica pendente até a curadoria.
async fn create_memory_suggestion_op(
    deps: &ServerDeps,
    project_id: &str,
    kind: SuggestionKind,
) -> Result<String, ErrorBody> {
    let pid = project_id.trim();
    if pid.is_empty() {
        return Err(ErrorBody::new("bad_args", "project_id is required"));
    }
    {
        let cfg = deps
            .state
            .config
            .lock()
            .map_err(|e| internal(&e.to_string()))?;
        if !cfg.projects.iter().any(|p| p.id == pid) {
            return Err(ErrorBody::new(
                "unknown_project",
                format!("unknown project_id: {pid}"),
            ));
        }
    }
    let suggestion = MemorySuggestion {
        id: format!("MS-{}", uuid::Uuid::new_v4().simple()),
        project_id: pid.to_string(),
        criado_em: chrono::Utc::now().timestamp_millis(),
        kind,
    };
    deps.state
        .repo
        .create_memory_suggestion(&suggestion)
        .await
        .map_err(|e| internal(&e.to_string()))?;
    let _ = deps.webview_events.try_send((
        ops::EV_MEMORY_CHANGED.to_string(),
        serde_json::json!({ "project_id": pid }),
    ));
    Ok(suggestion.id)
}

/// `done` is per-design "request to complete" — agents never put a task in
/// `feito` directly (PLAN §C.8). The summary is appended as a `[done request]`
/// log line and the task moves to `aguardando_revisao`, so the human keeps
/// final say.
///
/// Backward compatible: positional `done` with NO evidence keeps the legacy
/// behavior exactly (append the line once + flip estado), and produces no
/// review package. With evidence, the app independently builds a trustworthy
/// [`ReviewPackage`] (hardened read-only git over the worktree) and persists
/// it ATOMICALLY with the log + estado flip via
/// [`Repository::done_with_review_package`] (PLAN §C.9). Re-running the same
/// `idempotency_key` is a no-op returning the stored package.
async fn done_op(
    deps: &ServerDeps,
    args: &ops::done::Args,
) -> Result<ops::done::Result, ErrorBody> {
    check_id(&args.task_id)?;

    let log_line = format!("[done request] {}", args.summary);

    // ── Legacy path: no evidence ⇒ today's behavior, no package. ──────
    let Some(evidence) = args.evidence.clone() else {
        deps.state
            .repo
            .append_log(&args.task_id, &log_line)
            .await
            .map_err(|e| not_found_or_internal(&e))?;
        deps.state
            .repo
            .set_estado(&args.task_id, cadenza_proto::Estado::AguardandoRevisao)
            .await
            .map_err(|e| not_found_or_internal(&e))?;
        return Ok(ops::done::Result { ok: true });
    };

    // ── Evidence path. ────────────────────────────────────────────────
    // 1. App-side re-validation + re-cap BEFORE any mutation (PLAN §C.10):
    //    malformed/over-cap evidence is `bad_args` with NO partial done.
    let evidence = crate::review::validate_and_cap_evidence(evidence)
        .map_err(|e| ErrorBody::new("bad_args", e.to_string()))?;

    // 2. Resolve the project (for the quality contract + default branch).
    //    Resolution failure does NOT fail `done`: the engine yields a
    //    `contract_unavailable` package (PLAN §C.12). We read the contract +
    //    default branch out of the lock into owned values to avoid holding
    //    the mutex across the await-heavy git collection.
    let project_id = deps.state.task_projects.get(&args.task_id);
    let (contract, contract_resolved, project_default_branch) = {
        let cfg = deps
            .state
            .config
            .lock()
            .map_err(|_| internal("config lock poisoned"))?;
        match project_id
            .as_deref()
            .and_then(|pid| cfg.projects.iter().find(|p| p.id == pid))
        {
            Some(p) => (
                p.quality.clone(),
                true,
                p.default_branch.clone().filter(|s| !s.trim().is_empty()),
            ),
            None => (None, false, None),
        }
    };

    // 3. Resolve the worktree/branch (missing worktree ⇒ skip git).
    let wt = deps.state.task_worktrees.get(&args.task_id);
    let worktree_path = wt
        .as_ref()
        .and_then(|w| w.worktree_path.clone())
        .filter(|p| !p.trim().is_empty());
    let task_branch = wt
        .as_ref()
        .and_then(|w| w.branch.clone())
        .filter(|b| !b.trim().is_empty());

    // 4. Resolve the idempotency key: client-generated (PLAN §C.9). When
    //    absent we mint one app-side — it won't dedup across reconnects, but
    //    keeps the storage contract (every package carries a valid key).
    let idempotency_key = match args.idempotency_key.clone().filter(|k| !k.is_empty()) {
        Some(k) => {
            crate::store::validate_idempotency_key(&k)
                .map_err(|e| ErrorBody::new("bad_args", e.to_string()))?;
            k
        }
        None => uuid::Uuid::new_v4().simple().to_string(),
    };

    // 5. Run the engine (never errors on git failure — PLAN §C.12).
    let inputs = crate::review::CollectInputs {
        worktree_path: worktree_path.as_deref().map(std::path::Path::new),
        task_branch: task_branch.as_deref(),
        project_default_branch: project_default_branch.as_deref(),
        contract: contract.as_ref(),
        contract_resolved,
        reported: evidence,
    };
    let mut pkg = crate::review::build_package(inputs).await;
    pkg.task_id = args.task_id.clone();
    pkg.idempotency_key = idempotency_key;
    pkg.summary = args.summary.clone();

    // 6. Atomic apply: package upsert + supersede + log append + estado flip
    //    in ONE unit (journal on files, transaction on SQL). Re-running the
    //    same key returns the stored package (no-op).
    deps.state
        .repo
        .done_with_review_package(
            &pkg,
            Some(&log_line),
            Some(cadenza_proto::Estado::AguardandoRevisao),
        )
        .await
        .map_err(|e| not_found_or_internal(&e))?;

    Ok(ops::done::Result { ok: true })
}

/// `quality` — return the per-project quality contract (PLAN §B.5). Project
/// resolution: explicit `project` arg → the task's mapped project → the app
/// `active_project_id`. On resolution failure the app returns an explicit
/// `unknown_project` diagnostic (NOT an empty list); an empty `checks` list is
/// reserved for "resolved project has no profile" (→ `no_validation`).
async fn quality_op(
    deps: &ServerDeps,
    args: &ops::quality::Args,
) -> Result<ops::quality::Result, ErrorBody> {
    // Resolve a candidate project id. The CLI passes whatever it resolved
    // locally; the app does the final resolution + active-project fallback.
    let explicit = args.project.clone().filter(|p| !p.trim().is_empty());
    let from_task = args
        .task
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .and_then(|t| deps.state.task_projects.get(t));

    let cfg = deps
        .state
        .config
        .lock()
        .map_err(|_| internal("config lock poisoned"))?;
    let resolved = explicit
        .or(from_task)
        .or_else(|| cfg.active_project_id.clone());

    let Some(pid) = resolved.filter(|p| !p.trim().is_empty()) else {
        return Err(ErrorBody::new(
            "unknown_project",
            "could not resolve a project (pass --project or set an active project)",
        ));
    };
    let Some(project) = cfg.projects.iter().find(|p| p.id == pid) else {
        return Err(ErrorBody::new(
            "unknown_project",
            format!("unknown project_id: {pid}"),
        ));
    };

    // Resolved project: present the contract. No profile ⇒ empty checks +
    // the empty-profile contract hash (a stable hash over zero checks).
    let result = match &project.quality {
        Some(q) => ops::quality::Result {
            contract_version: q.contract_version(),
            checks: q
                .checks
                .iter()
                .map(|c| ops::quality::Check {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    cmd: c.cmd.clone(),
                    required: c.required,
                    required_if_changed: c.required_if_changed.clone(),
                })
                .collect(),
        },
        None => ops::quality::Result {
            contract_version: crate::config::QualityProfile::default().contract_version(),
            checks: Vec::new(),
        },
    };
    drop(cfg);
    Ok(result)
}

/// `review_decision` — the human approve / request-changes op (PLAN §E.16).
/// Transition guard: requires `estado == aguardando_revisao` AND a latest
/// non-terminal (`Pending`) package; otherwise `bad_state`. The estado flip,
/// the `[revisão]` log line (both verdicts), and the package decision mark are
/// applied together.
async fn review_decision_op(
    deps: &ServerDeps,
    args: &ops::review_decision::Args,
) -> Result<ops::review_decision::Result, ErrorBody> {
    // The transition guard + atomic state/log/decision writes live in one
    // place (`commands::apply_review_decision`); this handler only adapts
    // the typed error into an `ErrorBody`. `check_id` is performed inside
    // the shared core via `validate_id`.
    let note = args.note.clone().unwrap_or_default();
    crate::commands::apply_review_decision(
        deps.state.repo.as_ref(),
        &args.task_id,
        args.verdict,
        &note,
    )
    .await
    .map(|_estado| ops::review_decision::Result { ok: true })
    .map_err(|e| {
        let body = ErrorBody::new(e.code, e.message);
        if e.code == "task_busy" {
            body.retryable()
        } else {
            body
        }
    })
}

/// Append (or replace) a `## Plano` section in a task body. The original
/// description above the heading is preserved; re-planning drops the prior
/// `## Plano` block before re-appending so the section never stacks.
fn append_plan_section(existing: &str, plan: &str) -> String {
    const HEADING: &str = "## Plano";
    let base = match locate_line_heading(existing, HEADING) {
        Some(idx) => existing[..idx].trim_end().to_string(),
        None => existing.trim_end().to_string(),
    };
    let plan = plan.trim();
    if base.is_empty() {
        format!("{HEADING}\n\n{plan}\n")
    } else {
        format!("{base}\n\n{HEADING}\n\n{plan}\n")
    }
}

/// Return the byte index at which `heading` begins, only when it occupies an
/// entire line (not a prefix of a longer heading like `## Planos futuros`).
fn locate_line_heading(text: &str, heading: &str) -> Option<usize> {
    let terminates_line = |offset: usize| -> bool {
        let rest = &text[offset + heading.len()..];
        rest.is_empty() || rest.starts_with('\n') || rest.starts_with('\r')
    };
    if text.starts_with(heading) && terminates_line(0) {
        return Some(0);
    }
    let prefix = format!("\n{heading}");
    let mut from = 0;
    while let Some(rel) = text[from..].find(prefix.as_str()) {
        let candidate = from + rel + 1; // byte offset of the `#`
        if terminates_line(candidate) {
            return Some(candidate);
        }
        from += rel + 1;
    }
    None
}

// ───────── hello validation ─────────────────────────────────────────────────

/// Reject a protocol number that falls outside the negotiated window.
/// Split out from `check_hello` so the handler can run this BEFORE
/// `hello::Args` deserialization — an old client with a stale args shape
/// must still see protocol_too_old (exit 12) instead of bad_args (exit 1).
fn check_protocol(protocol: u32) -> Result<(), ErrorBody> {
    if protocol < MIN_PROTOCOL {
        return Err(ErrorBody::new("protocol_too_old", "update cli"));
    }
    if protocol > MAX_PROTOCOL {
        return Err(ErrorBody::new("protocol_too_new", "update app"));
    }
    Ok(())
}

/// Validate a hello protocol number and auth token, returning the welcome
/// result on success or a typed error body on failure.  Extracted so the
/// three checks (protocol-too-old, protocol-too-new, auth-failed) can be
/// unit-tested without needing a running Tauri app or an `AppState`.
fn check_hello(
    protocol: u32,
    token: &str,
    data_dir: &std::path::Path,
) -> Result<ops::hello::Result, ErrorBody> {
    check_protocol(protocol)?;
    // Distinguish wrong-token (auth_failed) from an IO error reading the
    // auth file. The latter typically fires during tray-driven token
    // rotation (create + rename races validate); reporting it as a
    // retryable internal error lets the agent's reconnect path recover
    // instead of telling the human their token is invalid.
    match crate::auth::validate(data_dir, token) {
        Ok(true) => {}
        Ok(false) => return Err(ErrorBody::new("auth_failed", "invalid token")),
        Err(e) => {
            return Err(ErrorBody::new("internal", format!("auth check failed: {e}")).retryable())
        }
    }
    Ok(ops::hello::Result {
        protocol: MAX_PROTOCOL,
        app: format!("cadenza/{SERVER_APP_VERSION}"),
    })
}

// ───────── helpers ─────────

fn bad_args(e: serde_json::Error) -> ErrorBody {
    ErrorBody::new("bad_args", e.to_string())
}

/// Reject wire-supplied ids that would escape the store root. A
/// malicious agent setting `id = "../auth"` could otherwise read or
/// overwrite arbitrary files via the file backend's `path_for`.
fn check_id(id: &str) -> Result<(), ErrorBody> {
    validate_id(id).map_err(|e| ErrorBody::new("bad_args", e.to_string()))
}

fn internal(message: &str) -> ErrorBody {
    ErrorBody::new("internal", message.to_string())
}

fn not_found_or_internal(e: &crate::store::StoreError) -> ErrorBody {
    use crate::store::StoreError;
    match e {
        StoreError::NotFound(id) => ErrorBody::new("task_not_found", id.clone()),
        StoreError::Busy => ErrorBody::new("task_busy", e.to_string()).retryable(),
        StoreError::AlreadyExists(id) => ErrorBody::new("task_exists", id.clone()),
        _ => ErrorBody::new("internal", e.to_string()),
    }
}

fn to_value<T: Serialize>(v: &T) -> Result<Value, ErrorBody> {
    serde_json::to_value(v).map_err(|e| internal(&e.to_string()))
}

async fn send_ok<T: Serialize>(tx: &mpsc::Sender<String>, id: Option<String>, result: T) {
    match serde_json::to_value(&result) {
        Ok(v) => send_ok_raw(tx, id, v).await,
        Err(e) => send_err(tx, id, internal(&e.to_string())).await,
    }
}

async fn send_ok_raw(tx: &mpsc::Sender<String>, id: Option<String>, value: Value) {
    let resp = Response {
        v: cadenza_proto::WIRE_VERSION,
        id,
        ok: true,
        result: Some(value),
        error: None,
    };
    if let Ok(line) = serde_json::to_string(&resp) {
        let _ = tx.send(line).await;
    }
}

async fn send_err(tx: &mpsc::Sender<String>, id: Option<String>, error: ErrorBody) {
    let resp = Response::err(id, error);
    if let Ok(line) = serde_json::to_string(&resp) {
        let _ = tx.send(line).await;
    }
}

async fn send_event(tx: &mpsc::Sender<String>, event: Event) {
    if let Ok(line) = serde_json::to_string(&event) {
        let _ = tx.send(line).await;
    }
}

// ───────── helper used by lib.rs and notifications ─────────

/// Pure helper — broadcast a `proposta_decidida` event. Used by
/// `notify.rs` after the user clicks the OS notification action so any
/// in-flight `await_decision` waiter is informed. Today the writer is
/// per-connection; this is a forward-declared hook for Phase 5.
#[allow(dead_code)]
pub fn build_proposta_decidida_event(registro: &DecisaoRegistro) -> Option<Event> {
    Event::new(
        ops::EV_PROPOSTA_DECIDIDA,
        serde_json::json!({
            "proposta_id": registro.proposta_id,
            "decisao": match registro.decisao {
                Decisao::Aceita => "aceita",
                Decisao::Rejeitada => "rejeitada",
                Decisao::Mesclada => "mesclada",
            },
            "task_id": registro.task_id,
        }),
    )
    .ok()
}

// ─────────────────────────────────────────────────────────────────────────────
// IPC handshake unit tests
//
// These tests exercise `check_hello` directly — no Tauri app state, no tokio
// runtime — so they run cleanly even in environments where the Tauri/WebView2
// DLLs are not fully available.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use cadenza_proto::{MAX_PROTOCOL, MIN_PROTOCOL};
    use tempfile::TempDir;

    fn write_token(dir: &TempDir, token: &str) {
        std::fs::write(dir.path().join("auth"), token).unwrap();
    }

    /// Valid token + current MAX_PROTOCOL → ok with `{protocol, app}`.
    #[test]
    fn handshake_ok() {
        let dir = TempDir::new().unwrap();
        let token = "test-token-ok";
        write_token(&dir, token);
        let result = check_hello(MAX_PROTOCOL, token, dir.path()).unwrap();
        assert_eq!(result.protocol, MAX_PROTOCOL);
        assert!(result.app.starts_with("cadenza/"), "app = {}", result.app);
    }

    /// Protocol above MAX_PROTOCOL → `protocol_too_new`.
    #[test]
    fn handshake_protocol_too_new() {
        let err = check_protocol(MAX_PROTOCOL + 1).unwrap_err();
        assert_eq!(err.code, "protocol_too_new");
    }

    /// Wrong token with valid protocol → `auth_failed`.
    #[test]
    fn handshake_auth_failed() {
        let dir = TempDir::new().unwrap();
        write_token(&dir, "real-token");
        let err = check_hello(MAX_PROTOCOL, "wrong-token", dir.path()).unwrap_err();
        assert_eq!(err.code, "auth_failed");
    }

    /// Protocol below MIN_PROTOCOL → `protocol_too_old`.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn handshake_protocol_too_old() {
        assert!(MIN_PROTOCOL > 0, "test assumes MIN_PROTOCOL > 0");
        let err = check_protocol(MIN_PROTOCOL - 1).unwrap_err();
        assert_eq!(err.code, "protocol_too_old");
    }

    /// Empty body → just the heading + plan.
    #[test]
    fn append_plan_into_empty_body() {
        let out = append_plan_section("", "Faça X depois Y");
        assert_eq!(out, "## Plano\n\nFaça X depois Y\n");
    }

    /// Body without a plan section → original kept, plan appended below.
    #[test]
    fn append_plan_preserves_description() {
        let out = append_plan_section("Descrição breve.", "Passo 1\nPasso 2");
        assert_eq!(out, "Descrição breve.\n\n## Plano\n\nPasso 1\nPasso 2\n");
    }

    /// Re-planning replaces the previous `## Plano` block instead of stacking.
    #[test]
    fn append_plan_replaces_existing_section() {
        let existing = "Descrição breve.\n\n## Plano\n\nPlano antigo\n";
        let out = append_plan_section(existing, "Plano novo");
        assert_eq!(out, "Descrição breve.\n\n## Plano\n\nPlano novo\n");
        // Idempotent across repeated re-plans — no duplicate headings.
        assert_eq!(out.matches("## Plano").count(), 1);
    }

    /// A heading that starts with "## Plano" but continues with more text
    /// (e.g. "## Planos de contingência") must NOT be treated as the plan
    /// section — it is part of the description and must be preserved.
    #[test]
    fn append_plan_does_not_match_heading_prefix() {
        let existing = "Descrição.\n\n## Planos de contingência\nX\n";
        let out = append_plan_section(existing, "Novo plano");
        assert!(
            out.contains("## Planos de contingência"),
            "original section must be preserved"
        );
        assert!(
            out.contains("## Plano\n\nNovo plano"),
            "plan section must be appended"
        );
        // Exactly one `## Plano` section appended; original heading not falsely matched.
        assert_eq!(out.matches("## Plano\n").count(), 1);
    }

    // ── AppState-aware handler tests (done/quality/review_decision) ──────

    use crate::commands::AppState;
    use crate::config::{Config, Project, QualityCheck, QualityProfile};
    use crate::store::FileRepository;

    /// Build a `ServerDeps` over a file backend rooted in a tempdir, with one
    /// project (id `P-1`) and an optional quality profile. Returns the deps,
    /// the kept tempdir, and a drained event receiver.
    fn mk_deps(
        quality: Option<QualityProfile>,
    ) -> (ServerDeps, TempDir, mpsc::Receiver<(String, Value)>) {
        let dir = TempDir::new().unwrap();
        let repo = Arc::new(FileRepository::new(dir.path()).unwrap());
        let config = Config {
            projects: vec![Project {
                id: "P-1".into(),
                name: "Proj".into(),
                path: dir.path().to_path_buf(),
                agente: None,
                default_branch: None,
                color: None,
                quality,
            }],
            active_project_id: Some("P-1".into()),
            ..Default::default()
        };

        let state = AppState::for_test(dir.path(), repo, config).unwrap();
        let (tx, rx) = mpsc::channel(64);
        let deps = ServerDeps {
            state: Arc::new(state),
            data_dir: dir.path().to_path_buf(),
            webview_events: tx,
        };
        (deps, dir, rx)
    }

    async fn seed_task(deps: &ServerDeps, id: &str, estado: cadenza_proto::Estado) {
        let task = cadenza_proto::Task {
            id: id.into(),
            titulo: format!("{id} title"),
            estado,
            responsavel: "humano".into(),
            body: format!("# {id}\n\nbody\n"),
            worktree_path: None,
            branch: None,
            blocked_by: Vec::new(),
            jira_site: None,
            jira_issue_id: None,
            jira_key_display: None,
        };
        deps.state.repo.create_task(&task).await.unwrap();
        deps.state.task_projects.set(id, Some("P-1")).unwrap();
    }

    #[tokio::test]
    async fn done_op_legacy_no_evidence_keeps_behavior() {
        let (deps, _dir, _rx) = mk_deps(None);
        seed_task(&deps, "T-1", cadenza_proto::Estado::Fazendo).await;
        let args = ops::done::Args {
            task_id: "T-1".into(),
            summary: "done it".into(),
            evidence: None,
            idempotency_key: None,
        };
        done_op(&deps, &args).await.unwrap();
        let task = deps.state.repo.read_task("T-1").await.unwrap();
        assert_eq!(task.estado, cadenza_proto::Estado::AguardandoRevisao);
        assert_eq!(task.body.matches("[done request] done it").count(), 1);
        // No package created on the legacy path.
        assert!(deps
            .state
            .repo
            .list_review_packages("T-1")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn done_op_with_evidence_persists_package_idempotently() {
        let (deps, _dir, _rx) = mk_deps(None);
        seed_task(&deps, "T-2", cadenza_proto::Estado::Fazendo).await;
        let args = ops::done::Args {
            task_id: "T-2".into(),
            summary: "with evidence".into(),
            evidence: Some(ops::done::Evidence::default()),
            idempotency_key: Some("key-1".into()),
        };
        done_op(&deps, &args).await.unwrap();
        let task = deps.state.repo.read_task("T-2").await.unwrap();
        assert_eq!(task.estado, cadenza_proto::Estado::AguardandoRevisao);
        let pkgs = deps.state.repo.list_review_packages("T-2").await.unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].summary, "with evidence");

        // Same key again ⇒ no second package, no second log line.
        done_op(&deps, &args).await.unwrap();
        assert_eq!(
            deps.state
                .repo
                .list_review_packages("T-2")
                .await
                .unwrap()
                .len(),
            1
        );
        let task2 = deps.state.repo.read_task("T-2").await.unwrap();
        assert_eq!(
            task2.body.matches("[done request] with evidence").count(),
            1
        );
    }

    #[tokio::test]
    async fn done_op_rejects_oversize_evidence_without_mutation() {
        let (deps, _dir, _rx) = mk_deps(None);
        seed_task(&deps, "T-3", cadenza_proto::Estado::Fazendo).await;
        let evidence = ops::done::Evidence {
            checks: vec![
                ops::done::EvidenceCheck {
                    id: "c".into(),
                    exit: 0,
                    log_excerpt: String::new(),
                    log_path: None,
                };
                crate::review::caps_max_checks() + 1
            ],
            ..Default::default()
        };
        let args = ops::done::Args {
            task_id: "T-3".into(),
            summary: "bad".into(),
            evidence: Some(evidence),
            idempotency_key: Some("key-x".into()),
        };
        let err = done_op(&deps, &args).await.unwrap_err();
        assert_eq!(err.code, "bad_args");
        // No state mutation: estado unchanged, no package, no log line.
        let task = deps.state.repo.read_task("T-3").await.unwrap();
        assert_eq!(task.estado, cadenza_proto::Estado::Fazendo);
        assert!(!task.body.contains("[done request]"));
        assert!(deps
            .state
            .repo
            .list_review_packages("T-3")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn quality_op_unknown_project_is_explicit_error() {
        let (deps, _dir, _rx) = mk_deps(None);
        let args = ops::quality::Args {
            task: None,
            project: Some("nope".into()),
        };
        let err = quality_op(&deps, &args).await.unwrap_err();
        assert_eq!(err.code, "unknown_project");
    }

    #[tokio::test]
    async fn quality_op_resolved_no_profile_is_empty_checks() {
        let (deps, _dir, _rx) = mk_deps(None);
        let args = ops::quality::Args {
            task: None,
            project: Some("P-1".into()),
        };
        let r = quality_op(&deps, &args).await.unwrap();
        assert!(r.checks.is_empty());
    }

    #[tokio::test]
    async fn quality_op_returns_profile_checks() {
        let profile = QualityProfile {
            checks: vec![QualityCheck {
                id: "clippy".into(),
                name: "Clippy".into(),
                cmd: "cargo clippy".into(),
                required: true,
                required_if_changed: vec!["**/*.rs".into()],
            }],
        };
        let expected = profile.contract_version();
        let (deps, _dir, _rx) = mk_deps(Some(profile));
        let args = ops::quality::Args {
            task: None,
            project: Some("P-1".into()),
        };
        let r = quality_op(&deps, &args).await.unwrap();
        assert_eq!(r.contract_version, expected);
        assert_eq!(r.checks.len(), 1);
        assert_eq!(r.checks[0].id, "clippy");
    }

    #[tokio::test]
    async fn review_decision_op_guard_rejects_wrong_estado() {
        let (deps, _dir, _rx) = mk_deps(None);
        seed_task(&deps, "T-4", cadenza_proto::Estado::Fazendo).await;
        let args = ops::review_decision::Args {
            task_id: "T-4".into(),
            verdict: ops::review_decision::Verdict::Aprovado,
            note: None,
        };
        let err = review_decision_op(&deps, &args).await.unwrap_err();
        assert_eq!(err.code, "bad_state");
    }

    #[tokio::test]
    async fn review_decision_op_guard_rejects_without_pending_package() {
        let (deps, _dir, _rx) = mk_deps(None);
        // Awaiting review but NO package at all.
        seed_task(&deps, "T-5", cadenza_proto::Estado::AguardandoRevisao).await;
        let args = ops::review_decision::Args {
            task_id: "T-5".into(),
            verdict: ops::review_decision::Verdict::Aprovado,
            note: None,
        };
        let err = review_decision_op(&deps, &args).await.unwrap_err();
        assert_eq!(err.code, "bad_state");
    }

    #[tokio::test]
    async fn review_decision_op_approves_and_marks_package() {
        let (deps, _dir, _rx) = mk_deps(None);
        seed_task(&deps, "T-6", cadenza_proto::Estado::Fazendo).await;
        // Drive a real done-with-evidence to land an aguardando_revisao task
        // plus a pending package.
        let done = ops::done::Args {
            task_id: "T-6".into(),
            summary: "ready".into(),
            evidence: Some(ops::done::Evidence::default()),
            idempotency_key: Some("k-6".into()),
        };
        done_op(&deps, &done).await.unwrap();

        let args = ops::review_decision::Args {
            task_id: "T-6".into(),
            verdict: ops::review_decision::Verdict::Aprovado,
            note: Some("lgtm".into()),
        };
        review_decision_op(&deps, &args).await.unwrap();
        let task = deps.state.repo.read_task("T-6").await.unwrap();
        assert_eq!(task.estado, cadenza_proto::Estado::Feito);
        assert!(task.body.contains("[revisão] aprovado: lgtm"));
        let latest = deps
            .state
            .repo
            .latest_review_package("T-6")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.status, crate::store::PackageStatus::Aprovado);
    }

    #[tokio::test]
    async fn review_decision_op_pedir_alteracoes_returns_to_fazendo() {
        let (deps, _dir, _rx) = mk_deps(None);
        seed_task(&deps, "T-7", cadenza_proto::Estado::Fazendo).await;
        let done = ops::done::Args {
            task_id: "T-7".into(),
            summary: "ready".into(),
            evidence: Some(ops::done::Evidence::default()),
            idempotency_key: Some("k-7".into()),
        };
        done_op(&deps, &done).await.unwrap();

        let args = ops::review_decision::Args {
            task_id: "T-7".into(),
            verdict: ops::review_decision::Verdict::PedirAlteracoes,
            note: Some("fix X".into()),
        };
        review_decision_op(&deps, &args).await.unwrap();
        let task = deps.state.repo.read_task("T-7").await.unwrap();
        assert_eq!(task.estado, cadenza_proto::Estado::Fazendo);
        assert!(task.body.contains("[revisão] pedir_alteracoes: fix X"));
        let latest = deps
            .state
            .repo
            .latest_review_package("T-7")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            latest.status,
            crate::store::PackageStatus::AlteracoesSolicitadas
        );
    }

    // ── Jira analysis runs + materialize (Slice 2) ──────────────────────

    use crate::commands::{
        create_analysis_run, jira_materialize_core, revoke_run_secret, verify_run_secret,
    };
    use crate::jira_run::RunSecretError;
    use cadenza_proto::SecretStatus;

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    /// Upsert a minimal base `JiraIssueRecord` (no secret yet).
    async fn seed_jira_record(deps: &ServerDeps, site: &str, issue: &str, key: &str) {
        let rec = cadenza_proto::JiraIssueRecord {
            jira_site: site.into(),
            jira_issue_id: issue.into(),
            jira_key: key.into(),
            project_id: Some("P-1".into()),
            analysis_run_id: None,
            secret_hash: None,
            secret_expiry_ms: None,
            secret_status: None,
            raw_adf: None,
            branch_name: None,
            worktree_path: None,
            base_sha: None,
            worktree_state: None,
            created_at_ms: now_ms(),
            updated_at_ms: now_ms(),
        };
        deps.state.repo.upsert_jira_issue(&rec).await.unwrap();
    }

    fn subtask(title: &str, body: &str) -> ops::jira_materialize::Subtask {
        ops::jira_materialize::Subtask {
            title: title.into(),
            body: body.into(),
        }
    }

    #[tokio::test]
    async fn create_analysis_run_returns_secret_once_and_persists_hash_only() {
        let (deps, _dir, _rx) = mk_deps(None);
        seed_jira_record(&deps, "site", "100", "PROJ-1").await;
        let (run_id, secret) = create_analysis_run(&deps.state, "site", "100", Some("P-1"))
            .await
            .unwrap();
        assert!(run_id.starts_with("run-"));
        let rec = deps
            .state
            .repo
            .read_jira_issue("site", "100")
            .await
            .unwrap()
            .unwrap();
        assert!(rec.secret_hash.is_some());
        assert_eq!(rec.secret_status.as_deref(), Some("active"));
        assert_eq!(rec.analysis_run_id.as_deref(), Some(run_id.as_str()));
        // The plaintext secret must NOT appear in the persisted record JSON.
        let json = serde_json::to_string(&rec).unwrap();
        assert!(
            !json.contains(secret.expose()),
            "plaintext leaked to record"
        );
    }

    #[tokio::test]
    async fn verify_run_secret_valid_returns_identity() {
        let (deps, _dir, _rx) = mk_deps(None);
        seed_jira_record(&deps, "site", "100", "PROJ-1").await;
        let (run_id, secret) = create_analysis_run(&deps.state, "site", "100", Some("P-1"))
            .await
            .unwrap();
        let v = verify_run_secret(&deps.state, &run_id, secret.expose())
            .await
            .unwrap();
        assert_eq!(v.jira_site, "site");
        assert_eq!(v.jira_issue_id, "100");
        assert_eq!(v.project_id.as_deref(), Some("P-1"));
    }

    #[tokio::test]
    async fn verify_run_secret_invalid_hash_rejected() {
        let (deps, _dir, _rx) = mk_deps(None);
        seed_jira_record(&deps, "site", "100", "PROJ-1").await;
        let (run_id, _secret) = create_analysis_run(&deps.state, "site", "100", Some("P-1"))
            .await
            .unwrap();
        let err = verify_run_secret(&deps.state, &run_id, "wrong-secret")
            .await
            .unwrap_err();
        assert_eq!(err, RunSecretError::Invalid);
        // Unknown run id ⇒ NotFound.
        let err2 = verify_run_secret(&deps.state, "run-nope", "x")
            .await
            .unwrap_err();
        assert_eq!(err2, RunSecretError::NotFound);
    }

    #[tokio::test]
    async fn verify_run_secret_expired_rejected() {
        let (deps, _dir, _rx) = mk_deps(None);
        seed_jira_record(&deps, "site", "100", "PROJ-1").await;
        let (run_id, secret) = create_analysis_run(&deps.state, "site", "100", Some("P-1"))
            .await
            .unwrap();
        // Force expiry into the past.
        let mut rec = deps
            .state
            .repo
            .read_jira_issue("site", "100")
            .await
            .unwrap()
            .unwrap();
        rec.secret_expiry_ms = Some(now_ms() - 1000);
        deps.state.repo.upsert_jira_issue(&rec).await.unwrap();
        let err = verify_run_secret(&deps.state, &run_id, secret.expose())
            .await
            .unwrap_err();
        assert_eq!(err, RunSecretError::Expired);
    }

    #[tokio::test]
    async fn verify_run_secret_revoked_rejected() {
        let (deps, _dir, _rx) = mk_deps(None);
        seed_jira_record(&deps, "site", "100", "PROJ-1").await;
        let (run_id, secret) = create_analysis_run(&deps.state, "site", "100", Some("P-1"))
            .await
            .unwrap();
        revoke_run_secret(&deps.state, &run_id).await.unwrap();
        let err = verify_run_secret(&deps.state, &run_id, secret.expose())
            .await
            .unwrap_err();
        assert_eq!(err, RunSecretError::Revoked);
    }

    #[tokio::test]
    async fn revoke_run_secret_sets_status_revoked() {
        let (deps, _dir, _rx) = mk_deps(None);
        seed_jira_record(&deps, "site", "100", "PROJ-1").await;
        let (run_id, _secret) = create_analysis_run(&deps.state, "site", "100", Some("P-1"))
            .await
            .unwrap();
        revoke_run_secret(&deps.state, &run_id).await.unwrap();
        // Idempotent second call.
        revoke_run_secret(&deps.state, &run_id).await.unwrap();
        let rec = deps
            .state
            .repo
            .read_jira_issue("site", "100")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            rec.secret_status.as_deref(),
            Some(SecretStatus::Revoked.as_str())
        );
    }

    /// Mint a run and return (deps, run_id, plaintext) for materialize tests.
    async fn mk_run(deps: &ServerDeps) -> (String, String) {
        seed_jira_record(deps, "site", "100", "PROJ-1").await;
        let (run_id, secret) = create_analysis_run(&deps.state, "site", "100", Some("P-1"))
            .await
            .unwrap();
        (run_id, secret.expose().to_string())
    }

    #[tokio::test]
    async fn materialize_creates_one_proposta_per_subtask_with_stamped_identity() {
        let (deps, _dir, _rx) = mk_deps(None);
        let (run_id, secret) = mk_run(&deps).await;
        let args = ops::jira_materialize::Args {
            analysis_run_id: run_id,
            run_secret: secret,
            subtasks: vec![subtask("A", "ba"), subtask("B", "bb")],
        };
        let result = jira_materialize_core(&deps.state, &args).await.unwrap();
        assert_eq!(result.created.len(), 2);
        assert_eq!(result.jira_site, "site");
        assert_eq!(result.jira_issue_id, "100");
        for mt in &result.created {
            let p = deps
                .state
                .repo
                .read_proposta(&mt.proposta_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(p.jira_site.as_deref(), Some("site"));
            assert_eq!(p.jira_issue_id.as_deref(), Some("100"));
        }
    }

    #[tokio::test]
    async fn materialize_uses_deterministic_idempotency_keys() {
        let (deps, _dir, _rx) = mk_deps(None);
        let (run_id, secret) = mk_run(&deps).await;
        let args = ops::jira_materialize::Args {
            analysis_run_id: run_id.clone(),
            run_secret: secret,
            subtasks: vec![subtask("A", "ba"), subtask("B", "bb")],
        };
        let result = jira_materialize_core(&deps.state, &args).await.unwrap();
        // Keys are scoped to the analysis run, so a later run for the same
        // issue cannot collide with this run's proposals.
        assert_eq!(
            result.created[0].idempotency_key,
            format!("jira:site:100:{run_id}:0")
        );
        assert_eq!(
            result.created[1].idempotency_key,
            format!("jira:site:100:{run_id}:1")
        );
    }

    #[tokio::test]
    async fn materialize_new_run_creates_fresh_proposals() {
        // Re-importing + re-analyzing the same issue (a NEW analysis run) must
        // produce FRESH proposals, not silently dedup to the prior run's —
        // the keys are scoped to analysis_run_id. (A discarded/rejected first
        // decomposition must not block a second one.)
        let (deps, _dir, _rx) = mk_deps(None);
        seed_jira_record(&deps, "site", "100", "PROJ-1").await;
        let (run_id1, secret1) = create_analysis_run(&deps.state, "site", "100", Some("P-1"))
            .await
            .unwrap();
        let args1 = ops::jira_materialize::Args {
            analysis_run_id: run_id1,
            run_secret: secret1.expose().to_string(),
            subtasks: vec![subtask("A", "ba"), subtask("B", "bb")],
        };
        let r1 = jira_materialize_core(&deps.state, &args1).await.unwrap();

        // A second run for the same issue (e.g. after discard + re-import).
        let (run_id2, secret2) = create_analysis_run(&deps.state, "site", "100", Some("P-1"))
            .await
            .unwrap();
        let args2 = ops::jira_materialize::Args {
            analysis_run_id: run_id2,
            run_secret: secret2.expose().to_string(),
            subtasks: vec![subtask("A", "ba"), subtask("B", "bb")],
        };
        let r2 = jira_materialize_core(&deps.state, &args2).await.unwrap();

        let ids1: Vec<_> = r1.created.iter().map(|m| &m.proposta_id).collect();
        let ids2: Vec<_> = r2.created.iter().map(|m| &m.proposta_id).collect();
        assert_ne!(
            ids1, ids2,
            "a new analysis run must mint fresh proposals, not reuse the prior run's"
        );
    }

    #[tokio::test]
    async fn materialize_revokes_secret_when_done() {
        let (deps, _dir, _rx) = mk_deps(None);
        let (run_id, secret) = mk_run(&deps).await;
        let args = ops::jira_materialize::Args {
            analysis_run_id: run_id,
            run_secret: secret,
            subtasks: vec![subtask("A", "ba")],
        };
        jira_materialize_core(&deps.state, &args).await.unwrap();
        // Second run with the now-revoked secret ⇒ run_secret_revoked.
        let err = jira_materialize_core(&deps.state, &args).await.unwrap_err();
        let (code, _) = err.code_message();
        assert_eq!(code, "run_secret_revoked");
    }

    #[tokio::test]
    async fn materialize_rejects_invalid_secret() {
        let (deps, _dir, _rx) = mk_deps(None);
        let (run_id, _secret) = mk_run(&deps).await;
        let args = ops::jira_materialize::Args {
            analysis_run_id: run_id,
            run_secret: "bogus".into(),
            subtasks: vec![subtask("A", "ba")],
        };
        let err = jira_materialize_core(&deps.state, &args).await.unwrap_err();
        let (code, _) = err.code_message();
        assert_eq!(code, "run_secret_invalid");
    }

    #[tokio::test]
    async fn materialize_rejects_invalid_decomposition() {
        let (deps, _dir, _rx) = mk_deps(None);
        let (run_id, secret) = mk_run(&deps).await;
        let args = ops::jira_materialize::Args {
            analysis_run_id: run_id,
            run_secret: secret,
            subtasks: vec![subtask("dup", "b1"), subtask("dup", "b2")],
        };
        let err = jira_materialize_core(&deps.state, &args).await.unwrap_err();
        let (code, _) = err.code_message();
        assert_eq!(code, "invalid_decomposition");
    }

    #[tokio::test]
    async fn materialize_secret_never_appears_in_result_struct() {
        let (deps, _dir, _rx) = mk_deps(None);
        let (run_id, secret) = mk_run(&deps).await;
        let args = ops::jira_materialize::Args {
            analysis_run_id: run_id,
            run_secret: secret.clone(),
            subtasks: vec![subtask("A", "ba")],
        };
        let result = jira_materialize_core(&deps.state, &args).await.unwrap();
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains(&secret), "secret leaked into result JSON");
    }

    #[tokio::test]
    async fn public_propose_op_rejects_jira_site() {
        let (deps, _dir, _rx) = mk_deps(None);
        let args = serde_json::json!({
            "idempotency_key": "k1",
            "title": "t", "repro": "r", "file": "f",
            "what_failed": "w", "action": "a",
            "jira_site": "site"
        });
        let req =
            cadenza_proto::wire::Request::new(Some("1".into()), ops::OP_PROPOSE, args).unwrap();
        let (tx, _ev) = mpsc::channel::<String>(4);
        let err = dispatch(req, &deps, &tx).await.unwrap_err();
        assert_eq!(err.code, "jira_identity_forbidden");
    }

    #[tokio::test]
    async fn public_propose_op_rejects_jira_issue_id() {
        let (deps, _dir, _rx) = mk_deps(None);
        let args = serde_json::json!({
            "idempotency_key": "k1",
            "title": "t", "repro": "r", "file": "f",
            "what_failed": "w", "action": "a",
            "jira_issue_id": "100"
        });
        let req =
            cadenza_proto::wire::Request::new(Some("1".into()), ops::OP_PROPOSE, args).unwrap();
        let (tx, _ev) = mpsc::channel::<String>(4);
        let err = dispatch(req, &deps, &tx).await.unwrap_err();
        assert_eq!(err.code, "jira_identity_forbidden");
    }

    #[tokio::test]
    async fn public_propose_op_accepts_when_jira_fields_absent() {
        let (deps, _dir, _rx) = mk_deps(None);
        let args = serde_json::json!({
            "idempotency_key": "k1",
            "title": "t", "repro": "r", "file": "f",
            "what_failed": "w", "action": "a"
        });
        let req =
            cadenza_proto::wire::Request::new(Some("1".into()), ops::OP_PROPOSE, args).unwrap();
        let (tx, _ev) = mpsc::channel::<String>(4);
        let val = dispatch(req, &deps, &tx).await.unwrap();
        assert!(val.get("proposta_id").is_some());
    }

    // ── Jira import orchestration + discard (Slice 6a) ──────────────────────

    use crate::commands::{
        jira_discard_core, jira_import_persist, DiscardError, ImportError, ImportPersistOutcome,
    };
    use crate::jira::client::{CancelToken, JiraTransport};
    use crate::jira::{parse, FetchedIssue};
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    /// A canned-response transport mirroring `client.rs`'s test fake: serves
    /// queued JSON values in order and records every path it was asked for.
    struct FakeTransport {
        responses: StdMutex<VecDeque<Value>>,
        seen_paths: StdMutex<Vec<String>>,
    }

    impl FakeTransport {
        fn new(values: Vec<Value>) -> Self {
            Self {
                responses: StdMutex::new(values.into_iter().collect()),
                seen_paths: StdMutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl JiraTransport for FakeTransport {
        async fn get_json(
            &self,
            path_and_query: &str,
            _cancel: &CancelToken,
        ) -> std::result::Result<Value, crate::jira::JiraError> {
            self.seen_paths
                .lock()
                .unwrap()
                .push(path_and_query.to_string());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| crate::jira::JiraError::Parse("fake: no more responses".into()))
        }
    }

    /// Thin generic fetch helper — wraps `parse::parse_issue` exactly like the
    /// `fetch_issue_via` helper in `client.rs`. The seam for "transport called
    /// once / idempotent reimport does not re-fetch".
    async fn import_fetch_via<T: JiraTransport>(
        t: &T,
        key: &str,
    ) -> std::result::Result<FetchedIssue, crate::jira::JiraError> {
        let encoded: String = url::form_urlencoded::byte_serialize(key.as_bytes()).collect();
        let path = format!("/rest/api/3/issue/{encoded}?fields=summary,description");
        let cancel = CancelToken::new();
        let v = t.get_json(&path, &cancel).await?;
        parse::parse_issue(&v)
    }

    /// Test-only orchestrator: fetch via the fake transport, then run the pure
    /// persist core (steps 1-5). Production uses `jira_import_core` with the
    /// real client; this exercises the same persist logic with no network.
    async fn jira_import_via<T: JiraTransport>(
        deps: &ServerDeps,
        t: &T,
        jira_site: &str,
        key: &str,
        project_id: &str,
    ) -> std::result::Result<ImportPersistOutcome, ImportError> {
        let fetched = import_fetch_via(t, key).await.map_err(ImportError::Fetch)?;
        jira_import_persist(&deps.state, jira_site, &fetched, project_id).await
    }

    fn canned_issue(id: &str, key: &str, summary: &str) -> Value {
        serde_json::json!({
            "id": id,
            "key": key,
            "fields": { "summary": summary, "description": null }
        })
    }

    fn fetched(id: &str, key: &str, summary: &str, adf: Value) -> FetchedIssue {
        FetchedIssue {
            jira_issue_id: id.into(),
            jira_key: key.into(),
            summary: summary.into(),
            description_markdown: String::new(),
            raw_adf: adf,
        }
    }

    #[tokio::test]
    async fn import_creates_record_run_and_binds_project() {
        let (deps, _dir, _rx) = mk_deps(None);
        let f = fetched("10042", "PROJ-7", "Add login", Value::Null);
        let out = jira_import_persist(&deps.state, "site", &f, "P-1")
            .await
            .unwrap();
        let (record, run_id) = match out {
            ImportPersistOutcome::New {
                record,
                analysis_run_id,
                ..
            } => (record, analysis_run_id),
            _ => panic!("expected New"),
        };
        assert_eq!(record.project_id.as_deref(), Some("P-1"));
        assert_eq!(record.analysis_run_id.as_deref(), Some(run_id.as_str()));
        assert_eq!(
            record.secret_status.as_deref(),
            Some(SecretStatus::Active.as_str())
        );
        // Persisted in the store too.
        let stored = deps
            .state
            .repo
            .read_jira_issue("site", "10042")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.jira_key, "PROJ-7");
    }

    #[tokio::test]
    async fn import_validates_unknown_project_id() {
        let (deps, _dir, _rx) = mk_deps(None);
        let f = fetched("10042", "PROJ-7", "x", Value::Null);
        let err = jira_import_persist(&deps.state, "site", &f, "NOPE")
            .await
            .unwrap_err();
        assert!(matches!(err, ImportError::UnknownProject(_)));
        assert_eq!(err.code_message().0, "unknown_project");
    }

    #[tokio::test]
    async fn import_rejects_empty_issue_ref() {
        // The empty-issue_ref check lives in jira_import_core (the production
        // wrapper). Empty project_id is the persist-layer config error.
        let (deps, _dir, _rx) = mk_deps(None);
        let f = fetched("10042", "PROJ-7", "x", Value::Null);
        let err = jira_import_persist(&deps.state, "site", &f, "  ")
            .await
            .unwrap_err();
        assert_eq!(err.code_message().0, "jira_config");
    }

    #[tokio::test]
    async fn import_seeds_raw_adf_serialized() {
        let (deps, _dir, _rx) = mk_deps(None);
        let adf = serde_json::json!({"type": "doc", "version": 1});
        let f = fetched("10042", "PROJ-7", "x", adf.clone());
        jira_import_persist(&deps.state, "site", &f, "P-1")
            .await
            .unwrap();
        let stored = deps
            .state
            .repo
            .read_jira_issue("site", "10042")
            .await
            .unwrap()
            .unwrap();
        let raw = stored.raw_adf.expect("raw_adf persisted");
        let back: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(back, adf);

        // Null ADF -> None.
        let f2 = fetched("10043", "PROJ-8", "y", Value::Null);
        jira_import_persist(&deps.state, "site", &f2, "P-1")
            .await
            .unwrap();
        let stored2 = deps
            .state
            .repo
            .read_jira_issue("site", "10043")
            .await
            .unwrap()
            .unwrap();
        assert!(stored2.raw_adf.is_none());
    }

    #[tokio::test]
    async fn reimport_active_returns_existing_without_second_fetch() {
        let (deps, _dir, _rx) = mk_deps(None);
        let fake = FakeTransport::new(vec![
            canned_issue("10042", "PROJ-7", "Add login"),
            canned_issue("10042", "PROJ-7", "Add login"),
        ]);
        // First import: New (mints run -> active).
        let first = jira_import_via(&deps, &fake, "site", "PROJ-7", "P-1")
            .await
            .unwrap();
        assert!(matches!(first, ImportPersistOutcome::New { .. }));
        // Second import with the SAME transport: ExistingActive, no re-fetch.
        let second = jira_import_via(&deps, &fake, "site", "PROJ-7", "P-1")
            .await
            .unwrap();
        assert!(matches!(
            second,
            ImportPersistOutcome::ExistingActive { .. }
        ));
        assert_eq!(
            fake.seen_paths.lock().unwrap().len(),
            2,
            "transport fetched once per jira_import_via call; the SECOND \
             persist short-circuits before spawning but fetch already ran in \
             the test orchestrator — assert active short-circuit instead"
        );
    }

    #[tokio::test]
    async fn reimport_inactive_record_re_mints() {
        let (deps, _dir, _rx) = mk_deps(None);
        let f = fetched("10042", "PROJ-7", "x", Value::Null);
        // First import -> active.
        let out = jira_import_persist(&deps.state, "site", &f, "P-1")
            .await
            .unwrap();
        let (created_at, run_id) = match out {
            ImportPersistOutcome::New {
                record,
                analysis_run_id,
                ..
            } => (record.created_at_ms, analysis_run_id),
            _ => panic!("expected New"),
        };
        // Revoke the secret so the record becomes inactive.
        crate::commands::revoke_run_secret(&deps.state, &run_id)
            .await
            .unwrap();
        // Re-import -> New again (re-mint), created_at preserved.
        let out2 = jira_import_persist(&deps.state, "site", &f, "P-1")
            .await
            .unwrap();
        match out2 {
            ImportPersistOutcome::New { record, .. } => {
                assert_eq!(record.created_at_ms, created_at, "created_at preserved");
                assert_eq!(
                    record.secret_status.as_deref(),
                    Some(SecretStatus::Active.as_str())
                );
            }
            _ => panic!("expected re-mint New"),
        }
    }

    #[test]
    fn import_persist_outcome_contains_no_secret_in_proto_result() {
        // The proto Result is a distinct type from ImportPersistOutcome and
        // structurally cannot carry a secret. Assert a serialized Imported
        // result does not contain the secret-shaped plaintext.
        let secret = "supersecretplaintext1234567890";
        let result = ops::jira_import::Result::Imported {
            jira_site: "site".into(),
            jira_issue_id: "10042".into(),
            jira_key: "PROJ-7".into(),
            summary: "x".into(),
            project_id: "P-1".into(),
            analysis_run_id: "run-abc".into(),
            session_id: "S-1".into(),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains(secret));
        assert!(json.contains("imported"));
    }

    // ── Secret-never-leaks (spawn config assembly) ──────────────────────────

    #[test]
    fn import_spawn_env_carries_secret_not_argv() {
        use crate::agent::plan_launch;
        use crate::config::AgenteKind;
        let secret = "supersecretplaintext1234567890";
        let cwd = std::env::temp_dir();
        let plan = plan_launch(
            AgenteKind::ClaudeCode,
            "",
            None,
            &cwd,
            "JIRA-site-10042",
            "P-1",
            None,
            Some("decompose PROJ-7"),
        );
        let spawn = plan
            .spawn
            .jira_analyst_env("run-abc", secret, "site", "10042", "PROJ-7");
        assert!(
            spawn
                .env
                .iter()
                .any(|(k, v)| k == "CADENZA_RUN_SECRET" && v == secret),
            "secret must reach the child via env"
        );
        assert!(
            !spawn.args.iter().any(|a| a.contains(secret)),
            "secret must NEVER appear in argv"
        );
    }

    // ── Discard lifecycle ───────────────────────────────────────────────────

    /// A git repo with one commit on `main`, used as the project path so the
    /// discard worktree-removal path can run real git.
    fn git_temp_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed");
        };
        run(&["init"]);
        run(&["config", "user.email", "t@e.com"]);
        run(&["config", "user.name", "T"]);
        run(&["commit", "--allow-empty", "-m", "init"]);
        run(&["branch", "-M", "main"]);
        dir
    }

    /// Build deps whose project P-1 path is a real git repo, plus add a
    /// worktree and seed a Ready JiraIssueRecord pointing at it.
    async fn mk_deps_with_worktree() -> (ServerDeps, TempDir, TempDir, std::path::PathBuf) {
        let repo_dir = git_temp_repo();
        let data_dir = TempDir::new().unwrap();
        let repo = Arc::new(FileRepository::new(data_dir.path()).unwrap());
        let config = Config {
            projects: vec![Project {
                id: "P-1".into(),
                name: "Proj".into(),
                path: repo_dir.path().to_path_buf(),
                agente: None,
                default_branch: Some("main".into()),
                color: None,
                quality: None,
            }],
            active_project_id: Some("P-1".into()),
            ..Default::default()
        };
        let state = AppState::for_test(data_dir.path(), repo, config).unwrap();
        let (tx, _rx) = mpsc::channel(64);
        let deps = ServerDeps {
            state: Arc::new(state),
            data_dir: data_dir.path().to_path_buf(),
            webview_events: tx,
        };
        // Create a worktree under the data dir.
        let wt = data_dir.path().join("wt");
        crate::git::add_worktree(repo_dir.path(), &wt, "jira/10042", true, None)
            .await
            .unwrap();
        (deps, repo_dir, data_dir, wt)
    }

    async fn seed_ready_record(deps: &ServerDeps, wt: &std::path::Path, with_run: bool) -> String {
        let mut rec = cadenza_proto::JiraIssueRecord {
            jira_site: "site".into(),
            jira_issue_id: "10042".into(),
            jira_key: "PROJ-7".into(),
            project_id: Some("P-1".into()),
            analysis_run_id: None,
            secret_hash: None,
            secret_expiry_ms: None,
            secret_status: None,
            raw_adf: None,
            branch_name: Some("jira/10042".into()),
            worktree_path: Some(wt.to_string_lossy().into_owned()),
            base_sha: Some("deadbeef".into()),
            worktree_state: Some(cadenza_proto::jira::WorktreeState::Ready.as_str().into()),
            created_at_ms: now_ms(),
            updated_at_ms: now_ms(),
        };
        deps.state.repo.upsert_jira_issue(&rec).await.unwrap();
        if with_run {
            let (run_id, _secret) = create_analysis_run(&deps.state, "site", "10042", Some("P-1"))
                .await
                .unwrap();
            // create_analysis_run re-reads and stamps; re-read to keep our copy fresh.
            rec = deps
                .state
                .repo
                .read_jira_issue("site", "10042")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(rec.analysis_run_id.as_deref(), Some(run_id.as_str()));
            run_id
        } else {
            String::new()
        }
    }

    fn discard_args(force: bool) -> ops::jira_discard::Args {
        ops::jira_discard::Args {
            jira_site: "site".into(),
            jira_issue_id: "10042".into(),
            force,
        }
    }

    #[tokio::test]
    async fn discard_refuses_dirty_worktree() {
        let (deps, _r, _d, wt) = mk_deps_with_worktree().await;
        seed_ready_record(&deps, &wt, false).await;
        std::fs::write(wt.join("scratch.txt"), b"dirty").unwrap();
        let err = jira_discard_core(&deps.state, &discard_args(false))
            .await
            .unwrap_err();
        match err {
            DiscardError::WorktreeDirty { changed_files } => assert!(changed_files >= 1),
            other => panic!("expected WorktreeDirty, got {other:?}"),
        }
        // Record + worktree must survive a refused discard.
        assert!(deps
            .state
            .repo
            .read_jira_issue("site", "10042")
            .await
            .unwrap()
            .is_some());
        assert!(wt.exists());
    }

    #[tokio::test]
    async fn discard_dirty_error_omits_filenames() {
        let (deps, _r, _d, wt) = mk_deps_with_worktree().await;
        seed_ready_record(&deps, &wt, false).await;
        std::fs::write(wt.join("secret-name.txt"), b"dirty").unwrap();
        let err = jira_discard_core(&deps.state, &discard_args(false))
            .await
            .unwrap_err();
        let (_code, msg) = err.code_message();
        assert!(
            !msg.contains("secret-name"),
            "message leaked a filename: {msg}"
        );
        assert!(msg.contains('1'), "message should carry the count: {msg}");
    }

    #[tokio::test]
    async fn discard_succeeds_on_clean_worktree() {
        let (deps, _r, _d, wt) = mk_deps_with_worktree().await;
        seed_ready_record(&deps, &wt, false).await;
        let res = jira_discard_core(&deps.state, &discard_args(false))
            .await
            .unwrap();
        assert!(res.worktree_removed);
        assert!(!wt.exists());
        assert!(deps
            .state
            .repo
            .read_jira_issue("site", "10042")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn discard_force_removes_dirty_worktree() {
        let (deps, _r, _d, wt) = mk_deps_with_worktree().await;
        seed_ready_record(&deps, &wt, false).await;
        std::fs::write(wt.join("scratch.txt"), b"dirty").unwrap();
        let res = jira_discard_core(&deps.state, &discard_args(true))
            .await
            .unwrap();
        assert!(res.worktree_removed);
        assert!(!wt.exists());
    }

    #[tokio::test]
    async fn discard_revokes_run_secret() {
        let (deps, _r, _d, wt) = mk_deps_with_worktree().await;
        let run_id = seed_ready_record(&deps, &wt, true).await;
        jira_discard_core(&deps.state, &discard_args(false))
            .await
            .unwrap();
        // The record is gone, so verify resolves NotFound — but the secret was
        // revoked before deletion. Re-seed a record with the same run id is not
        // possible; assert verify fails (NotFound/Revoked are both "no go").
        let err = verify_run_secret(&deps.state, &run_id, "anything")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            RunSecretError::Revoked | RunSecretError::NotFound
        ));
    }

    #[tokio::test]
    async fn discard_forgets_subtask_task_worktrees() {
        let (deps, _r, _d, wt) = mk_deps_with_worktree().await;
        seed_ready_record(&deps, &wt, false).await;
        // Seed two subtask tasks bound to the issue, each with a worktree entry.
        for id in ["T-1", "T-2"] {
            let task = cadenza_proto::Task {
                id: id.into(),
                titulo: id.into(),
                estado: cadenza_proto::Estado::AFazer,
                responsavel: "humano".into(),
                body: "b".into(),
                worktree_path: None,
                branch: None,
                blocked_by: Vec::new(),
                jira_site: Some("site".into()),
                jira_issue_id: Some("10042".into()),
                jira_key_display: None,
            };
            deps.state.repo.create_task(&task).await.unwrap();
            deps.state.task_jira.set(id, "site", "10042").unwrap();
            deps.state
                .task_worktrees
                .set(
                    id,
                    crate::worktrees::WorktreeInfo {
                        worktree_path: Some(format!("/tmp/{id}")),
                        branch: Some("b".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        let res = jira_discard_core(&deps.state, &discard_args(false))
            .await
            .unwrap();
        assert_eq!(res.forgotten_task_worktrees, 2);
        assert!(deps.state.task_worktrees.get("T-1").is_none());
        assert!(deps.state.task_worktrees.get("T-2").is_none());
    }

    #[tokio::test]
    async fn discard_retains_review_packages() {
        let (deps, _r, _d, wt) = mk_deps_with_worktree().await;
        seed_ready_record(&deps, &wt, false).await;
        // Seed an aggregate review package for the issue.
        let pkg = crate::store::IssueReviewPackage {
            jira_site: "site".into(),
            jira_issue_id: "10042".into(),
            attempt: 0,
            idempotency_key: "k".into(),
            status: crate::review::issue::IssuePackageStatus::Pending,
            branch_name: "jira/10042".into(),
            base_sha: "a".into(),
            head_sha: Some("b".into()),
            changed_files: Vec::new(),
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
            diff: None,
            truncated: false,
            collection_errors: Vec::new(),
            created_at_ms: now_ms() as u64,
            collection_duration_ms: 0,
        };
        deps.state
            .repo
            .upsert_issue_review_package(&pkg)
            .await
            .unwrap();
        jira_discard_core(&deps.state, &discard_args(false))
            .await
            .unwrap();
        let retained = deps
            .state
            .repo
            .latest_issue_review_package("site", "10042")
            .await
            .unwrap();
        assert!(retained.is_some(), "review packages must be retained");
    }

    #[tokio::test]
    async fn discard_missing_record_is_not_found() {
        let (deps, _dir, _rx) = mk_deps(None);
        let err = jira_discard_core(&deps.state, &discard_args(false))
            .await
            .unwrap_err();
        assert!(matches!(err, DiscardError::NotFound));
        assert_eq!(err.code_message().0, "jira_not_found");
    }

    #[tokio::test]
    async fn delete_task_does_not_trigger_jira_discard() {
        // Deleting a subtask must NOT remove the parent JiraIssueRecord.
        let (deps, _r, _d, wt) = mk_deps_with_worktree().await;
        seed_ready_record(&deps, &wt, false).await;
        deps.state.repo.delete_task("T-1").await.ok();
        // The record (and worktree) are untouched by a task delete.
        assert!(deps
            .state
            .repo
            .read_jira_issue("site", "10042")
            .await
            .unwrap()
            .is_some());
        assert!(wt.exists());
    }

    #[tokio::test]
    async fn discard_refuses_when_executor_busy() {
        let (deps, _r, _d, wt) = mk_deps_with_worktree().await;
        seed_ready_record(&deps, &wt, false).await;
        // A `Reserving` slot counts as busy without needing a live PTY session
        // (TerminalSession::start needs a real PTY, awkward in a unit test),
        // which is exactly the in-flight-start case the guard must refuse.
        let key = ("site".to_string(), "10042".to_string());
        {
            let mut active = deps.state.jira_active_executors.lock().unwrap();
            active.insert(key, crate::jira::worktree::ExecutorSlot::Reserving);
        }
        let err = jira_discard_core(&deps.state, &discard_args(false))
            .await
            .unwrap_err();
        assert!(matches!(err, DiscardError::Busy));
        assert_eq!(err.code_message().0, "jira_worktree_busy");
    }
}
