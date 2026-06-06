//! Aggregate (issue-owned) review packages — filesystem backend under
//! `<home>/reviews/jira/` (Slice 5).
//!
//! Structurally mirrors [`super::review_inner::Reviews`] but keyed by
//! `(jira_site, jira_issue_id, attempt)` instead of `(task_id, attempt)`, and
//! WITHOUT the write-ahead journal: the per-task journal exists only to make
//! the estado-flipping `done` path crash-atomic. The aggregate is
//! STATE-NEUTRAL — it writes a single sidecar and supersedes priors, with no
//! task `.md`/estado side-effect — so a single atomic sidecar write is enough.
//!
//! The root is a dedicated SUBDIR (`<home>/reviews/jira/`), distinct from the
//! flat `<home>/reviews/` the per-task `Reviews` scans, so the broad
//! `*.review.json` scans there can never ingest an aggregate. The sidecar
//! filename additionally carries the `.aggregate.review.json` suffix.
//!
//! Identity is read ONLY from the JSON payload, never parsed back from the
//! filename — the site/issue segments are sanitized into a path-safe token,
//! which can be lossy, so the filename is just an addressing convenience.
#![allow(dead_code)]

use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

use super::review_inner::{Result, ReviewError};
use crate::review::issue::{IssuePackageStatus, IssueReviewPackage};

#[derive(Default)]
struct State {
    /// `(site, issue_id, idempotency_key)` -> attempt, rebuilt by `recover()`.
    by_key: HashMap<(String, String, String), u32>,
    /// `(site, issue_id)` -> max attempt seen, for O(1) next-attempt alloc.
    max_attempt: HashMap<(String, String), u32>,
}

/// Filesystem-backed aggregate-review store rooted at `<home>/reviews/jira/`.
pub struct JiraReviews {
    root: PathBuf,
    state: Mutex<State>,
}

/// Outcome of attempt allocation under the lock.
enum Allocation {
    Existing(Box<IssueReviewPackage>),
    Fresh { attempt: u32, supersede: Vec<u32> },
}

