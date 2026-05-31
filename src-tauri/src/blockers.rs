//! Task blocker side mapping at `~/.cadenza/task-blockers.json`.
//!
//! The task YAML frontmatter is frozen for legacy compatibility, so this
//! relationship follows the same sidecar pattern as projects/worktrees.
//! Mapping shape: `{ "map": { "<task_id>": ["<blocking_task_id>", ...] } }`.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    map: HashMap<String, Vec<String>>,
}

pub struct TaskBlockers {
    path: PathBuf,
    state: Mutex<Doc>,
}

impl TaskBlockers {
    pub fn load(home: &Path) -> Result<Self> {
        let path = home.join("task-blockers.json");
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

    pub fn get(&self, task_id: &str) -> Vec<String> {
        self.lock().map.get(task_id).cloned().unwrap_or_default()
    }

    pub fn set(&self, task_id: &str, blocked_by: Vec<String>) -> Result<()> {
        let blocked_by = normalize(task_id, blocked_by)?;
        {
            let mut state = self.lock();
            if let Some(culprit) = cycle_culprit(&state.map, task_id, &blocked_by) {
                return Err(anyhow!(
                    "task '{task_id}' cannot be blocked by '{culprit}': \
                     it would create a dependency cycle"
                ));
            }
            if blocked_by.is_empty() {
                state.map.remove(task_id);
            } else {
                state.map.insert(task_id.to_string(), blocked_by);
            }
        }
        self.save()
    }

    pub fn enrich(&self, task: cadenza_proto::Task) -> cadenza_proto::Task {
        let blocked_by = self.get(&task.id);
        if blocked_by.is_empty() {
            task
        } else {
            cadenza_proto::Task { blocked_by, ..task }
        }
    }

    /// Remove the task's own blocker row and remove it from other rows too.
    pub fn forget(&self, task_id: &str) -> Result<()> {
        let changed = {
            let mut state = self.lock();
            let mut changed = state.map.remove(task_id).is_some();
            let mut empty = Vec::new();
            for (blocked_task, blockers) in &mut state.map {
                let before = blockers.len();
                blockers.retain(|id| id != task_id);
                if blockers.len() != before {
                    changed = true;
                }
                if blockers.is_empty() {
                    empty.push(blocked_task.clone());
                }
            }
            for id in empty {
                state.map.remove(&id);
            }
            changed
        };
        if changed {
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

/// Returns the proposed blocker that would close a dependency cycle if
/// `task_id` were blocked by `blocked_by`, or `None` when the result stays
/// acyclic. Walks the existing edges out of each proposed blocker; reaching
/// `task_id` means the new edge closes a loop (e.g. `T-1 -> T-2 -> T-1`).
/// `normalize` already rejects the direct self-block, so this covers the
/// transitive case.
fn cycle_culprit(
    map: &HashMap<String, Vec<String>>,
    task_id: &str,
    blocked_by: &[String],
) -> Option<String> {
    let mut visited = HashSet::new();
    for start in blocked_by {
        let mut stack = vec![start.as_str()];
        while let Some(cur) = stack.pop() {
            if cur == task_id {
                return Some(start.clone());
            }
            if !visited.insert(cur.to_string()) {
                continue;
            }
            if let Some(next) = map.get(cur) {
                stack.extend(next.iter().map(String::as_str));
            }
        }
    }
    None
}

fn normalize(task_id: &str, blocked_by: Vec<String>) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in blocked_by {
        let id = raw.trim();
        if id.is_empty() {
            continue;
        }
        if id == task_id {
            return Err(anyhow!("task '{task_id}' cannot block itself"));
        }
        if seen.insert(id.to_string()) {
            out.push(id.to_string());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trip_set_get() {
        let dir = TempDir::new().unwrap();
        let blockers = TaskBlockers::load(dir.path()).unwrap();
        blockers
            .set("T-3", vec!["T-1".into(), "T-2".into(), "T-1".into()])
            .unwrap();

        assert_eq!(blockers.get("T-3"), vec!["T-1", "T-2"]);
        assert!(blockers.get("T-99").is_empty());
    }

    #[test]
    fn set_empty_removes_entry() {
        let dir = TempDir::new().unwrap();
        let blockers = TaskBlockers::load(dir.path()).unwrap();
        blockers.set("T-2", vec!["T-1".into()]).unwrap();
        blockers.set("T-2", Vec::new()).unwrap();

        assert!(blockers.get("T-2").is_empty());
    }

    #[test]
    fn rejects_self_block() {
        let dir = TempDir::new().unwrap();
        let blockers = TaskBlockers::load(dir.path()).unwrap();

        assert!(blockers.set("T-1", vec!["T-1".into()]).is_err());
    }

    #[test]
    fn rejects_direct_cycle() {
        let dir = TempDir::new().unwrap();
        let blockers = TaskBlockers::load(dir.path()).unwrap();
        blockers.set("T-1", vec!["T-2".into()]).unwrap();

        // T-2 blocked by T-1 would close the loop T-1 -> T-2 -> T-1.
        assert!(blockers.set("T-2", vec!["T-1".into()]).is_err());
        assert!(blockers.get("T-2").is_empty());
    }

    #[test]
    fn rejects_transitive_cycle() {
        let dir = TempDir::new().unwrap();
        let blockers = TaskBlockers::load(dir.path()).unwrap();
        blockers.set("T-1", vec!["T-2".into()]).unwrap();
        blockers.set("T-2", vec!["T-3".into()]).unwrap();

        // T-3 blocked by T-1 closes T-1 -> T-2 -> T-3 -> T-1.
        assert!(blockers.set("T-3", vec!["T-1".into()]).is_err());
    }

    #[test]
    fn forget_removes_incoming_and_outgoing_edges() {
        let dir = TempDir::new().unwrap();
        let blockers = TaskBlockers::load(dir.path()).unwrap();
        blockers
            .set("T-3", vec!["T-1".into(), "T-2".into()])
            .unwrap();
        blockers.set("T-2", vec!["T-1".into()]).unwrap();

        blockers.forget("T-1").unwrap();

        assert_eq!(blockers.get("T-3"), vec!["T-2"]);
        assert!(blockers.get("T-2").is_empty());
    }

    #[test]
    fn survives_reload() {
        let dir = TempDir::new().unwrap();
        {
            let blockers = TaskBlockers::load(dir.path()).unwrap();
            blockers.set("T-3", vec!["T-1".into()]).unwrap();
        }
        let blockers = TaskBlockers::load(dir.path()).unwrap();

        assert_eq!(blockers.get("T-3"), vec!["T-1"]);
    }
}
