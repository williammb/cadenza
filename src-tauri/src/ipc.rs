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
        OP_CURRENT_TASK, OP_DELETE_IDEIA, OP_DONE, OP_HELLO, OP_LIST_IDEIAS, OP_LIST_TASKS,
        OP_PROPOSE, OP_READ_IDEIA, OP_SET_IDEIA_STATUS,
    },
    wire::{ErrorBody, Event, Request, Response},
    Decisao, DecisaoRegistro, Ideia, IdeiaStatus, MAX_PROTOCOL, MIN_PROTOCOL,
};
use interprocess::local_socket::{tokio::prelude::*, ListenerOptions};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
#[cfg(not(windows))]
use interprocess::local_socket::{GenericFilePath, ToFsName};
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
const MAX_LINE_BYTES: usize = 1 * 1024 * 1024;
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
            send_err(&tx, None, ErrorBody::new("line_too_long", "line exceeds 1 MiB")).await;
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
    if protocol < MIN_PROTOCOL {
        send_err(
            &tx,
            hello_id,
            ErrorBody::new("protocol_too_old", "update cli"),
        )
        .await;
        drop(tx);
        let _ = writer_handle.await;
        return Ok(());
    }
    if protocol > MAX_PROTOCOL {
        send_err(
            &tx,
            hello_id,
            ErrorBody::new("protocol_too_new", "update app"),
        )
        .await;
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

    // Token validation.
    let token_ok = crate::auth::validate(&deps.data_dir, &args.token).unwrap_or(false);
    if !token_ok {
        send_err(&tx, hello_id, ErrorBody::new("auth_failed", "invalid token")).await;
        drop(tx);
        let _ = writer_handle.await;
        return Ok(());
    }

    // Welcome.
    let hello_result = ops::hello::Result {
        protocol: MAX_PROTOCOL,
        app: format!("cadenza/{SERVER_APP_VERSION}"),
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
                send_err(&tx, None, ErrorBody::new("line_too_long", "line exceeds 1 MiB"))
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
            let args: ops::list_tasks::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            let filter = args
                .estado
                .as_deref()
                .and_then(cadenza_proto::Estado::parse);
            let tasks = repo
                .list_tasks(filter)
                .await
                .map_err(|e| not_found_or_internal(&e))?;
            to_value(&tasks)
        }
        OP_CURRENT_TASK => {
            let _: ops::current_task::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            let current: ops::current_task::Result = repo
                .current_task()
                .await
                .map_err(|e| not_found_or_internal(&e))?;
            to_value(&current)
        }
        OP_APPEND_LOG => {
            let args: ops::append_log::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
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
            let args: ops::propose::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
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
            check_id(&args.task_id)?;
            done_op(repo.as_ref(), &args).await?;
            // Estado changed to aguardando_revisao + body appended; UI
            // needs to pick up both. Emit alongside OP_CREATE_TASK's
            // event so the board reconciles without a manual reload.
            let _ = deps.webview_events.try_send((
                ops::EV_TASKS_CHANGED.to_string(),
                serde_json::json!({ "task_id": args.task_id }),
            ));
            to_value(&ops::done::Result { ok: true })
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
            let _: ops::list_ideias::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
            let ideias = repo
                .list_ideias()
                .await
                .map_err(|e| internal(&e.to_string()))?;
            to_value(&ideias)
        }
        OP_READ_IDEIA => {
            let args: ops::read_ideia::Args =
                serde_json::from_value(req.args).map_err(bad_args)?;
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
        OP_BYE => to_value(&ops::bye::Result { ok: true }),
        OP_HELLO => Err(ErrorBody::new(
            "hello_already_done",
            "hello may only be sent once",
        )),
        other => Err(ErrorBody::new(
            "unknown_op",
            format!("unknown op: {other}"),
        )),
    }
}

/// Validar projeto + criar task + amarrar mapping. Compartilhado entre o
/// dispatcher e a versão Tauri (que tem essa lógica inline em
/// `commands.rs::create_task` — duplicada de propósito porque os tipos
/// de erro e o caminho de origem são diferentes).
async fn create_task_op(
    deps: &ServerDeps,
    args: &ops::create_task::Args,
) -> Result<ops::create_task::Result, ErrorBody> {
    let pid = args.project_id.trim();
    if pid.is_empty() {
        return Err(ErrorBody::new("bad_args", "project_id is required"));
    }
    {
        let cfg = deps.state.config.lock().map_err(|e| internal(&e.to_string()))?;
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
        let cfg = deps.state.config.lock().map_err(|e| internal(&e.to_string()))?;
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

/// `done` is per-design "request to complete" — agents never put a task
/// in `feito` directly. We append the summary as a log line and move
/// the task to `aguardando_revisao`, so the human still has final say.
async fn done_op(repo: &dyn Repository, args: &ops::done::Args) -> Result<(), ErrorBody> {
    repo.append_log(
        &args.task_id,
        &format!("[done request] {}", args.summary),
    )
    .await
    .map_err(|e| not_found_or_internal(&e))?;
    repo.set_estado(&args.task_id, cadenza_proto::Estado::AguardandoRevisao)
        .await
        .map_err(|e| not_found_or_internal(&e))?;
    Ok(())
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
