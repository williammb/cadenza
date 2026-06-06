//! Review packages — filesystem backend under `<home>/reviews/`.
//!
//! Mirrors the triage sidecar engine (`triage_inner.rs`): one JSON file per
//! `done` attempt, written atomically (fsync-then-rename), plus an
//! in-memory dedup/version index rebuilt by `recover()` on startup.
//!
//! Two pieces (PLAN §C.9, §F.17, §F.18):
//!
//! 1. **Sidecar package files** — `<task_id>.a<attempt>.review.json`, the
//!    full [`ReviewPackage`] JSON. Identity is `(task_id, attempt)`; the
//!    `(task_id, idempotency_key)` pair is the durable dedup key.
//! 2. **A write-ahead journal** — `done-<sha256(idempotency_key)>.journal`
//!    makes the multi-file `done` (sidecar write + supersede priors + task
//!    `.md` log/state flip, the latter driven by the caller) crash-atomic.
//!    The raw idempotency key is NEVER a path component: only its hash names
//!    the journal file; the raw key lives inside the journal JSON.
//!
//! The bare [`Reviews::upsert`] used by `copy_all` and tests writes ONLY the
//! sidecar (attempt allocation + supersede + file write + index update). The
//! `done` op orchestration (a separate layer) wraps that intent in a journal
//! record and drives the full multi-file commit via [`Reviews::apply_done`].
#![allow(dead_code)]

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use thiserror::Error;
use uuid::Uuid;

use crate::review::{PackageStatus, ReviewPackage};

/// Current journal record schema version.
const JOURNAL_VERSION: u32 = 1;

#[derive(Error, Debug)]
pub enum ReviewError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("bad data: {0}")]
    BadData(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("anyhow: {0}")]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, ReviewError>;

/// One write-ahead journal record for an atomic `done` (PLAN §C.9). The raw
/// `idempotency_key` lives here only — never in the journal's filename.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoneJournal {
    pub v: u32,
    /// Raw key (the journal filename uses `sha256(idempotency_key)` instead).
    pub idempotency_key: String,
    pub task_id: String,
    /// Backend-allocated attempt for the new package.
    pub attempt: u32,
    /// Full package payload to write to the sidecar.
    pub package: ReviewPackage,
    /// Exact line to append to `<task_id>.md`; `None` skips the append.
    #[serde(default)]
    pub log_line: Option<String>,
    /// Target estado string to set on the task; `None` skips the flip.
    #[serde(default)]
    pub target_estado: Option<String>,
    /// Prior attempts to mark superseded.
    #[serde(default)]
    pub supersede_attempts: Vec<u32>,
}

#[derive(Default)]
struct ReviewState {
    /// `(task_id, idempotency_key)` -> attempt, rebuilt by `recover()`.
    by_key: HashMap<(String, String), u32>,
    /// `task_id` -> max attempt seen, for O(1) next-attempt allocation.
    max_attempt: HashMap<String, u32>,
}

/// Outcome of attempt allocation under the lock: either the package already
/// exists for this key (no-op) or a fresh `(attempt, supersede_vec)` pair.
enum Allocation {
    Existing(Box<ReviewPackage>),
    Fresh { attempt: u32, supersede: Vec<u32> },
}

/// Filesystem-backed review-package store rooted at `<home>/reviews/`.
pub struct Reviews {
    root: PathBuf,
    state: Mutex<ReviewState>,
}

