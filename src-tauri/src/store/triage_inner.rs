//! Triage — persist proposals and decisions under `~/.cadenza/triage/`.
//!
//! Per DESIGN-desktop-v2.md § "`propose` resiliente":
//! - Every proposal carries a client-generated `idempotency_key` (uuid v4).
//! - Re-sending `propose` with the same key returns the existing
//!   `proposta_id` (at-most-one task per key).
//! - Either side can crash and reconnect; `await_decisao` returns
//!   immediately if the decision was already written.
//!
//! Wired into Tauri commands + IPC in Phase 3-4.
#![allow(dead_code)]

use anyhow::Context;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Notify;
use uuid::Uuid;

// Re-export so existing callers (commands.rs, etc.) keep working.
#[allow(unused_imports)]
pub use cadenza_proto::{Decisao, DecisaoRegistro, NewProposta, Proposta};

#[derive(Error, Debug)]
pub enum TriageError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("anyhow: {0}")]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, TriageError>;

#[derive(Default)]
struct State {
    /// idempotency_key → proposta_id (built at recovery time, kept in sync on writes).
    by_key: HashMap<String, String>,
    /// proposta_id → Notify woken when a decision is written.
    waiters: HashMap<String, Arc<Notify>>,
}

/// Filesystem-backed triage store with in-memory dedup map and waiters.
pub struct Triage {
    root: PathBuf,
    state: Mutex<State>,
}

impl Triage {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let triage = Triage {
            root,
            state: Mutex::new(State::default()),
        };
        triage.recover()?;
        Ok(triage)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn proposta_path(&self, proposta_id: &str) -> PathBuf {
        self.root.join(format!("{proposta_id}.proposta.json"))
    }

    fn decisao_path(&self, proposta_id: &str) -> PathBuf {
        self.root.join(format!("{proposta_id}.decisao.json"))
    }

