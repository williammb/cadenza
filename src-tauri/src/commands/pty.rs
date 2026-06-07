//! PTY / terminal command handlers — split out of the `commands` god-module.
//! Pure relocation: the PTY spawn/write/resize/kill/snapshot/attach commands,
//! their arg/result structs, the column/row defaults, and the lag-resync
//! helper. Re-exported via `commands`'s `pub use pty::*;` so every existing
//! `commands::pty_*` path still resolves unchanged (Tauri `generate_handler!`
//! in lib.rs references these paths).

// Bring in the parent module's imports and shared helpers (AppState,
// to_str_err, SpawnConfig, PtyHandle, TerminalSession, Channel, Uuid, etc.).
// Parent-private items are visible to this child module.
use super::*;

#[derive(Debug, Deserialize)]
pub struct PtySpawnArgs {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub session_id_hint: Option<String>,
}

fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

#[derive(Debug, Serialize)]
pub struct PtySpawnResult {
    pub session_id: String,
}

#[tauri::command]
pub fn pty_spawn(
    state: State<'_, Arc<AppState>>,
    args: PtySpawnArgs,
) -> Result<PtySpawnResult, String> {
    let claude_session_id = args
        .session_id_hint
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());

    let mut cfg = SpawnConfig::new(args.command)
        .args(args.args)
        .size(args.cols, args.rows);
    if let Some(d) = args.cwd {
        cfg = cfg.cwd(d);
    }
    for (k, v) in args.env {
        cfg = cfg.env(k, v);
    }
    if let (Some(pid), Some(tid)) = (args.project_id.as_ref(), args.task_id.as_ref()) {
        cfg = cfg.cadenza_env(pid, tid, &claude_session_id);
    }

    let pty = PtyHandle::spawn(cfg).map_err(to_str_err)?;
    let session_id = format!("S-{}", Uuid::new_v4().simple());
    let session = TerminalSession::start(session_id.clone(), pty).map_err(to_str_err)?;
    state
        .sessions
        .lock()
        .map_err(to_str_err)?
        .insert(session_id.clone(), session);
    tracing::info!(session = %session_id, "pty session started");
    Ok(PtySpawnResult { session_id })
}

#[tauri::command]
pub fn pty_write(
    state: State<'_, Arc<AppState>>,
    session_id: String,
    data: Vec<u8>,
) -> Result<(), String> {
    let session = state
        .sessions
        .lock()
        .map_err(to_str_err)?
        .get(&session_id)
        .cloned()
        .ok_or_else(|| format!("session {session_id} not found"))?;
    session.write(&data).map_err(to_str_err)
}

#[tauri::command]
pub fn pty_resize(
    state: State<'_, Arc<AppState>>,
    session_id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let session = state
        .sessions
        .lock()
        .map_err(to_str_err)?
        .get(&session_id)
        .cloned()
        .ok_or_else(|| format!("session {session_id} not found"))?;
    session.resize(cols, rows).map_err(to_str_err)
}

#[tauri::command]
pub fn pty_kill(state: State<'_, Arc<AppState>>, session_id: String) -> Result<(), String> {
    let session = state
        .sessions
        .lock()
        .map_err(to_str_err)?
        .remove(&session_id)
        .ok_or_else(|| format!("session {session_id} not found"))?;
    // Slice 4: clear the one-executor-per-issue registry for whichever issue
    // (if any) this session was the active executor of, so a follow-up task
    // for the same issue can start immediately.
    if let Ok(mut active) = state.jira_active_executors.lock() {
        // Drop only the `Live` slot for this session; leave any `Reserving`
        // slot (owned by an in-flight start) untouched.
        active.retain(
            |_, slot| !matches!(slot, crate::jira::worktree::ExecutorSlot::Live(sid) if sid == &session_id),
        );
    }
    session.kill().map_err(to_str_err)
}

#[tauri::command]
pub fn pty_snapshot(
    state: State<'_, Arc<AppState>>,
    session_id: String,
) -> Result<Vec<u8>, String> {
    let session = state
        .sessions
        .lock()
        .map_err(to_str_err)?
        .get(&session_id)
        .cloned()
        .ok_or_else(|| format!("session {session_id} not found"))?;
    Ok(session.snapshot())
}

/// Bytes that resync a lagged subscriber: home the cursor, clear the
/// viewport (`2J`) and xterm's scrollback (`3J`), then replay the current
/// authoritative ring. The clear avoids doubling content the viewer
/// already rendered before the lag.
///
/// `pub(crate)` so the `commands::tests` unit tests (still in `mod.rs`) can
/// exercise it through the `pub use pty::*;` re-export.
pub(crate) fn resync_payload(snapshot: Vec<u8>) -> Vec<u8> {
    const CLEAR: &[u8] = b"\x1b[H\x1b[2J\x1b[3J";
    let mut payload = Vec::with_capacity(snapshot.len() + CLEAR.len());
    payload.extend_from_slice(CLEAR);
    payload.extend_from_slice(&snapshot);
    payload
}

/// Stream PTY bytes to the frontend over a Tauri channel. The frontend
/// constructs `new Channel<number[]>()` and passes it as the `channel`
/// arg — the first message is the current scrollback, subsequent
/// messages are live chunks.
#[tauri::command]
pub async fn pty_attach(
    state: State<'_, Arc<AppState>>,
    session_id: String,
    channel: Channel<Vec<u8>>,
) -> Result<(), String> {
    let session = {
        let sessions = state.sessions.lock().map_err(to_str_err)?;
        sessions
            .get(&session_id)
            .cloned()
            .ok_or_else(|| format!("session {session_id} not found"))?
    };

    // Replay scrollback first so reattaches don't lose context. Snapshot
    // and subscription are paired atomically so chunks produced during
    // attach are not lost between the two operations.
    let (snap, mut rx) = session.subscribe_with_snapshot();
    if !snap.is_empty() {
        let _ = channel.send(snap);
    }

    // Cloned for the resync path below — the forwarding loop needs to
    // re-snapshot the ring after a broadcast lag.
    let session_for_resync = session.clone();
    let handle = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(bytes) => {
                    if channel.send(bytes).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // The subscriber fell behind and the broadcast dropped
                    // `n` chunks. Resuming from the next live chunk would
                    // leave a hole mid-stream and corrupt xterm's escape-
                    // sequence state. Resync instead: atomically grab a
                    // fresh snapshot + receiver (so no chunk is lost or
                    // doubled across the swap), clear the viewport +
                    // scrollback, and replay the current authoritative
                    // ring. Best-effort — a backend terminal emulator
                    // would resync without the visible reset.
                    tracing::warn!(lagged = n, "pty broadcast lagged; resyncing from ring");
                    let (resync_snap, fresh_rx) = session_for_resync.subscribe_with_snapshot();
                    rx = fresh_rx;
                    if channel.send(resync_payload(resync_snap)).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    // Keep at most one stream loop alive per session: a re-attach (e.g.
    // after a webview reload) aborts the previous loop so the old
    // subscriber can't keep draining the broadcast into a dead channel.
    session.set_attach_task(handle.abort_handle());

    Ok(())
}
