//! Task→run side mapping at `~/.cadenza/task-runs.json`.
//!
//! Mirrors `projects.rs` (`TaskProjects`) in shape: a JSON file written
//! atomically via `.tmp + rename`, keyed by `task_id`. Stores the last
//! agent / model / conversation id used to launch an agent for that
//! task, so a second click on "Iniciar" can become "Continuar" with the
//! right `--resume` flag.
//!
//! Separate file (rather than YAML frontmatter on the task) because the
//! task YAML is frozen for Node.js `task-ai` compat — see `projects.rs`.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::config::AgenteKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRun {
    pub agent: AgenteKind,
    pub model: String,
    /// Claude: UUID we generated and passed via `--session-id`.
    /// Codex: UUID Codex generated, captured from `~/.codex/sessions/`
    /// asynchronously after first spawn. `None` while still pending or
    /// when capture failed.
    #[serde(default)]
    pub conversation_id: Option<String>,
    pub last_started_at: DateTime<Utc>,
    /// In-memory PTY session id (e.g. `S-<uuid>`). Useful for the UI
    /// to reattach without re-spawning if the session is still alive.
    #[serde(default)]
    pub last_session_id: Option<String>,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    runs: HashMap<String, TaskRun>,
}

pub struct TaskRuns {
    path: PathBuf,
    state: Mutex<Doc>,
}

impl TaskRuns {
    pub fn load(home: &Path) -> Result<Self> {
        let path = home.join("task-runs.json");
        let state = if path.exists() {
            let text = fs::read_to_string(&path)?;
            // Tolerant: a corrupt file shouldn't brick the app. Worst
            // case the user re-runs and we re-populate.
            serde_json::from_str::<Doc>(&text).unwrap_or_default()
        } else {
            Doc::default()
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    pub fn snapshot(&self) -> HashMap<String, TaskRun> {
        self.lock().runs.clone()
    }

    pub fn get(&self, task_id: &str) -> Option<TaskRun> {
        self.lock().runs.get(task_id).cloned()
    }

    pub fn upsert(&self, task_id: &str, run: TaskRun) -> Result<()> {
        {
            let mut state = self.lock();
            state.runs.insert(task_id.to_string(), run);
        }
        self.save()
    }

    /// Patch only the `conversation_id` for an existing entry. Used by
    /// the Codex async capture path: we wrote the entry on spawn with
    /// `conversation_id: None`, then patched it once we discovered the
    /// UUID. No-op if the entry doesn't exist.
    pub fn set_conversation_id(&self, task_id: &str, conv_id: &str) -> Result<()> {
        let patched = {
            let mut state = self.lock();
            match state.runs.get_mut(task_id) {
                Some(run) => {
                    run.conversation_id = Some(conv_id.to_string());
                    true
                }
                None => false,
            }
        };
        if patched {
            self.save()
        } else {
            Ok(())
        }
    }

    pub fn forget(&self, task_id: &str) -> Result<()> {
        let removed = {
            let mut state = self.lock();
            state.runs.remove(task_id).is_some()
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

    fn sample_run() -> TaskRun {
        TaskRun {
            agent: AgenteKind::ClaudeCode,
            model: "claude-opus-4-7".into(),
            conversation_id: Some("8f2a73e9-1111-2222-3333-444455556666".into()),
            last_started_at: Utc::now(),
            last_session_id: Some("S-deadbeef".into()),
        }
    }

    #[test]
    fn round_trip_upsert_get() {
        let dir = TempDir::new().unwrap();
        let tr = TaskRuns::load(dir.path()).unwrap();
        tr.upsert("T-1", sample_run()).unwrap();
        let got = tr.get("T-1").unwrap();
        assert_eq!(got.agent, AgenteKind::ClaudeCode);
        assert_eq!(got.model, "claude-opus-4-7");
        assert_eq!(
            got.conversation_id.as_deref(),
            Some("8f2a73e9-1111-2222-3333-444455556666")
        );
    }

    #[test]
    fn set_conversation_id_patches_existing() {
        let dir = TempDir::new().unwrap();
        let tr = TaskRuns::load(dir.path()).unwrap();
        let mut run = sample_run();
        run.agent = AgenteKind::Codex;
        run.conversation_id = None;
        tr.upsert("T-2", run).unwrap();
        tr.set_conversation_id("T-2", "thr-abc").unwrap();
        assert_eq!(
            tr.get("T-2").unwrap().conversation_id.as_deref(),
            Some("thr-abc")
        );
    }

    #[test]
    fn set_conversation_id_noop_when_missing() {
        let dir = TempDir::new().unwrap();
        let tr = TaskRuns::load(dir.path()).unwrap();
        // Doesn't error — capture races shouldn't crash anything.
        tr.set_conversation_id("T-nope", "thr-abc").unwrap();
        assert!(tr.get("T-nope").is_none());
    }

    #[test]
    fn forget_removes() {
        let dir = TempDir::new().unwrap();
        let tr = TaskRuns::load(dir.path()).unwrap();
        tr.upsert("T-3", sample_run()).unwrap();
        tr.forget("T-3").unwrap();
        assert!(tr.get("T-3").is_none());
        // idempotent
        tr.forget("T-3").unwrap();
    }

    #[test]
    fn survives_reload() {
        let dir = TempDir::new().unwrap();
        {
            let tr = TaskRuns::load(dir.path()).unwrap();
            tr.upsert("T-4", sample_run()).unwrap();
        }
        let tr2 = TaskRuns::load(dir.path()).unwrap();
        assert!(tr2.get("T-4").is_some());
    }

    #[test]
    fn corrupt_file_is_tolerated_as_empty() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("task-runs.json"), "{not json").unwrap();
        let tr = TaskRuns::load(dir.path()).unwrap();
        assert!(tr.snapshot().is_empty());
    }
}
