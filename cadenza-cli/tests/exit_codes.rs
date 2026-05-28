//! Integration tests that verify the CLI exits with the documented exit codes.
//!
//! Exit codes (CLAUDE.md § "Exit codes"):
//!   0  ok
//!   1  generic
//!   10 app not running (socket not found)
//!   11 bad/missing token
//!   12 protocol mismatch
//!   20 proposal rejected
//!   21 decision timeout
//!   30 task/resource not found

use assert_cmd::Command;
use std::path::Path;
use tempfile::TempDir;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn unique_user() -> String {
    // USERNAME env var drives the Windows named-pipe name: cadenza-<user>.
    // A UUID suffix guarantees no collision with the real cadenza instance.
    format!("cadenza-test-{}", uuid::Uuid::new_v4().simple())
}

/// Create a temp data dir, optionally writing an auth token.
fn make_data_dir(token: Option<&str>) -> TempDir {
    let dir = TempDir::new().unwrap();
    if let Some(t) = token {
        std::fs::write(dir.path().join("auth"), t).unwrap();
    }
    dir
}

/// Build a `cadenza-cli` command redirected to a test pipe and data dir.
fn cli(username: &str, data_dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("cadenza-cli").unwrap();
    cmd.env("USERNAME", username)
        .env("CADENZA_DATA_DIR", data_dir);
    cmd
}

// ─────────────────────────────────────────────────────────────────────────────
// Mock IPC server (Windows named-pipe only)
// ─────────────────────────────────────────────────────────────────────────────

/// Spawn a named-pipe server that handles exactly one connection.
///
/// For each request received, the next entry from `responses` is sent back
/// after injecting the request's `id` into the template.  Returns as soon as
/// the listener is created (i.e. the pipe exists and the CLI can connect
/// without sleeping).
#[cfg(windows)]
fn start_mock(pipe_name: &str, responses: Vec<serde_json::Value>) -> std::thread::JoinHandle<()> {
    use interprocess::local_socket::tokio::prelude::*;
    use interprocess::local_socket::{GenericNamespaced, ListenerOptions, ToNsName};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let pipe_name = pipe_name.to_string();

    // Build the runtime and create the listener synchronously so the caller
    // does not need to sleep — the pipe exists the moment this function
    // returns.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let name = pipe_name
        .as_str()
        .to_ns_name::<GenericNamespaced>()
        .unwrap();
    let listener = rt
        .block_on(async { ListenerOptions::new().name(name).create_tokio() })
        .expect("create mock pipe listener");

    std::thread::spawn(move || {
        rt.block_on(async move {
            let conn = match listener.accept().await {
                Ok(c) => c,
                Err(_) => return,
            };
            let (rh, mut wh) = tokio::io::split(conn);
            let mut lines = BufReader::new(rh).lines();

            for mut tmpl in responses {
                let raw = match lines.next_line().await {
                    Ok(Some(l)) => l,
                    _ => break,
                };
                // Mirror the request id so the client's correlation check passes.
                if let Ok(req) = serde_json::from_str::<serde_json::Value>(&raw) {
                    if let Some(id) = req.get("id") {
                        tmpl["id"] = id.clone();
                    }
                }
                let line = serde_json::to_string(&tmpl).unwrap();
                if wh.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if wh.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = wh.flush().await;
            }
        });
    })
}

// Canned response builders.

#[cfg(windows)]
fn hello_ok() -> serde_json::Value {
    serde_json::json!({
        "v": 1, "ok": true,
        "result": {
            "protocol": cadenza_proto::MAX_PROTOCOL,
            "app": "cadenza/test"
        }
    })
}

#[cfg(windows)]
fn propose_ok() -> serde_json::Value {
    serde_json::json!({
        "v": 1, "ok": true,
        "result": {"proposta_id": "P-test"}
    })
}

