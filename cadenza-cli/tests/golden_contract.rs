//! Golden CLI contract tests — assert the CLI's JSON output SHAPE and the
//! STABLE exit codes (CLAUDE.md § "Exit codes").
//!
//! Exit codes:
//!   0  ok
//!   1  generic
//!   2  bad usage (clap / malformed args)
//!   10 app not running (socket not found)
//!   11 bad/missing token
//!   12 protocol mismatch
//!   20 proposal rejected
//!   21 decision timeout
//!   30 task/resource not found
//!
//! These complement `exit_codes.rs`: that file proves the connect/auth/wire
//! error → exit-code mapping; this file pins the codes reachable WITHOUT a
//! running app (0/2/10/11) plus the JSON output SHAPE of the read/report
//! commands the CLI exposes (`current` / `list` / `log` / `done`), driven by a
//! real `cadenza-cli` binary against a mock NDJSON server. JSON values stay in
//! the canonical Portuguese task states.

use assert_cmd::Command;
use std::path::Path;
use tempfile::TempDir;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers (shared shape with exit_codes.rs; duplicated to keep tests independent)
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
// Exit codes reachable WITHOUT a running app (platform-independent)
// ─────────────────────────────────────────────────────────────────────────────

/// Exit 2 — bad usage. `done --idempotency-key` with an invalid key is a
/// validated usage error (UsageError → exit 2), reachable before any socket
/// connect because key validation runs locally.
///
/// Note: this fails at `connect` first only if it reaches the wire — but the
/// idempotency-key validation in `cmd_done` runs AFTER hello. To keep this a
/// pure no-app usage test we instead use clap-level bad usage, which exits 2
/// without touching the socket.
#[test]
fn exit_2_clap_bad_usage_missing_required_arg() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    // `propose` requires --title etc.; omitting them is a clap usage error (2).
    cli(&user, data.path())
        .args(["propose"])
        .assert()
        .failure()
        .code(2);
}

/// Exit 2 — unknown subcommand is also a clap usage error.
#[test]
fn exit_2_unknown_subcommand() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    cli(&user, data.path())
        .args(["definitely-not-a-command"])
        .assert()
        .failure()
        .code(2);
}

/// Exit 10 — no server listening on the test pipe → connect fails.
#[test]
fn exit_10_app_not_running() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    cli(&user, data.path())
        .args(["list"])
        .assert()
        .failure()
        .code(10);
}

/// Exit 11 — auth file absent → `read_token` fails before any connect.
#[test]
fn exit_11_token_file_missing() {
    let user = unique_user();
    let data = make_data_dir(None);
    cli(&user, data.path())
        .args(["list"])
        .assert()
        .failure()
        .code(11);
}

/// Exit 0 — the local-only `diag` command needs no app and succeeds.
#[test]
fn exit_0_diag_succeeds_without_app() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    cli(&user, data.path()).args(["diag"]).assert().success();
}

// ─────────────────────────────────────────────────────────────────────────────
// Mock IPC server (Windows named-pipe only) — for JSON SHAPE + wire exit codes
// ─────────────────────────────────────────────────────────────────────────────

