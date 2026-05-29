//! Task↔project side mapping at `~/.cadenza/task-projects.json`.
//!
//! Why a side file: the task YAML frontmatter is frozen for Node.js
//! task-ai compat (`CLAUDE.md`). Adding a `project_id` field there
//! would round-trip-corrupt the file every time the Node.js side
//! re-wrote it without the project field. A separate JSON keeps the
//! task files untouched.
//!
//! Mapping shape: `{ "<task_id>": "<project_id>" }`. Entries with no
//! association are simply absent (no sentinel like `null`).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    map: HashMap<String, String>,
}

pub struct TaskProjects {
    path: PathBuf,
    state: Mutex<Doc>,
}

impl TaskProjects {
    pub fn load(home: &Path) -> Result<Self> {
        let path = home.join("task-projects.json");
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

    pub fn snapshot(&self) -> HashMap<String, String> {
        self.lock().map.clone()
    }

    /// Look up the project a task is mapped to, if any. Used to inherit
    /// the project when materializing a derived task from a proposal.
    pub fn get(&self, task_id: &str) -> Option<String> {
        self.lock().map.get(task_id).cloned()
    }

    /// Set the mapping. Passing `None` removes the entry — used by the
    /// UI to mean "task belongs to no project".
    pub fn set(&self, task_id: &str, project_id: Option<&str>) -> Result<()> {
        {
            let mut state = self.lock();
            match project_id {
                Some(pid) => {
                    state.map.insert(task_id.to_string(), pid.to_string());
                }
                None => {
                    state.map.remove(task_id);
                }
            }
        }
        self.save()
    }

    /// Forget any mapping for `task_id` — called when a task is
    /// deleted, so the JSON doesn't grow stale entries forever.
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

    /// PoisonError on a sync mutex means we already crashed during a
    /// write — the in-memory map and the file may be out of sync, but
    /// continuing is safer than aborting the UI. Treat the poisoned
    /// guard as the next-best snapshot.
    fn lock(&self) -> std::sync::MutexGuard<'_, Doc> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(&*self.lock())?;
        let tmp = self.path.with_extension("json.tmp");
        // fsync the tmp before rename so a power loss after rename
        // can't leave a zero-byte file on the visible path.
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trip_set_get() {
        let dir = TempDir::new().unwrap();
        let tp = TaskProjects::load(dir.path()).unwrap();
        tp.set("T-1", Some("proj-a")).unwrap();
        tp.set("T-2", Some("proj-b")).unwrap();
        assert_eq!(tp.get("T-1").as_deref(), Some("proj-a"));
        assert_eq!(tp.get("T-2").as_deref(), Some("proj-b"));
        assert_eq!(tp.get("T-99"), None);
    }

    #[test]
    fn set_none_removes_entry() {
        let dir = TempDir::new().unwrap();
        let tp = TaskProjects::load(dir.path()).unwrap();
        tp.set("T-1", Some("proj-a")).unwrap();
        tp.set("T-1", None).unwrap();
        assert_eq!(tp.get("T-1"), None);
    }

    #[test]
    fn forget_removes_and_skips_if_absent() {
        let dir = TempDir::new().unwrap();
        let tp = TaskProjects::load(dir.path()).unwrap();
        tp.set("T-1", Some("proj-a")).unwrap();
        tp.forget("T-1").unwrap();
        assert!(tp.snapshot().is_empty());
        // Idempotent — second forget is a no-op, no error.
        tp.forget("T-1").unwrap();
    }

    #[test]
    fn survives_reload() {
        let dir = TempDir::new().unwrap();
        {
            let tp = TaskProjects::load(dir.path()).unwrap();
            tp.set("T-1", Some("proj-a")).unwrap();
        }
        let tp2 = TaskProjects::load(dir.path()).unwrap();
        assert_eq!(tp2.get("T-1").as_deref(), Some("proj-a"));
    }
}
