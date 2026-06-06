//! NDJSON client over the local socket (named pipe on Windows,
//! Unix socket elsewhere).
//!
//! See DESIGN-desktop-v2.md § "Protocolo IPC" for the wire shape.
//! Exit codes are honored by `main.rs` by inspecting `WireError::code`.

use anyhow::{anyhow, Context, Result};
use cadenza_proto::{
    ops,
    wire::{ErrorBody, Request, Response, ServerFrame},
    MAX_PROTOCOL,
};
use interprocess::local_socket::tokio::{prelude::*, Stream};
#[cfg(not(windows))]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use serde::{de::DeserializeOwned, Serialize};
use std::io::ErrorKind;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Errors that should map to specific CLI exit codes.
#[derive(Debug)]
pub struct WireError(pub ErrorBody);

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.0.code, self.0.message)
    }
}
impl std::error::Error for WireError {}

impl WireError {
    /// Map error codes per DESIGN-desktop-v2.md § "Exit codes".
    pub fn exit_code(&self) -> i32 {
        match self.0.code.as_str() {
            "auth_failed" => 11,
            "protocol_too_old" | "protocol_too_new" => 12,
            // "task_not_found" e "unknown_project" são ambos do mesmo
            // perfil: o agente referenciou um recurso que não existe.
            // Mapear pro mesmo exit code mantém o contrato simples
            // (`30 = recurso não encontrado`).
            "task_not_found" | "unknown_project" => 30,
            "decision_timeout" => 21,
            // A forged Jira identity on the public propose surface is a
            // rejected proposal (same family as proposal_rejected).
            "proposal_rejected" | "jira_identity_forbidden" => 20,
            // Capability-secret failures (Slice 2) are auth-family, like
            // auth_failed.
            "run_secret_invalid" | "run_secret_expired" | "run_secret_revoked" => 11,
            // Bad decomposition payload is a usage error (exit 2),
            // consistent with the CLI's other bad-usage exits.
            "invalid_decomposition" => 2,
            // Jira data layer (Slice 3). Without these arms the new codes
            // would fall through to the generic exit 1.
            "jira_config" => 2,       // bad usage / misconfiguration
            "jira_auth" => 11,        // auth family (like auth_failed)
            "jira_not_found" => 30,   // resource not found (like task_not_found)
            "jira_rate_limited" => 1, // transient; generic failure
            "jira_cancelled" => 1,
            "jira_http" | "jira_transport" | "jira_parse" => 1,
            // Jira shared-worktree lifecycle (Slice 4). Neither is auth(11),
            // usage(2), not-found(30), rejected-proposal(20), nor
            // timeout(21): worktree-busy is transient contention (like
            // `jira_rate_limited`) and worktree-failed is an operational
            // failure (like `jira_http`/`jira_transport`), so both take the
            // generic, retryable exit 1.
            "jira_worktree_busy" => 1,
            "jira_worktree_failed" => 1,
            // Jira aggregate review (Slice 5). `jira_worktree_not_ready` is a
            // transient precondition (the worktree becomes Ready after the
            // Slice-4 `ensure` runs), like `jira_worktree_busy`;
            // `jira_review_failed` is an operational diff failure like
            // `jira_http`. Both take the generic, retryable exit 1.
            // (`jira_not_found` from a missing record already maps to 30.)
            "jira_worktree_not_ready" => 1,
            "jira_review_failed" => 1,
            // Jira import orchestration + discard (Slice 6a). `jira_import_failed`
            // is an operational failure (mint/spawn/store) and `jira_worktree_dirty`
            // is a transient refusal (pass --force) — both take the generic,
            // retryable exit 1, the same family as `jira_worktree_failed`.
            "jira_import_failed" => 1,
            "jira_worktree_dirty" => 1,
            _ => 1,
        }
    }
}

/// "App not running" condition — `connect` could not reach the socket.
#[derive(Debug)]
pub struct AppNotRunning(pub std::io::Error);
impl std::fmt::Display for AppNotRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cadenza app is not running ({})", self.0)
    }
}
impl std::error::Error for AppNotRunning {}