impl Reviews {
    /// Open (creating the dir) and replay any dangling journals + rebuild
    /// the dedup/version index. Runs once per process at `FileRepository::new`.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let reviews = Reviews {
            root,
            state: Mutex::new(ReviewState::default()),
        };
        reviews.recover()?;
        Ok(reviews)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn package_path(&self, task_id: &str, attempt: u32) -> PathBuf {
        self.root.join(format!("{task_id}.a{attempt}.review.json"))
    }

    fn journal_path(&self, key_hash: &str) -> PathBuf {
        self.root.join(format!("done-{key_hash}.journal"))
    }

    /// Startup recovery (mirrors `Triage::recover`):
    /// 1. Replay every dangling `done-*.journal` FIRST so a crash mid-`done`
    ///    is healed before the index is read. Replay failures are logged and
    ///    the journal is left in place for the next boot — they never abort
    ///    startup.
    /// 2. Rebuild `by_key` + `max_attempt` from `*.review.json`. Malformed
    ///    files are logged and skipped.
    pub fn recover(&self) -> Result<()> {
        // Phase 1: replay dangling journals.
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !(name.starts_with("done-") && name.ends_with(".journal")) {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let record: DoneJournal = match serde_json::from_str(&content) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(path = ?path, error = %e, "skipping malformed done journal");
                    continue;
                }
            };
            if let Err(e) = self.apply_done(&record) {
                tracing::warn!(path = ?path, error = ?e, "failed to replay done journal; left in place");
            }
        }

        // Phase 2: rebuild the index from sidecars.
        let mut state = self.state.lock().unwrap();
        state.by_key.clear();
        state.max_attempt.clear();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !name.ends_with(".review.json") {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let pkg: ReviewPackage = match serde_json::from_str(&content) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(path = ?path, error = %e, "skipping malformed review package");
                    continue;
                }
            };
            state.by_key.insert(
                (pkg.task_id.clone(), pkg.idempotency_key.clone()),
                pkg.attempt,
            );
            let slot = state.max_attempt.entry(pkg.task_id.clone()).or_insert(0);
            if pkg.attempt > *slot {
                *slot = pkg.attempt;
            }
        }
        Ok(())
    }

    /// Allocate (or short-circuit on an existing key) under the lock.
    fn allocate(&self, task_id: &str, idempotency_key: &str) -> Result<Allocation> {
        let state = self.state.lock().unwrap();
        if let Some(att) = state
            .by_key
            .get(&(task_id.to_string(), idempotency_key.to_string()))
        {
            let att = *att;
            drop(state);
            let existing = self
                .read(task_id, att)?
                .ok_or_else(|| ReviewError::NotFound(format!("{task_id}.a{att}")))?;
            return Ok(Allocation::Existing(Box::new(existing)));
        }
        let max = state.max_attempt.get(task_id).copied().unwrap_or(0);
        let attempt = max + 1;
        let supersede: Vec<u32> = (1..attempt)
            .filter(|n| self.package_path(task_id, *n).exists())
            .collect();
        Ok(Allocation::Fresh { attempt, supersede })
    }

    /// Sidecar-only idempotent upsert (used by `copy_all` + tests). Returns
    /// the stored package unchanged when the key already exists. Otherwise
    /// allocates `attempt`, supersedes priors, writes the sidecar, and
    /// updates the index. The `attempt`/`status` fields of `pkg` are
    /// overwritten by the backend.
    pub fn upsert(&self, pkg: &ReviewPackage) -> Result<ReviewPackage> {
        super::validate_id(&pkg.task_id).map_err(map_validation)?;
        super::validate_idempotency_key(&pkg.idempotency_key).map_err(map_validation)?;
        match self.allocate(&pkg.task_id, &pkg.idempotency_key)? {
            Allocation::Existing(existing) => Ok(*existing),
            Allocation::Fresh { attempt, supersede } => {
                // Supersede priors (skip any already decided).
                for n in &supersede {
                    if let Some(mut prior) = self.read(&pkg.task_id, *n)? {
                        if prior.status == PackageStatus::Pending {
                            prior.status = PackageStatus::Superseded;
                            write_json_atomic(&self.package_path(&pkg.task_id, *n), &prior)?;
                        }
                    }
                }
                let mut stored = pkg.clone();
                stored.attempt = attempt;
                stored.status = PackageStatus::Pending;
                write_json_atomic(&self.package_path(&pkg.task_id, attempt), &stored)?;
                self.record_index(&stored);
                Ok(stored)
            }
        }
    }

    /// Update the in-memory index after a sidecar write.
    fn record_index(&self, pkg: &ReviewPackage) {
        let mut state = self.state.lock().unwrap();
        state.by_key.insert(
            (pkg.task_id.clone(), pkg.idempotency_key.clone()),
            pkg.attempt,
        );
        let slot = state.max_attempt.entry(pkg.task_id.clone()).or_insert(0);
        if pkg.attempt > *slot {
            *slot = pkg.attempt;
        }
    }

    /// Build a journal record for the `done` op: validates, allocates the
    /// attempt, and returns either the existing package (key already seen —
    /// caller treats as a no-op) or a ready-to-commit [`DoneJournal`].
    pub fn prepare_done(
        &self,
        pkg: &ReviewPackage,
        log_line: Option<String>,
        target_estado: Option<String>,
    ) -> Result<std::result::Result<DoneJournal, ReviewPackage>> {
        super::validate_id(&pkg.task_id).map_err(map_validation)?;
        super::validate_idempotency_key(&pkg.idempotency_key).map_err(map_validation)?;
        match self.allocate(&pkg.task_id, &pkg.idempotency_key)? {
            Allocation::Existing(existing) => Ok(Err(*existing)),
            Allocation::Fresh { attempt, supersede } => {
                let mut stored = pkg.clone();
                stored.attempt = attempt;
                stored.status = PackageStatus::Pending;
                Ok(Ok(DoneJournal {
                    v: JOURNAL_VERSION,
                    idempotency_key: pkg.idempotency_key.clone(),
                    task_id: pkg.task_id.clone(),
                    attempt,
                    package: stored,
                    log_line,
                    target_estado,
                    supersede_attempts: supersede,
                }))
            }
        }
    }

    /// Commit a prepared journal: write the WAL file, then apply every
    /// idempotent step, then remove the WAL. The same method is used both on
    /// the live `done` path and by `recover()` replay, so a crash anywhere is
    /// safely re-applied to completion. The task `.md` log/state side effects
    /// are delegated to `task_ops` so this engine stays focused on its
    /// sidecars (the caller wires the task store).
    pub fn commit_done<F>(&self, record: &DoneJournal, task_ops: F) -> Result<ReviewPackage>
    where
        F: Fn(&DoneJournal) -> Result<()>,
    {
        let key_hash = hash_key(&record.idempotency_key);
        let journal = self.journal_path(&key_hash);
        write_json_atomic(&journal, record)?;
        self.apply_steps(record, Some(&task_ops))?;
        // The journal vanishing = "done fully applied".
        if journal.exists() {
            fs::remove_file(&journal).with_context(|| format!("remove {}", journal.display()))?;
        }
        Ok(record.package.clone())
    }

    /// Replay path: apply the sidecar side of a journal (used by `recover()`
    /// and as the building block of `commit_done`). Idempotent.
    fn apply_done(&self, record: &DoneJournal) -> Result<()> {
        self.apply_steps(record, None::<&fn(&DoneJournal) -> Result<()>>)?;
        // On a successful full replay, drop the journal so the next boot
        // doesn't retry it.
        let journal = self.journal_path(&hash_key(&record.idempotency_key));
        if journal.exists() {
            let _ = fs::remove_file(&journal);
        }
        Ok(())
    }

    /// Idempotent apply of all journal steps EXCEPT the final journal removal.
    /// `task_ops`, when present, runs the log-append + estado flip (the caller
    /// supplies it on the live path; `recover()` passes `None` because the
    /// task `.md` is owned by a separate store the caller wires in).
    fn apply_steps<F>(&self, record: &DoneJournal, task_ops: Option<&F>) -> Result<()>
    where
        F: Fn(&DoneJournal) -> Result<()>,
    {
        // a. supersede priors (no-op if already non-pending / file missing).
        for n in &record.supersede_attempts {
            if let Some(mut prior) = self.read(&record.task_id, *n)? {
                if prior.status == PackageStatus::Pending {
                    prior.status = PackageStatus::Superseded;
                    write_json_atomic(&self.package_path(&record.task_id, *n), &prior)?;
                }
            }
        }
        // b. write the new sidecar (overwrite-safe — same content on replay).
        write_json_atomic(
            &self.package_path(&record.task_id, record.attempt),
            &record.package,
        )?;
        // c/d. task `.md` log + estado (idempotent, owned by the caller).
        if let Some(ops) = task_ops {
            ops(record)?;
        }
        // index update.
        self.record_index(&record.package);
        Ok(())
    }

    pub fn read(&self, task_id: &str, attempt: u32) -> Result<Option<ReviewPackage>> {
        let path = self.package_path(task_id, attempt);
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)?;
        let pkg: ReviewPackage = serde_json::from_str(&text)?;
        Ok(Some(pkg))
    }

    /// Every package for a task, ordered by `attempt` ascending.
    pub fn list(&self, task_id: &str) -> Result<Vec<ReviewPackage>> {
        let mut out = Vec::new();
        let prefix = format!("{task_id}.a");
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !(name.starts_with(&prefix) && name.ends_with(".review.json")) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(pkg): std::result::Result<ReviewPackage, _> = serde_json::from_str(&text) else {
                continue;
            };
            // Guard against a different task whose id is a prefix of this one.
            if pkg.task_id != task_id {
                continue;
            }
            out.push(pkg);
        }
        out.sort_by_key(|p| p.attempt);
        Ok(out)
    }

    /// Mark every attempt except `except_attempt` as superseded.
    pub fn mark_superseded(&self, task_id: &str, except_attempt: u32) -> Result<()> {
        for pkg in self.list(task_id)? {
            if pkg.attempt == except_attempt {
                continue;
            }
            if pkg.status == PackageStatus::Pending {
                let mut updated = pkg;
                updated.status = PackageStatus::Superseded;
                write_json_atomic(&self.package_path(task_id, updated.attempt), &updated)?;
            }
        }
        Ok(())
    }

    /// Set the lifecycle status (decision) on one `(task_id, attempt)`.
    pub fn set_status(&self, task_id: &str, attempt: u32, status: PackageStatus) -> Result<()> {
        let mut pkg = self
            .read(task_id, attempt)?
            .ok_or_else(|| ReviewError::NotFound(format!("{task_id}.a{attempt}")))?;
        pkg.status = status;
        write_json_atomic(&self.package_path(task_id, attempt), &pkg)?;
        Ok(())
    }

    /// Remove every sidecar for a task plus any dangling `done-*.journal`
    /// whose record targets the same task, and drop the index entries.
    /// Idempotent.
    pub fn delete_all(&self, task_id: &str) -> Result<()> {
        let prefix = format!("{task_id}.a");
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.starts_with(&prefix) && name.ends_with(".review.json") {
                // Confirm the embedded task_id before deleting (prefix guard).
                if let Ok(text) = fs::read_to_string(&path) {
                    if let Ok(pkg) = serde_json::from_str::<ReviewPackage>(&text) {
                        if pkg.task_id != task_id {
                            continue;
                        }
                    }
                }
                let _ = fs::remove_file(&path);
            } else if name.starts_with("done-") && name.ends_with(".journal") {
                if let Ok(text) = fs::read_to_string(&path) {
                    if let Ok(record) = serde_json::from_str::<DoneJournal>(&text) {
                        if record.task_id == task_id {
                            let _ = fs::remove_file(&path);
                        }
                    }
                }
            }
        }
        let mut state = self.state.lock().unwrap();
        state.by_key.retain(|(t, _), _| t != task_id);
        state.max_attempt.remove(task_id);
        Ok(())
    }

    /// Every package across all tasks, ordered by `(task_id, attempt)`.
    pub fn all(&self) -> Result<Vec<ReviewPackage>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !name.ends_with(".review.json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(pkg): std::result::Result<ReviewPackage, _> = serde_json::from_str(&text) else {
                continue;
            };
            out.push(pkg);
        }
        out.sort_by(|a, b| a.task_id.cmp(&b.task_id).then(a.attempt.cmp(&b.attempt)));
        Ok(out)
    }
}