#[cfg(windows)]
fn err_resp(code: &str, msg: &str) -> serde_json::Value {
    serde_json::json!({
        "v": 1, "ok": false,
        "error": {"code": code, "message": msg, "retryable": false}
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — platform-independent
// ─────────────────────────────────────────────────────────────────────────────

/// No server listening on the test pipe → connect fails → exit 10.
#[test]
fn exit_10_app_not_running() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    cli(&user, data.path())
        .args(["current"])
        .assert()
        .failure()
        .code(10);
}

/// Auth file absent → `read_token` fails → `TokenError` → exit 11 (no server needed).
#[test]
fn exit_11_token_file_missing() {
    let user = unique_user();
    let data = make_data_dir(None);
    cli(&user, data.path())
        .args(["current"])
        .assert()
        .failure()
        .code(11);
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — Windows (mock server via named pipe)
// ─────────────────────────────────────────────────────────────────────────────

/// Token present but server returns `auth_failed` → `WireError` → exit 11.
#[cfg(windows)]
#[test]
fn exit_11_auth_rejected_by_server() {
    let user = unique_user();
    let data = make_data_dir(Some("bad-token"));
    let _srv = start_mock(
        &format!("cadenza-{user}"),
        vec![err_resp("auth_failed", "invalid token")],
    );
    cli(&user, data.path())
        .args(["current"])
        .assert()
        .failure()
        .code(11);
}

/// Server returns `protocol_too_new` on hello → `WireError` → exit 12.
#[cfg(windows)]
#[test]
fn exit_12_protocol_mismatch() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    let _srv = start_mock(
        &format!("cadenza-{user}"),
        vec![err_resp("protocol_too_new", "update app")],
    );
    cli(&user, data.path())
        .args(["current"])
        .assert()
        .failure()
        .code(12);
}

/// Server returns decision `rejeitada` → client creates `WireError("proposal_rejected")` → exit 20.
#[cfg(windows)]
#[test]
fn exit_20_proposal_rejected() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    let _srv = start_mock(
        &format!("cadenza-{user}"),
        vec![
            hello_ok(),
            propose_ok(),
            // await_decision returns a successful decisao=rejeitada (the CLI
            // then converts this into a WireError("proposal_rejected")).
            serde_json::json!({
                "v": 1, "ok": true,
                "result": {
                    "proposta_id": "P-test",
                    "decisao": "rejeitada",
                    "task_id": null,
                    "autor": "test",
                    "decided_at_ms": 0
                }
            }),
        ],
    );
    cli(&user, data.path())
        .args([
            "propose",
            "--title", "Test proposal",
            "--repro", "reproduce: step 1",
            "--file", "src/foo.rs",
            "--what-failed", "assertion fails",
            "--action", "fix the bug",
        ])
        .assert()
        .failure()
        .code(20);
}

/// Server returns `decision_timeout` on await_decision → exit 21.
#[cfg(windows)]
#[test]
fn exit_21_decision_timeout() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    let _srv = start_mock(
        &format!("cadenza-{user}"),
        vec![
            hello_ok(),
            propose_ok(),
            err_resp("decision_timeout", "no decision in time"),
        ],
    );
    cli(&user, data.path())
        .args([
            "propose",
            "--title", "Test proposal",
            "--repro", "reproduce: step 1",
            "--file", "src/foo.rs",
            "--what-failed", "assertion fails",
            "--action", "fix the bug",
        ])
        .assert()
        .failure()
        .code(21);
}

/// Server returns `task_not_found` on `append_log` → exit 30.
#[cfg(windows)]
#[test]
fn exit_30_task_not_found() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    let _srv = start_mock(
        &format!("cadenza-{user}"),
        vec![
            hello_ok(),
            err_resp("task_not_found", "T-nonexistent"),
        ],
    );
    cli(&user, data.path())
        .args(["log", "T-nonexistent", "progress update"])
        .assert()
        .failure()
        .code(30);
}