pub struct Client {
    reader: tokio::io::Lines<BufReader<tokio::io::ReadHalf<Stream>>>,
    writer: tokio::io::WriteHalf<Stream>,
    next_id: u64,
}

impl Client {
    pub async fn connect() -> Result<Self> {
        let stream = open_stream()
            .await
            .map_err(|e| anyhow::Error::new(AppNotRunning(e)))?;
        let (read, write) = tokio::io::split(stream);
        Ok(Self {
            reader: BufReader::new(read).lines(),
            writer: write,
            next_id: 1,
        })
    }

    pub async fn hello(&mut self, token: &str) -> Result<ops::hello::Result> {
        let args = ops::hello::Args {
            protocol: MAX_PROTOCOL,
            client: format!("cadenza-cli/{}", env!("CARGO_PKG_VERSION")),
            token: token.to_string(),
        };
        let req = Request::new(None, ops::OP_HELLO, args)?;
        self.send_frame(&req).await?;
        loop {
            match self.next_frame().await? {
                ServerFrame::Response(r) => {
                    return self.decode_response(r);
                }
                ServerFrame::Event(_) => continue, // unlikely pre-hello, but tolerate
            }
        }
    }

    pub async fn request<A, R>(&mut self, op: &str, args: A) -> Result<R>
    where
        A: Serialize,
        R: DeserializeOwned,
    {
        let id = self.mint_id();
        let req = Request::new(Some(id.clone()), op, args)?;
        self.send_frame(&req).await?;
        loop {
            match self.next_frame().await? {
                ServerFrame::Response(r) if r.id.as_deref() == Some(&id) => {
                    return self.decode_response(r);
                }
                ServerFrame::Response(r) if r.id.is_none() && !r.ok => {
                    // Connection-level error from the server (e.g.
                    // line_too_long, bad_frame). The server can't know
                    // which request it applies to — surface it to the
                    // pending request so we don't hang waiting for a
                    // correlated reply that will never arrive.
                    let err = r
                        .error
                        .unwrap_or_else(|| ErrorBody::new("missing_error", ""));
                    return Err(anyhow::Error::new(WireError(err)));
                }
                ServerFrame::Response(r) => {
                    tracing::warn!(got = ?r.id, want = %id, "stray response id, ignoring");
                }
                ServerFrame::Event(e) => {
                    tracing::debug!(event = %e.event, "event during request, ignoring");
                }
            }
        }
    }

    /// Like `request`, but surfaces `proposta_pendente` events to stderr
    /// so the human watching the CLI knows we're blocked.
    pub async fn await_decision(
        &mut self,
        args: ops::await_decision::Args,
    ) -> Result<ops::await_decision::Result> {
        let id = self.mint_id();
        let req = Request::new(Some(id.clone()), ops::OP_AWAIT_DECISION, args)?;
        self.send_frame(&req).await?;
        loop {
            match self.next_frame().await? {
                ServerFrame::Response(r) if r.id.as_deref() == Some(&id) => {
                    return self.decode_response(r);
                }
                ServerFrame::Response(r) if r.id.is_none() && !r.ok => {
                    let err = r
                        .error
                        .unwrap_or_else(|| ErrorBody::new("missing_error", ""));
                    return Err(anyhow::Error::new(WireError(err)));
                }
                ServerFrame::Response(r) => {
                    tracing::warn!(got = ?r.id, want = %id, "stray response id");
                }
                ServerFrame::Event(e) if e.event == ops::EV_PROPOSTA_PENDENTE => {
                    eprintln!("proposta pendente — aguardando humano…");
                }
                ServerFrame::Event(e) => {
                    tracing::debug!(event = %e.event, "event");
                }
            }
        }
    }

    fn mint_id(&mut self) -> String {
        let s = self.next_id.to_string();
        self.next_id += 1;
        s
    }

