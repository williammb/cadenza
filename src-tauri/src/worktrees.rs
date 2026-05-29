//! Task↔worktree side mapping at `~/.cadenza/task-worktrees.json`.
//!
//! Why a side file: the task YAML frontmatter is frozen for Node.js
//! task-ai compat (`CLAUDE.md`). A separate JSON keeps the task files
//! untouched. Same pattern as `task-projects.json`.
//!
//! Mapping shape: `{ "map": { "<task_id>": { "worktree_path": "…", "branch": "…" } } }`.
//! Entries with no association are simply absent.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

impl WorktreeInfo {
    pub fn is_empty(&self) -> bool {
        self.worktree_path.is_none() && self.branch.is_none()
    }
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    map: HashMap<String, WorktreeInfo>,
}

pub struct TaskWorktrees {
    path: PathBuf,
    state: Mutex<Doc>,
}

impl TaskWorktrees {
    pub fn load(home: &Path) -> Result<Self> {
        let path = home.join("task-worktrees.json");
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

    pub fn snapshot(&self) -> HashMap<String, WorktreeInfo> {
        self.lock().map.clone()
    }

    pub fn get(&self, task_id: &str) -> Option<WorktreeInfo> {
        self.lock().map.get(task_id).cloned()
    }

    /// Set the worktree/branch for a task. Passing an empty/None info
    /// removes the entry — used by the UI to clear the association.
    pub fn set(&self, task_id: &str, info: WorktreeInfo) -> Result<()> {
        {
            let mut state = self.lock();
            if info.is_empty() {
                state.map.remove(task_id);
            } else {
                state.map.insert(task_id.to_string(), info);
            }
        }
        self.save()
    }

    /// Inject this task's `worktree_path`/`branch` from the sidecar.
    /// Shared by the Tauri command layer (`commands.rs`) and the IPC
    /// dispatch (`ipc.rs`) so the two paths can't drift.
    pub fn enrich(&self, task: cadenza_proto::Task) -> cadenza_proto::Task {
        if let Some(info) = self.get(&task.id) {
            cadenza_proto::Task {
                worktree_path: info.worktree_path,
                branch: info.branch,
                ..task
            }
        } else {
            task
        }
    }

    /// Forget any mapping for `task_id` — called when a task is deleted.
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trip_set_get() {
        let dir = TempDir::new().unwrap();
        let tw = TaskWorktrees::load(dir.path()).unwrap();
        tw.set(
            "T-1",
            WorktreeInfo {
                worktree_path: Some("/repo/worktrees/T-1".into()),
                branch: Some("feat/T-1".into()),
            },
        )
        .unwrap();
        let info = tw.get("T-1").unwrap();
        assert_eq!(info.worktree_path.as_deref(), Some("/repo/worktrees/T-1"));
        assert_eq!(info.branch.as_deref(), Some("feat/T-1"));
        assert_eq!(tw.get("T-99"), None);
    }

    #[test]
    fn set_empty_removes_entry() {
        let dir = TempDir::new().unwrap();
        let tw = TaskWorktrees::load(dir.path()).unwrap();
        tw.set(
            "T-1",
            WorktreeInfo {
                worktree_path: Some("/repo/worktrees/T-1".into()),
                branch: None,
            },
        )
        .unwrap();
        tw.set("T-1", WorktreeInfo::default()).unwrap();
        assert_eq!(tw.get("T-1"), None);
    }

    #[test]
    fn forget_removes_and_skips_if_absent() {
        let dir = TempDir::new().unwrap();
        let tw = TaskWorktrees::load(dir.path()).unwrap();
        tw.set(
            "T-1",
            WorktreeInfo {
                worktree_path: Some("/x".into()),
                branch: None,
            },
        )
        .unwrap();
        tw.forget("T-1").unwrap();
        assert!(tw.snapshot().is_empty());
        tw.forget("T-1").unwrap(); // idempotent
    }

    #[test]
    fn survives_reload() {
        let dir = TempDir::new().unwrap();
        {
            let tw = TaskWorktrees::load(dir.path()).unwrap();
            tw.set(
                "T-1",
                WorktreeInfo {
                    worktree_path: Some("/repo".into()),
                    branch: Some("main".into()),
                },
            )
            .unwrap();
        }
        let tw2 = TaskWorktrees::load(dir.path()).unwrap();
        let info = tw2.get("T-1").unwrap();
        assert_eq!(info.worktree_path.as_deref(), Some("/repo"));
        assert_eq!(info.branch.as_deref(), Some("main"));
    }
}