/// Spawn a named-pipe server that handles exactly one connection. For each
/// request received, the next entry from `responses` is sent back after
/// injecting the request's `id` into the template. Returns as soon as the
/// listener is created (the pipe exists and the CLI can connect without
/// sleeping). The listener is dropped when the runtime thread ends, so there is
/// no pipe to clean up afterward.
#[cfg(windows)]
fn start_mock(pipe_name: &str, responses: Vec<serde_json::Value>) -> std::thread::JoinHandle<()> {
    use interprocess::local_socket::tokio::prelude::*;
    use interprocess::local_socket::{GenericNamespaced, ListenerOptions, ToNsName};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let pipe_name = pipe_name.to_string();
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

#[cfg(windows)]
fn hello_ok() -> serde_json::Value {
    serde_json::json!({
        "v": 1, "ok": true,
        "result": { "protocol": cadenza_proto::MAX_PROTOCOL, "app": "cadenza/test" }
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
// JSON output SHAPE — read/report commands (Windows mock server)
// ─────────────────────────────────────────────────────────────────────────────

/// `list --json` emits a JSON array; each task carries the canonical PT estado.
#[cfg(windows)]
#[test]
fn list_json_shape_is_array_with_canonical_estado() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    let _srv = start_mock(
        &format!("cadenza-{user}"),
        vec![
            hello_ok(),
            serde_json::json!({
                "v": 1, "ok": true,
                "result": [
                    {"id": "T-1", "titulo": "a", "estado": "fazendo", "responsavel": "humano", "body": ""},
                    {"id": "T-2", "titulo": "b", "estado": "a_fazer", "responsavel": "humano", "body": ""}
                ]
            }),
        ],
    );
    let out = cli(&user, data.path())
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("stdout is valid JSON");
    let arr = v.as_array().expect("list --json must be a JSON array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], "T-1");
    assert_eq!(arr[0]["estado"], "fazendo");
    assert_eq!(arr[1]["estado"], "a_fazer");
}

/// `current --json` with a task emits a single task OBJECT (not an array).
#[cfg(windows)]
#[test]
fn current_json_shape_is_object() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    let _srv = start_mock(
        &format!("cadenza-{user}"),
        vec![
            hello_ok(),
            serde_json::json!({
                "v": 1, "ok": true,
                "result": {"id": "T-1", "titulo": "a", "estado": "fazendo", "responsavel": "humano", "body": ""}
            }),
        ],
    );
    let out = cli(&user, data.path())
        .args(["current", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("stdout is valid JSON");
    assert!(v.is_object(), "current --json with a task is an object");
    assert_eq!(v["id"], "T-1");
    assert_eq!(v["estado"], "fazendo");
}

/// `current --json` with no fazendo task prints `null`.
#[cfg(windows)]
#[test]
fn current_json_shape_null_when_empty() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    let _srv = start_mock(
        &format!("cadenza-{user}"),
        vec![
            hello_ok(),
            serde_json::json!({"v": 1, "ok": true, "result": null}),
        ],
    );
    cli(&user, data.path())
        .args(["current", "--json"])
        .assert()
        .success()
        .stdout("null\n");
}

/// `log --json` prints `{"ok":true}` and exits 0 on success.
#[cfg(windows)]
#[test]
fn log_json_shape_ok_true() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    let _srv = start_mock(
        &format!("cadenza-{user}"),
        vec![
            hello_ok(),
            serde_json::json!({"v": 1, "ok": true, "result": {"ok": true}}),
        ],
    );
    let out = cli(&user, data.path())
        .args(["log", "T-1", "progress", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("stdout is valid JSON");
    assert_eq!(v["ok"], true);
}

/// `done --json` prints `{"ok":true,"idempotency_key":"..."}` on success — the
/// resolved key is echoed in the JSON shape (and stderr).
#[cfg(windows)]
#[test]
fn done_json_shape_carries_idempotency_key() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    let _srv = start_mock(
        &format!("cadenza-{user}"),
        vec![
            hello_ok(),
            serde_json::json!({"v": 1, "ok": true, "result": {"ok": true}}),
        ],
    );
    let out = cli(&user, data.path())
        .args([
            "done",
            "T-1",
            "all good",
            "--idempotency-key",
            "key-abc",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("stdout is valid JSON");
    assert_eq!(v["ok"], true);
    assert_eq!(v["idempotency_key"], "key-abc");
}

// ─────────────────────────────────────────────────────────────────────────────
// Stable wire-mapped exit codes (Windows mock server)
// ─────────────────────────────────────────────────────────────────────────────

/// Exit 12 — `protocol_too_new` on hello → protocol mismatch.
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
        .args(["list"])
        .assert()
        .failure()
        .code(12);
}

/// Exit 30 — `task_not_found` on a read op.
#[cfg(windows)]
#[test]
fn exit_30_task_not_found() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    let _srv = start_mock(
        &format!("cadenza-{user}"),
        vec![hello_ok(), err_resp("task_not_found", "T-x")],
    );
    cli(&user, data.path())
        .args(["get", "T-x"])
        .assert()
        .failure()
        .code(30);
}

/// Exit 21 — `decision_timeout` on await_decision (after a successful propose).
#[cfg(windows)]
#[test]
fn exit_21_decision_timeout() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    let _srv = start_mock(
        &format!("cadenza-{user}"),
        vec![
            hello_ok(),
            serde_json::json!({"v": 1, "ok": true, "result": {"proposta_id": "P-1"}}),
            err_resp("decision_timeout", "no decision in time"),
        ],
    );
    cli(&user, data.path())
        .args([
            "propose",
            "--title",
            "t",
            "--repro",
            "r",
            "--file",
            "src/foo.rs",
            "--what-failed",
            "w",
            "--action",
            "a",
        ])
        .assert()
        .failure()
        .code(21);
}

/// Exit 20 — a `rejeitada` decision is converted by the CLI into
/// `proposal_rejected` → exit 20.
#[cfg(windows)]
#[test]
fn exit_20_proposal_rejected() {
    let user = unique_user();
    let data = make_data_dir(Some("any-token"));
    let _srv = start_mock(
        &format!("cadenza-{user}"),
        vec![
            hello_ok(),
            serde_json::json!({"v": 1, "ok": true, "result": {"proposta_id": "P-1"}}),
            serde_json::json!({
                "v": 1, "ok": true,
                "result": {
                    "proposta_id": "P-1",
                    "decisao": "rejeitada",
                    "task_id": null,
                    "autor": "humano",
                    "decided_at_ms": 0
                }
            }),
        ],
    );
    cli(&user, data.path())
        .args([
            "propose",
            "--title",
            "t",
            "--repro",
            "r",
            "--file",
            "src/foo.rs",
            "--what-failed",
            "w",
            "--action",
            "a",
        ])
        .assert()
        .failure()
        .code(20);
}