    async fn send_frame<T: Serialize>(&mut self, frame: &T) -> Result<()> {
        let s = serde_json::to_string(frame)?;
        self.writer.write_all(s.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;
        Ok(())
    }

    async fn next_frame(&mut self) -> Result<ServerFrame> {
        let line = self
            .reader
            .next_line()
            .await
            .context("read from server")?
            .ok_or_else(|| anyhow!("server closed connection"))?;
        let frame: ServerFrame =
            serde_json::from_str(&line).with_context(|| format!("parse frame: {line}"))?;
        Ok(frame)
    }

    fn decode_response<R: DeserializeOwned>(&self, r: Response) -> Result<R> {
        if r.ok {
            let value = r
                .result
                .ok_or_else(|| anyhow!("server returned ok=true with no result"))?;
            Ok(serde_json::from_value(value)?)
        } else {
            let err = r
                .error
                .unwrap_or_else(|| ErrorBody::new("missing_error", ""));
            Err(anyhow::Error::new(WireError(err)))
        }
    }
}

async fn open_stream() -> std::io::Result<Stream> {
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
        let raw = format!("cadenza-{user}");
        let name = raw
            .as_str()
            .to_ns_name::<GenericNamespaced>()
            .map_err(|e| std::io::Error::new(ErrorKind::InvalidInput, e))?;
        Stream::connect(name).await
    }
    #[cfg(not(windows))]
    {
        // Honor CADENZA_DATA_DIR (via crate::data_dir()) so integration
        // tests that point at a temp directory don't accidentally connect
        // to the developer's real `~/.cadenza/run/socket`.
        let path = crate::data_dir().join("run").join("socket");
        let name = path
            .as_path()
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| std::io::Error::new(ErrorKind::InvalidInput, e))?;
        Stream::connect(name).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exit_for(code: &str) -> i32 {
        WireError(ErrorBody::new(code, "x")).exit_code()
    }

    #[test]
    fn exit_code_run_secret_invalid_is_11() {
        assert_eq!(exit_for("run_secret_invalid"), 11);
    }

    #[test]
    fn exit_code_run_secret_expired_is_11() {
        assert_eq!(exit_for("run_secret_expired"), 11);
    }

    #[test]
    fn exit_code_run_secret_revoked_is_11() {
        assert_eq!(exit_for("run_secret_revoked"), 11);
    }

    #[test]
    fn exit_code_invalid_decomposition_is_2() {
        assert_eq!(exit_for("invalid_decomposition"), 2);
    }

    #[test]
    fn exit_code_jira_identity_forbidden_is_20() {
        assert_eq!(exit_for("jira_identity_forbidden"), 20);
    }

    #[test]
    fn exit_code_jira_config_is_2() {
        assert_eq!(exit_for("jira_config"), 2);
    }

    #[test]
    fn exit_code_jira_auth_is_11() {
        assert_eq!(exit_for("jira_auth"), 11);
    }

    #[test]
    fn exit_code_jira_not_found_is_30() {
        assert_eq!(exit_for("jira_not_found"), 30);
    }

    #[test]
    fn exit_code_jira_worktree_busy_is_1() {
        assert_eq!(exit_for("jira_worktree_busy"), 1);
    }

    #[test]
    fn exit_code_jira_worktree_failed_is_1() {
        assert_eq!(exit_for("jira_worktree_failed"), 1);
    }

    #[test]
    fn exit_code_jira_worktree_not_ready_is_1() {
        assert_eq!(exit_for("jira_worktree_not_ready"), 1);
    }

    #[test]
    fn exit_code_jira_review_failed_is_1() {
        assert_eq!(exit_for("jira_review_failed"), 1);
    }

    #[test]
    fn exit_code_jira_import_failed_is_1() {
        assert_eq!(exit_for("jira_import_failed"), 1);
    }

    #[test]
    fn exit_code_jira_worktree_dirty_is_1() {
        assert_eq!(exit_for("jira_worktree_dirty"), 1);
    }
}