/// Translate a `store::StoreError` from the shared id/key validators into
/// the engine's local error type (both carry the same `BadData` semantics).
fn map_validation(e: super::StoreError) -> ReviewError {
    match e {
        super::StoreError::BadData(s) => ReviewError::BadData(s),
        super::StoreError::Io(e) => ReviewError::Io(e),
        other => ReviewError::BadData(other.to_string()),
    }
}

/// Hex of `sha256(key)` — the journal filename component. The raw key is
/// never used as a path component (PLAN §C.9).
fn hash_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
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
    use crate::review::{EvidenceState, RISK_HEURISTIC_VERSION};
    use tempfile::TempDir;

    fn mk_pkg(task_id: &str, key: &str) -> ReviewPackage {
        ReviewPackage {
            task_id: task_id.into(),
            attempt: 0,
            idempotency_key: key.into(),
            status: PackageStatus::Pending,
            checks: vec![],
            groups: vec![],
            open_questions: vec![],
            summary: "did it".into(),
            changed_files: vec![],
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
            risks: vec![],
            secret_matches: vec![],
            evidence_state: EvidenceState::NoValidation,
            needs_focused_human_review: false,
            validation_scope_unknown: false,
            base_sha: None,
            head_sha: None,
            worktree_fingerprint: None,
            contract_version: None,
            reported_contract_version: None,
            risk_heuristic_version: RISK_HEURISTIC_VERSION,
            created_at_ms: 1,
            collection_duration_ms: 0,
            collection_errors: vec![],
            truncated: false,
            uncommitted_patch: None,
        }
    }

    #[test]
    fn upsert_allocates_attempt_and_reads_back() {
        let dir = TempDir::new().unwrap();
        let r = Reviews::new(dir.path()).unwrap();
        let stored = r.upsert(&mk_pkg("T-1", "k1")).unwrap();
        assert_eq!(stored.attempt, 1);
        let back = r.read("T-1", 1).unwrap().unwrap();
        assert_eq!(back.idempotency_key, "k1");
    }

    #[test]
    fn upsert_is_idempotent_on_key() {
        let dir = TempDir::new().unwrap();
        let r = Reviews::new(dir.path()).unwrap();
        let a = r.upsert(&mk_pkg("T-1", "same")).unwrap();
        let b = r.upsert(&mk_pkg("T-1", "same")).unwrap();
        assert_eq!(a.attempt, b.attempt);
        assert_eq!(r.list("T-1").unwrap().len(), 1);
    }

    #[test]
    fn attempts_version_and_supersede() {
        let dir = TempDir::new().unwrap();
        let r = Reviews::new(dir.path()).unwrap();
        r.upsert(&mk_pkg("T-1", "k1")).unwrap();
        r.upsert(&mk_pkg("T-1", "k2")).unwrap();
        let third = r.upsert(&mk_pkg("T-1", "k3")).unwrap();
        assert_eq!(third.attempt, 3);
        let list = r.list("T-1").unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].status, PackageStatus::Superseded);
        assert_eq!(list[1].status, PackageStatus::Superseded);
        assert_eq!(list[2].status, PackageStatus::Pending);
    }

    #[test]
    fn set_status_and_not_found() {
        let dir = TempDir::new().unwrap();
        let r = Reviews::new(dir.path()).unwrap();
        r.upsert(&mk_pkg("T-1", "k1")).unwrap();
        r.set_status("T-1", 1, PackageStatus::Aprovado).unwrap();
        assert_eq!(
            r.read("T-1", 1).unwrap().unwrap().status,
            PackageStatus::Aprovado
        );
        assert!(matches!(
            r.set_status("T-1", 9, PackageStatus::Aprovado),
            Err(ReviewError::NotFound(_))
        ));
    }

    #[test]
    fn delete_all_removes_sidecars_idempotent() {
        let dir = TempDir::new().unwrap();
        let r = Reviews::new(dir.path()).unwrap();
        r.upsert(&mk_pkg("T-1", "k1")).unwrap();
        r.upsert(&mk_pkg("T-1", "k2")).unwrap();
        r.delete_all("T-1").unwrap();
        assert!(r.list("T-1").unwrap().is_empty());
        // Idempotent on empty.
        r.delete_all("T-1").unwrap();
    }

    #[test]
    fn validate_rejects_bad_key() {
        let dir = TempDir::new().unwrap();
        let r = Reviews::new(dir.path()).unwrap();
        assert!(matches!(
            r.upsert(&mk_pkg("T-1", "bad/key")),
            Err(ReviewError::BadData(_))
        ));
    }

    #[test]
    fn dangling_journal_is_replayed_on_recover() {
        let dir = TempDir::new().unwrap();
        // Write a dangling journal directly (simulate crash mid-done).
        let record = DoneJournal {
            v: JOURNAL_VERSION,
            idempotency_key: "jk".into(),
            task_id: "T-9".into(),
            attempt: 1,
            package: {
                let mut p = mk_pkg("T-9", "jk");
                p.attempt = 1;
                p
            },
            log_line: None,
            target_estado: None,
            supersede_attempts: vec![],
        };
        let hash = hash_key(&record.idempotency_key);
        let journal = dir.path().join(format!("done-{hash}.journal"));
        write_json_atomic(&journal, &record).unwrap();
        // Fresh Reviews replays it.
        let r = Reviews::new(dir.path()).unwrap();
        assert!(r.read("T-9", 1).unwrap().is_some());
        assert!(!journal.exists(), "journal should be removed after replay");
    }

    #[test]
    fn recover_rebuilds_index() {
        let dir = TempDir::new().unwrap();
        {
            let r = Reviews::new(dir.path()).unwrap();
            r.upsert(&mk_pkg("T-1", "k1")).unwrap();
        }
        // Fresh instance: a repeat key must dedup to the same attempt.
        let r2 = Reviews::new(dir.path()).unwrap();
        let again = r2.upsert(&mk_pkg("T-1", "k1")).unwrap();
        assert_eq!(again.attempt, 1);
        assert_eq!(r2.list("T-1").unwrap().len(), 1);
    }
}
