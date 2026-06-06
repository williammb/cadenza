//! File-backed `JiraIssueRecord` store under `<home>/jira/`.
//!
//! Each record is a JSON file keyed by `(jira_site, jira_issue_id)`. Since
//! `jira_site` can contain `:` / `/` (it is a base URL or cloud id), the
//! filename is a percent-encoded slug of both components joined by `__`, run
//! through `validate_id` so it can never escape the store root. Like
//! `IdeiaStore`, the schema is not frozen by the Node.js legacy.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

pub use cadenza_proto::JiraIssueRecord;

use super::{validate_id, Result, StoreError};

pub struct JiraIssueStore {
    root: PathBuf,
}

/// Percent-encode any byte outside `[A-Za-z0-9._-]` as `%XX` so the result
/// is a single safe path component. Mirrors the conservative charset used by
/// `validate_idempotency_key`.
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Build the composite-key slug for `(site, issue_id)`. Each component is
/// percent-encoded independently then joined with `__`, so two distinct
/// pairs can never collide (the encoding makes `__` the only literal
/// double-underscore separator).
fn key_slug(site: &str, issue_id: &str) -> String {
    format!("{}__{}", pct_encode(site), pct_encode(issue_id))
}

impl JiraIssueStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(StoreError::Io)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, site: &str, issue_id: &str) -> Result<PathBuf> {
        let slug = key_slug(site, issue_id);
        // The slug is the filename stem; guard it against path traversal even
        // though pct_encode already strips separators.
        validate_id(&slug)?;
        Ok(self.root.join(format!("{slug}.json")))
    }

    /// Unconditional overwrite upsert: atomic tmp + fsync + rename.
    pub fn upsert(&self, record: &JiraIssueRecord) -> Result<()> {
        use std::io::Write;
        let path = self.path_for(&record.jira_site, &record.jira_issue_id)?;
        let tmp = path.with_extension("json.tmp");
        let text =
            serde_json::to_string_pretty(record).map_err(|e| StoreError::BadData(e.to_string()))?;
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                .map_err(StoreError::Io)?;
            f.write_all(text.as_bytes()).map_err(StoreError::Io)?;
            f.sync_all().map_err(StoreError::Io)?;
        }
        fs::rename(&tmp, &path).map_err(StoreError::Io)?;
        Ok(())
    }

    pub fn read(&self, site: &str, issue_id: &str) -> Result<Option<JiraIssueRecord>> {
        let path = self.path_for(site, issue_id)?;
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(
                serde_json::from_str(&text).map_err(|e| StoreError::BadData(e.to_string()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    pub fn list(&self) -> Result<Vec<JiraIssueRecord>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(StoreError::Io(e)),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let text = match fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => return Err(StoreError::Io(e)),
            };
            match serde_json::from_str::<JiraIssueRecord>(&text) {
                Ok(rec) => out.push(rec),
                Err(e) => {
                    tracing::warn!(
                        error = ?e,
                        path = %path.display(),
                        "skipping malformed jira issue json"
                    );
                }
            }
        }
        out.sort_by(|a, b| {
            a.jira_site
                .cmp(&b.jira_site)
                .then_with(|| a.jira_issue_id.cmp(&b.jira_issue_id))
        });
        Ok(out)
    }

    pub fn delete(&self, site: &str, issue_id: &str) -> Result<()> {
        let path = self.path_for(site, issue_id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StoreError::NotFound(format!("{site}/{issue_id}")))
            }
            Err(e) => Err(StoreError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn mk(d: &TempDir) -> JiraIssueStore {
        JiraIssueStore::new(d.path().join("jira")).unwrap()
    }

    fn sample(site: &str, id: &str, key: &str) -> JiraIssueRecord {
        JiraIssueRecord {
            jira_site: site.into(),
            jira_issue_id: id.into(),
            jira_key: key.into(),
            project_id: None,
            analysis_run_id: None,
            secret_hash: None,
            secret_expiry_ms: None,
            secret_status: None,
            raw_adf: None,
            branch_name: None,
            worktree_path: None,
            base_sha: None,
            worktree_state: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn upsert_read_delete() {
        let d = TempDir::new().unwrap();
        let store = mk(&d);
        store.upsert(&sample("site", "1", "PROJ-1")).unwrap();
        assert_eq!(store.read("site", "1").unwrap().unwrap().jira_key, "PROJ-1");
        // Upsert overwrites.
        store.upsert(&sample("site", "1", "PROJ-2")).unwrap();
        assert_eq!(store.read("site", "1").unwrap().unwrap().jira_key, "PROJ-2");
        assert_eq!(store.list().unwrap().len(), 1);
        store.delete("site", "1").unwrap();
        assert!(store.read("site", "1").unwrap().is_none());
        assert!(matches!(
            store.delete("site", "1"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn composite_key_with_special_chars() {
        let d = TempDir::new().unwrap();
        let store = mk(&d);
        let site = "https://x.atlassian.net";
        store.upsert(&sample(site, "10001", "PROJ-123")).unwrap();
        let got = store.read(site, "10001").unwrap().unwrap();
        assert_eq!(got.jira_key, "PROJ-123");
        // A different pair that would naively collide must not.
        store.upsert(&sample(site, "10002", "PROJ-124")).unwrap();
        assert_eq!(store.list().unwrap().len(), 2);
    }
}
