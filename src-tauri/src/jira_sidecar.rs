//! Jira side-stores for the file backend + synchronous enrichment.
//!
//! Two pieces, both mirroring the `task-projects.json` / `task-worktrees.json`
//! sidecar pattern (the task YAML frontmatter is frozen for Node.js compat,
//! so Jira identity can't live there):
//!
//! - [`TaskJira`] — durable `task-jira.json` map `task_id → (site, issue_id)`.
//!   The file backend has no task column for Jira identity, so this sidecar is
//!   the file-backend equivalent of the SQL `tasks.jira_site/jira_issue_id`
//!   columns. Written by `create_task_from_proposta`.
//!
//! - [`JiraKeyIndex`] — in-memory `(site, issue_id) → jira_key` map used by the
//!   synchronous `enrich_task` seam to fill `jira_key_display` without an async
//!   store read. Seeded at startup from `repo.list_jira_issues()` and updated
//!   when a record is upserted.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JiraIdentity {
    pub jira_site: String,
    pub jira_issue_id: String,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    map: HashMap<String, JiraIdentity>,
}

/// `task_id → (jira_site, jira_issue_id)` durable side mapping at
/// `~/.cadenza/task-jira.json`.
pub struct TaskJira {
    path: PathBuf,
    state: Mutex<Doc>,
}

impl TaskJira {
    pub fn load(home: &Path) -> Result<Self> {
        let path = home.join("task-jira.json");
        let state = if path.exists() {
            let text = fs::read_to_string(&path)?;
            serde_json::from_str::<Doc>(&text).unwrap_or_default()
        } else {
            Doc::default()
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    /// Look up the Jira identity a task is mapped to, if any.
    pub fn get(&self, task_id: &str) -> Option<(String, String)> {
        self.lock()
            .map
            .get(task_id)
            .map(|i| (i.jira_site.clone(), i.jira_issue_id.clone()))
    }

    /// Record the Jira identity for a task.
    pub fn set(&self, task_id: &str, site: &str, issue_id: &str) -> Result<()> {
        {
            let mut state = self.lock();
            state.map.insert(
                task_id.to_string(),
                JiraIdentity {
                    jira_site: site.to_string(),
                    jira_issue_id: issue_id.to_string(),
                },
            );
        }
        self.save()
    }

    /// Forget any mapping for `task_id` — called when a task is deleted.
    /// Wired into the delete-task cascade in a later slice.
    #[allow(dead_code)]
    pub fn forget(&self, task_id: &str) -> Result<()> {
        let removed = {
            let mut state = self.lock();
            state.map.remove(task_id).is_some()
        };
        if removed {
            self.save()
        } else {
            Ok(())
        }
    }

    /// Inject this task's `jira_site`/`jira_issue_id` from the sidecar. Used
    /// on the file backend, where those fields are not on the task row.
    pub fn enrich(&self, task: cadenza_proto::Task) -> cadenza_proto::Task {
        if task.jira_site.is_some() && task.jira_issue_id.is_some() {
            return task;
        }
        if let Some((site, issue_id)) = self.get(&task.id) {
            cadenza_proto::Task {
                jira_site: Some(site),
                jira_issue_id: Some(issue_id),
                ..task
            }
        } else {
            task
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Doc> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(&*self.lock())?;
        let tmp = self.path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// In-memory `(jira_site, jira_issue_id) → jira_key` index for synchronous
/// `jira_key_display` enrichment.
#[derive(Default)]
pub struct JiraKeyIndex {
    map: RwLock<HashMap<(String, String), String>>,
}

impl JiraKeyIndex {
    /// Build the index from a snapshot of records (e.g. `list_jira_issues`).
    pub fn from_records(records: &[cadenza_proto::JiraIssueRecord]) -> Self {
        let mut map = HashMap::new();
        for r in records {
            map.insert(
                (r.jira_site.clone(), r.jira_issue_id.clone()),
                r.jira_key.clone(),
            );
        }
        Self {
            map: RwLock::new(map),
        }
    }

    /// Resolve the stored display key for `(site, issue_id)`, if cached.
    pub fn get(&self, site: &str, issue_id: &str) -> Option<String> {
        let map = self.map.read().unwrap_or_else(|p| p.into_inner());
        map.get(&(site.to_string(), issue_id.to_string())).cloned()
    }

    /// Record/refresh a single key — called after `upsert_jira_issue`
    /// (wired in a later slice once the upsert command/IPC op exists).
    #[allow(dead_code)]
    pub fn set(&self, site: &str, issue_id: &str, jira_key: &str) {
        let mut map = self.map.write().unwrap_or_else(|p| p.into_inner());
        map.insert(
            (site.to_string(), issue_id.to_string()),
            jira_key.to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn task_jira_round_trip_and_reload() {
        let dir = TempDir::new().unwrap();
        {
            let tj = TaskJira::load(dir.path()).unwrap();
            tj.set("T-1", "https://x.atlassian.net", "10001").unwrap();
            assert_eq!(
                tj.get("T-1"),
                Some(("https://x.atlassian.net".into(), "10001".into()))
            );
        }
        let tj2 = TaskJira::load(dir.path()).unwrap();
        assert_eq!(
            tj2.get("T-1"),
            Some(("https://x.atlassian.net".into(), "10001".into()))
        );
        tj2.forget("T-1").unwrap();
        assert!(tj2.get("T-1").is_none());
        // Idempotent second forget.
        tj2.forget("T-1").unwrap();
    }

    #[test]
    fn key_index_get_set() {
        let idx = JiraKeyIndex::default();
        assert!(idx.get("s", "1").is_none());
        idx.set("s", "1", "PROJ-1");
        assert_eq!(idx.get("s", "1").as_deref(), Some("PROJ-1"));
    }
}