impl JiraReviews {
    /// Open (creating the dir) and rebuild the dedup/version index.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let reviews = JiraReviews {
            root,
            state: Mutex::new(State::default()),
        };
        reviews.recover()?;
        Ok(reviews)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<sanitized_site>.<sanitized_issue>.a<attempt>.aggregate.review.json`.
    fn package_path(&self, site: &str, issue_id: &str, attempt: u32) -> PathBuf {
        let s = sanitize_segment(site);
        let i = sanitize_segment(issue_id);
        self.root
            .join(format!("{s}.{i}.a{attempt}.aggregate.review.json"))
    }

    /// Rebuild `by_key` + `max_attempt` from `*.aggregate.review.json`,
    /// re-keying from the payload (never the filename). Malformed files are
    /// logged and skipped.
    pub fn recover(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state.by_key.clear();
        state.max_attempt.clear();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !name.ends_with(".aggregate.review.json") {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let pkg: IssueReviewPackage = match serde_json::from_str(&content) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(path = ?path, error = %e, "skipping malformed aggregate review");
                    continue;
                }
            };
            index_insert(&mut state, &pkg);
        }
        Ok(())
    }

    /// Allocate (or short-circuit on an existing key) under the lock.
    fn allocate(&self, pkg: &IssueReviewPackage) -> Result<Allocation> {
        let mut state = self.state.lock().unwrap();
        let key = (
            pkg.jira_site.clone(),
            pkg.jira_issue_id.clone(),
            pkg.idempotency_key.clone(),
        );
        if let Some(att) = state.by_key.get(&key) {
            let att = *att;
            drop(state);
            let existing = self
                .read(&pkg.jira_site, &pkg.jira_issue_id, att)?
                .ok_or_else(|| {
                    ReviewError::NotFound(format!("{}.{}.a{att}", pkg.jira_site, pkg.jira_issue_id))
                })?;
            return Ok(Allocation::Existing(Box::new(existing)));
        }
        let ikey = (pkg.jira_site.clone(), pkg.jira_issue_id.clone());
        let max = state.max_attempt.get(&ikey).copied().unwrap_or(0);
        let attempt = max + 1;
        // Reserve the attempt under the lock: the sidecar write + index_insert
        // happen AFTER the lock is released, so without this a concurrent
        // upsert for the same issue would read the same `max` and allocate the
        // same attempt, overwriting this one's sidecar. A reserved-but-unwritten
        // attempt (write failure) just leaves a harmless numbering gap.
        state.max_attempt.insert(ikey, attempt);
        let supersede: Vec<u32> = (1..attempt)
            .filter(|n| {
                self.package_path(&pkg.jira_site, &pkg.jira_issue_id, *n)
                    .exists()
            })
            .collect();
        Ok(Allocation::Fresh { attempt, supersede })
    }

    /// Idempotent upsert. Returns the stored package unchanged when the key
    /// already exists. Otherwise allocates `attempt`, supersedes prior
    /// `Pending` aggregates for the same issue, writes the sidecar, and
    /// updates the index. The package's `attempt` is overwritten by the
    /// backend; its carried `status` is preserved (defaults to `Pending`).
    pub fn upsert(&self, pkg: &IssueReviewPackage) -> Result<IssueReviewPackage> {
        super::validate_idempotency_key(&pkg.idempotency_key).map_err(map_validation)?;
        match self.allocate(pkg)? {
            Allocation::Existing(existing) => Ok(*existing),
            Allocation::Fresh { attempt, supersede } => {
                for n in &supersede {
                    if let Some(mut prior) = self.read(&pkg.jira_site, &pkg.jira_issue_id, *n)? {
                        if prior.status == IssuePackageStatus::Pending {
                            prior.status = IssuePackageStatus::Superseded;
                            write_json_atomic(
                                &self.package_path(&pkg.jira_site, &pkg.jira_issue_id, *n),
                                &prior,
                            )?;
                        }
                    }
                }
                let mut stored = pkg.clone();
                stored.attempt = attempt;
                write_json_atomic(
                    &self.package_path(&pkg.jira_site, &pkg.jira_issue_id, attempt),
                    &stored,
                )?;
                let mut state = self.state.lock().unwrap();
                index_insert(&mut state, &stored);
                Ok(stored)
            }
        }
    }

    pub fn read(
        &self,
        site: &str,
        issue_id: &str,
        attempt: u32,
    ) -> Result<Option<IssueReviewPackage>> {
        let path = self.package_path(site, issue_id, attempt);
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)?;
        let pkg: IssueReviewPackage = serde_json::from_str(&text)?;
        Ok(Some(pkg))
    }

    /// Every aggregate for an issue, ordered by `attempt` ascending. Guards
    /// against another issue whose sanitized token collides by checking the
    /// payload's `(site, issue_id)` against the requested pair.
    pub fn list(&self, site: &str, issue_id: &str) -> Result<Vec<IssueReviewPackage>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !name.ends_with(".aggregate.review.json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(pkg): std::result::Result<IssueReviewPackage, _> = serde_json::from_str(&text)
            else {
                continue;
            };
            if pkg.jira_site != site || pkg.jira_issue_id != issue_id {
                continue;
            }
            out.push(pkg);
        }
        out.sort_by_key(|p| p.attempt);
        Ok(out)
    }

    /// Every aggregate across all issues, ordered `(site, issue_id, attempt)`.
    pub fn all(&self) -> Result<Vec<IssueReviewPackage>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !name.ends_with(".aggregate.review.json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(pkg): std::result::Result<IssueReviewPackage, _> = serde_json::from_str(&text)
            else {
                continue;
            };
            out.push(pkg);
        }
        out.sort_by(|a, b| {
            a.jira_site
                .cmp(&b.jira_site)
                .then(a.jira_issue_id.cmp(&b.jira_issue_id))
                .then(a.attempt.cmp(&b.attempt))
        });
        Ok(out)
    }
}

fn index_insert(state: &mut State, pkg: &IssueReviewPackage) {
    state.by_key.insert(
        (
            pkg.jira_site.clone(),
            pkg.jira_issue_id.clone(),
            pkg.idempotency_key.clone(),
        ),
        pkg.attempt,
    );
    let slot = state
        .max_attempt
        .entry((pkg.jira_site.clone(), pkg.jira_issue_id.clone()))
        .or_insert(0);
    if pkg.attempt > *slot {
        *slot = pkg.attempt;
    }
}

/// Lowercase, replace any char not in `[a-z0-9-]` with `-`, collapse runs of
/// `-`, trim. Mirrors the branch-name sanitizer. Lossy, hence the payload —
/// not the filename — is the source of truth for identity. An all-punctuation
/// segment maps to a stable `_` placeholder so the filename stays valid.
fn sanitize_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for ch in s.chars() {
        let lc = ch.to_ascii_lowercase();
        let mapped = if lc.is_ascii_lowercase() || lc.is_ascii_digit() {
            lc
        } else {
            '-'
        };
        if mapped == '-' {
            if prev_dash {
                continue;
            }
            prev_dash = true;
        } else {
            prev_dash = false;
        }
        out.push(mapped);
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "_".to_string()
    } else {
        trimmed
    }
}

fn map_validation(e: super::StoreError) -> ReviewError {
    match e {
        super::StoreError::BadData(s) => ReviewError::BadData(s),
        super::StoreError::Io(e) => ReviewError::Io(e),
        other => ReviewError::BadData(other.to_string()),
    }
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    use anyhow::Context;
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
    use crate::review::issue::IssuePackageStatus;
    use tempfile::TempDir;

    fn mk(site: &str, issue: &str, key: &str) -> IssueReviewPackage {
        IssueReviewPackage {
            jira_site: site.into(),
            jira_issue_id: issue.into(),
            attempt: 0,
            idempotency_key: key.into(),
            status: IssuePackageStatus::Pending,
            branch_name: "jira/x".into(),
            base_sha: "abc".into(),
            head_sha: Some("def".into()),
            changed_files: vec![],
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
            diff: None,
            truncated: false,
            collection_errors: vec![],
            created_at_ms: 1,
            collection_duration_ms: 0,
        }
    }

    #[test]
    fn upsert_allocates_and_reads_back() {
        let dir = TempDir::new().unwrap();
        let r = JiraReviews::new(dir.path()).unwrap();
        let stored = r
            .upsert(&mk("https://x.atlassian.net", "10001", "k1"))
            .unwrap();
        assert_eq!(stored.attempt, 1);
        let back = r
            .read("https://x.atlassian.net", "10001", 1)
            .unwrap()
            .unwrap();
        assert_eq!(back.idempotency_key, "k1");
    }

    #[test]
    fn upsert_dedups_on_key() {
        let dir = TempDir::new().unwrap();
        let r = JiraReviews::new(dir.path()).unwrap();
        let a = r.upsert(&mk("s", "1", "same")).unwrap();
        let b = r.upsert(&mk("s", "1", "same")).unwrap();
        assert_eq!(a.attempt, b.attempt);
        assert_eq!(r.list("s", "1").unwrap().len(), 1);
    }

    #[test]
    fn supersede_prior_pending() {
        let dir = TempDir::new().unwrap();
        let r = JiraReviews::new(dir.path()).unwrap();
        r.upsert(&mk("s", "1", "k1")).unwrap();
        let second = r.upsert(&mk("s", "1", "k2")).unwrap();
        assert_eq!(second.attempt, 2);
        let list = r.list("s", "1").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].status, IssuePackageStatus::Superseded);
        assert_eq!(list[1].status, IssuePackageStatus::Pending);
    }

    #[test]
    fn recover_rebuilds_index() {
        let dir = TempDir::new().unwrap();
        {
            let r = JiraReviews::new(dir.path()).unwrap();
            r.upsert(&mk("s", "1", "k1")).unwrap();
        }
        let r2 = JiraReviews::new(dir.path()).unwrap();
        let again = r2.upsert(&mk("s", "1", "k1")).unwrap();
        assert_eq!(again.attempt, 1);
        assert_eq!(r2.list("s", "1").unwrap().len(), 1);
    }

    #[test]
    fn segments_with_special_chars_do_not_collide_in_list() {
        let dir = TempDir::new().unwrap();
        let r = JiraReviews::new(dir.path()).unwrap();
        // Two issues whose sanitized tokens could collide are kept distinct by
        // the payload guard in `list`.
        r.upsert(&mk("https://x.atlassian.net", "PROJ-1", "k1"))
            .unwrap();
        r.upsert(&mk("https://x.atlassian.net", "PROJ-2", "k1"))
            .unwrap();
        assert_eq!(
            r.list("https://x.atlassian.net", "PROJ-1").unwrap().len(),
            1
        );
        assert_eq!(
            r.list("https://x.atlassian.net", "PROJ-2").unwrap().len(),
            1
        );
    }
}