    /// Scan disk and rebuild the dedup map. Idempotent — safe to call
    /// repeatedly. Returns the list of currently pending proposals
    /// (those without a matching `.decisao.json`).
    pub fn recover(&self) -> Result<Vec<Proposta>> {
        let mut state = self.state.lock().unwrap();
        state.by_key.clear();
        let mut pending = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !name.ends_with(".proposta.json") {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let proposta: Proposta = match serde_json::from_str(&content) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(path = ?path, error = %e, "skipping malformed proposta");
                    continue;
                }
            };
            state.by_key.insert(
                proposta.idempotency_key.clone(),
                proposta.proposta_id.clone(),
            );
            let decisao_exists = self.decisao_path(&proposta.proposta_id).exists();
            if !decisao_exists {
                pending.push(proposta);
            }
        }
        Ok(pending)
    }

    /// Create the proposal — or return the existing one if the
    /// `idempotency_key` was seen before.
    pub fn propose(&self, args: NewProposta) -> Result<Proposta> {
        // Step 1: dedup check, briefly under lock.
        let existing = {
            let state = self.state.lock().unwrap();
            state.by_key.get(&args.idempotency_key).cloned()
        };
        if let Some(existing_id) = existing {
            if let Some(p) = self.read_proposta(&existing_id)? {
                return Ok(p);
            }
            // Stale map entry (file vanished out from under us) — fall
            // through to recreate. The map will be overwritten below.
        }

        // Step 2: mint, persist, then record the dedup mapping. We
        // accept a vanishingly small race where two threads pass the
        // fast check concurrently and both write distinct files; on
        // a desktop with one CLI per key this never happens, and
        // `recover()` would converge anyway.
        let proposta_id = format!("P-{}", Uuid::new_v4().simple());
        let proposta = Proposta {
            proposta_id: proposta_id.clone(),
            idempotency_key: args.idempotency_key.clone(),
            parent: args.parent,
            title: args.title,
            repro: args.repro,
            file: args.file,
            what_failed: args.what_failed,
            action: args.action,
            created_at_ms: now_ms(),
        };
        write_json_atomic(&self.proposta_path(&proposta_id), &proposta)?;
        {
            let mut state = self.state.lock().unwrap();
            state.by_key.insert(args.idempotency_key, proposta_id);
        }
        Ok(proposta)
    }

    pub fn read_proposta(&self, proposta_id: &str) -> Result<Option<Proposta>> {
        let path = self.proposta_path(proposta_id);
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)?;
        let p: Proposta = serde_json::from_str(&text)?;
        Ok(Some(p))
    }

    pub fn read_decisao(&self, proposta_id: &str) -> Result<Option<DecisaoRegistro>> {
        let path = self.decisao_path(proposta_id);
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)?;
        let d: DecisaoRegistro = serde_json::from_str(&text)?;
        Ok(Some(d))
    }

    pub fn list_pending(&self) -> Result<Vec<Proposta>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !name.ends_with(".proposta.json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(p): std::result::Result<Proposta, _> = serde_json::from_str(&text) else {
                continue;
            };
            if !self.decisao_path(&p.proposta_id).exists() {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// Persist the decision and wake any waiter blocked on `await_decisao`.
    pub fn write_decisao(&self, registro: DecisaoRegistro) -> Result<()> {
        write_json_atomic(&self.decisao_path(&registro.proposta_id), &registro)?;
        let mut state = self.state.lock().unwrap();
        if let Some(notify) = state.waiters.remove(&registro.proposta_id) {
            notify.notify_waiters();
        }
        Ok(())
    }

    /// Block until a decision is written for `proposta_id` or until
    /// `timeout` elapses. `None` means timeout.
    pub async fn await_decisao(
        &self,
        proposta_id: &str,
        timeout: Duration,
    ) -> Result<Option<DecisaoRegistro>> {
        // Fast path: decision already on disk.
        if let Some(d) = self.read_decisao(proposta_id)? {
            return Ok(Some(d));
        }

        // Register / reuse a waiter, then arm and double-check disk
        // before parking. `Notify::notify_waiters` stores no permit,
        // so a future that isn't yet armed when notify fires misses
        // the wake — pinning + `enable()` registers the listener now.
        let notify = {
            let mut state = self.state.lock().unwrap();
            state
                .waiters
                .entry(proposta_id.to_string())
                .or_insert_with(|| Arc::new(Notify::new()))
                .clone()
        };
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(d) = self.read_decisao(proposta_id)? {
            return Ok(Some(d));
        }

        match tokio::time::timeout(timeout, notified).await {
            Ok(()) => Ok(self.read_decisao(proposta_id)?),
            Err(_) => Ok(None),
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    let tmp = parent.join(format!(".{}.tmp", Uuid::new_v4().simple()));
    {
        let f = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        serde_json::to_writer_pretty(&f, value)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn mk_args(key: &str, title: &str) -> NewProposta {
        NewProposta {
            idempotency_key: key.into(),
            parent: Some("T-1".into()),
            title: title.into(),
            repro: "...".into(),
            file: "src/foo.rs".into(),
            what_failed: "panic".into(),
            action: "fix bounds check".into(),
        }
    }

    #[test]
    fn propose_persists_and_returns_proposta() {
        let dir = TempDir::new().unwrap();
        let t = Triage::new(dir.path()).unwrap();
        let p = t.propose(mk_args("k1", "first")).unwrap();
        assert!(p.proposta_id.starts_with("P-"));
        assert_eq!(p.idempotency_key, "k1");
        let on_disk = t.read_proposta(&p.proposta_id).unwrap().unwrap();
        assert_eq!(on_disk.title, "first");
    }

    #[test]
    fn propose_is_idempotent_on_key() {
        let dir = TempDir::new().unwrap();
        let t = Triage::new(dir.path()).unwrap();
        let p1 = t.propose(mk_args("same-key", "first")).unwrap();
        // Even a totally different title with the same key returns p1's id.
        let p2 = t.propose(mk_args("same-key", "different")).unwrap();
        assert_eq!(p1.proposta_id, p2.proposta_id);
        assert_eq!(p2.title, "first"); // original wins
    }

    #[test]
    fn different_keys_make_different_propostas() {
        let dir = TempDir::new().unwrap();
        let t = Triage::new(dir.path()).unwrap();
        let p1 = t.propose(mk_args("k1", "one")).unwrap();
        let p2 = t.propose(mk_args("k2", "two")).unwrap();
        assert_ne!(p1.proposta_id, p2.proposta_id);
    }

    #[test]
    fn decisao_round_trip() {
        let dir = TempDir::new().unwrap();
        let t = Triage::new(dir.path()).unwrap();
        let p = t.propose(mk_args("k", "x")).unwrap();
        let d = DecisaoRegistro {
            proposta_id: p.proposta_id.clone(),
            decisao: Decisao::Aceita,
            task_id: Some("T-99".into()),
            autor: "humano via modal".into(),
            decided_at_ms: 12345,
        };
        t.write_decisao(d.clone()).unwrap();
        let got = t.read_decisao(&p.proposta_id).unwrap().unwrap();
        assert_eq!(got.decisao, Decisao::Aceita);
        assert_eq!(got.task_id.as_deref(), Some("T-99"));
    }

    #[test]
    fn list_pending_excludes_decided() {
        let dir = TempDir::new().unwrap();
        let t = Triage::new(dir.path()).unwrap();
        let p1 = t.propose(mk_args("k1", "one")).unwrap();
        let p2 = t.propose(mk_args("k2", "two")).unwrap();
        let _p3 = t.propose(mk_args("k3", "three")).unwrap();

        t.write_decisao(DecisaoRegistro {
            proposta_id: p1.proposta_id.clone(),
            decisao: Decisao::Rejeitada,
            task_id: None,
            autor: "h".into(),
            decided_at_ms: 0,
        })
        .unwrap();

        let pending = t.list_pending().unwrap();
        assert_eq!(pending.len(), 2);
        let pending_ids: Vec<&str> = pending.iter().map(|p| p.proposta_id.as_str()).collect();
        assert!(!pending_ids.contains(&p1.proposta_id.as_str()));
        assert!(pending_ids.contains(&p2.proposta_id.as_str()));
    }

    #[test]
    fn recover_rebuilds_dedup_map() {
        let dir = TempDir::new().unwrap();
        // First triage instance writes a proposta then drops.
        let original_id = {
            let t = Triage::new(dir.path()).unwrap();
            let p = t.propose(mk_args("recover-key", "x")).unwrap();
            p.proposta_id
        };
        // A fresh instance should reuse the same id when the key repeats.
        let t2 = Triage::new(dir.path()).unwrap();
        let p2 = t2.propose(mk_args("recover-key", "y")).unwrap();
        assert_eq!(p2.proposta_id, original_id);
    }

    // Verifies the "re-announce on startup" contract: after a restart the
    // fresh Triage scans triage/ and returns only proposals that have no
    // matching .decisao.json — decided proposals must be silently skipped.
    #[test]
    fn recover_returns_only_undecided_on_restart() {
        let dir = TempDir::new().unwrap();

        let (decided_id, pending_id) = {
            let triage = Triage::new(dir.path()).unwrap();
            let p1 = triage.propose(mk_args("k1", "one")).unwrap();
            let p2 = triage.propose(mk_args("k2", "two")).unwrap();
            triage
                .write_decisao(DecisaoRegistro {
                    proposta_id: p1.proposta_id.clone(),
                    decisao: Decisao::Aceita,
                    task_id: None,
                    autor: "h".into(),
                    decided_at_ms: 0,
                })
                .unwrap();
            (p1.proposta_id, p2.proposta_id)
        };

        // Simulate restart with a fresh instance.
        let t2 = Triage::new(dir.path()).unwrap();
        let pending = t2.recover().unwrap();

        assert_eq!(
            pending.len(),
            1,
            "decided proposal must not be re-announced"
        );
        assert_eq!(pending[0].proposta_id, pending_id);
        let _ = decided_id;
    }

    #[tokio::test]
    async fn await_decisao_fast_path_when_already_decided() {
        let dir = TempDir::new().unwrap();
        let t = Triage::new(dir.path()).unwrap();
        let p = t.propose(mk_args("k", "x")).unwrap();
        t.write_decisao(DecisaoRegistro {
            proposta_id: p.proposta_id.clone(),
            decisao: Decisao::Aceita,
            task_id: Some("T-42".into()),
            autor: "h".into(),
            decided_at_ms: 0,
        })
        .unwrap();

        let got = t
            .await_decisao(&p.proposta_id, Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(got.unwrap().decisao, Decisao::Aceita);
    }

    #[tokio::test]
    async fn await_decisao_times_out() {
        let dir = TempDir::new().unwrap();
        let t = Triage::new(dir.path()).unwrap();
        let p = t.propose(mk_args("k", "x")).unwrap();
        let got = t
            .await_decisao(&p.proposta_id, Duration::from_millis(50))
            .await
            .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn await_decisao_wakes_on_write() {
        let dir = TempDir::new().unwrap();
        let triage = Arc::new(Triage::new(dir.path()).unwrap());
        let p = triage.propose(mk_args("k", "x")).unwrap();

        let proposta_id = p.proposta_id.clone();
        let writer = triage.clone();
        let pid = proposta_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            writer
                .write_decisao(DecisaoRegistro {
                    proposta_id: pid,
                    decisao: Decisao::Mesclada,
                    task_id: Some("T-77".into()),
                    autor: "h".into(),
                    decided_at_ms: 0,
                })
                .unwrap();
        });

        let got = triage
            .await_decisao(&proposta_id, Duration::from_secs(2))
            .await
            .unwrap();
        let d = got.expect("waiter should have been notified before timeout");
        assert_eq!(d.decisao, Decisao::Mesclada);
        assert_eq!(d.task_id.as_deref(), Some("T-77"));
    }
}
