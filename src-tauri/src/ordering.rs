//! Per-column card ordering at `~/.cadenza/task-order.json`.
//!
//! Why a side file: the task YAML frontmatter is frozen for Node.js
//! task-ai compat (`CLAUDE.md`), and none of the storage backends carry
//! an ordering column. A separate JSON keeps the task files and DB
//! schemas untouched. Same pattern as `task-projects.json` and
//! `task-worktrees.json`.
//!
//! Shape: `{ "map": { "<estado>": ["<task_id>", …] } }` — one ordered id
//! list per column. An id's position in its column's list is its
//! priority; ids absent from the list sort to the end (see
//! `commands::sort_tasks_by_order`), which is how a brand-new task lands
//! at the bottom of its queue with zero write at create time. Columns
//! with no custom order are simply absent.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    map: HashMap<String, Vec<String>>,
}

pub struct TaskOrder {
    path: PathBuf,
    state: Mutex<Doc>,
}

impl TaskOrder {
    pub fn load(home: &Path) -> Result<Self> {
        let path = home.join("task-order.json");
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

    /// Full estado → ordered ids mapping. The command layer takes a
    /// snapshot once per `list_tasks` and sorts each column against it.
    pub fn snapshot(&self) -> HashMap<String, Vec<String>> {
        self.lock().map.clone()
    }

    /// Replace the ordered id list for one column. An empty list removes
    /// the entry so the JSON doesn't accumulate empty-array cruft.
    pub fn set(&self, estado: &str, ids: Vec<String>) -> Result<()> {
        {
            let mut state = self.lock();
            if ids.is_empty() {
                state.map.remove(estado);
            } else {
                state.map.insert(estado.to_string(), ids);
            }
        }
        self.save()
    }

    /// Drop a task id from every column list — called when a task is
    /// deleted so the JSON doesn't keep dangling ids. Unlike the
    /// id-keyed sidecars, an id can live in any column, so we scan all
    /// lists. Idempotent: a no-op when the id is absent everywhere.
    pub fn forget(&self, task_id: &str) -> Result<()> {
        let changed = {
            let mut state = self.lock();
            let mut changed = false;
            for ids in state.map.values_mut() {
                let before = ids.len();
                ids.retain(|x| x != task_id);
                changed |= ids.len() != before;
            }
            // Drop any column lists left empty by the removal.
            state.map.retain(|_, ids| !ids.is_empty());
            changed
        };
        if changed {
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
        let to = TaskOrder::load(dir.path()).unwrap();
        to.set("a_fazer", vec!["T-3".into(), "T-1".into(), "T-5".into()])
            .unwrap();
        let snap = to.snapshot();
        assert_eq!(
            snap.get("a_fazer").unwrap(),
            &vec!["T-3".to_string(), "T-1".into(), "T-5".into()]
        );
        // A column never ordered is simply absent.
        assert!(!snap.contains_key("feito"));
    }

    #[test]
    fn set_empty_removes_key() {
        let dir = TempDir::new().unwrap();
        let to = TaskOrder::load(dir.path()).unwrap();
        to.set("a_fazer", vec!["T-1".into()]).unwrap();
        to.set("a_fazer", vec![]).unwrap();
        assert!(!to.snapshot().contains_key("a_fazer"));
    }

    #[test]
    fn forget_removes_from_all_columns() {
        let dir = TempDir::new().unwrap();
        let to = TaskOrder::load(dir.path()).unwrap();
        to.set("a_fazer", vec!["T-1".into(), "T-2".into()]).unwrap();
        to.set("fazendo", vec!["T-2".into(), "T-3".into()]).unwrap();
        to.forget("T-2").unwrap();
        let snap = to.snapshot();
        assert_eq!(snap.get("a_fazer").unwrap(), &vec!["T-1".to_string()]);
        assert_eq!(snap.get("fazendo").unwrap(), &vec!["T-3".to_string()]);
        // Idempotent — a second forget changes nothing and errors not.
        to.forget("T-2").unwrap();
    }

    #[test]
    fn survives_reload() {
        let dir = TempDir::new().unwrap();
        {
            let to = TaskOrder::load(dir.path()).unwrap();
            to.set("a_fazer", vec!["T-9".into(), "T-4".into()]).unwrap();
        }
        let to2 = TaskOrder::load(dir.path()).unwrap();
        assert_eq!(
            to2.snapshot().get("a_fazer").unwrap(),
            &vec!["T-9".to_string(), "T-4".into()]
        );
    }
}
