//! File backend for the run timeline / audit event log (feature #8).
//!
//! Append-only NDJSON at `<home>/events/events.ndjson` — one JSON line per
//! [`RunEvent`]. NDJSON-append (rather than one-file-per-event) is chosen
//! because file order == insertion order, which gives a stable timeline
//! without a sequence column to match the SQL backends' `seq`.
//!
//! Concurrency: only the app process writes events, so an in-process
//! `Mutex` around the append is enough (mirrors `memory_inner`'s shared-file
//! guard). Each append is `write_all` + `sync_all` (fsync) so a power loss
//! can't leave a torn line visible. Reads skip-and-warn on a malformed line
//! rather than aborting the whole list (mirrors `ideias_inner` listing).

use cadenza_proto::RunEvent;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad data: {0}")]
    BadData(String),
}

type Result<T> = std::result::Result<T, EventError>;

/// Append-only event store rooted at `<home>/events/`.
pub struct EventStore {
    path: PathBuf,
    /// Serializes concurrent appends within this process.
    write_lock: Mutex<()>,
}

impl EventStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self {
            path: root.join("events.ndjson"),
            write_lock: Mutex::new(()),
        })
    }

    /// Append one event as a JSON line (fsync before returning).
    pub fn append(&self, ev: &RunEvent) -> Result<()> {
        let line = serde_json::to_string(ev).map_err(|e| EventError::BadData(e.to_string()))?;
        // Recover from a poisoned lock: a panic mid-append can't corrupt the
        // file (append is a single write_all), so the guard is reusable.
        let _g = self.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        Ok(())
    }

    /// Events in insertion order (oldest first), optionally filtered by
    /// `task_id` and capped to the most-recent `limit`.
    pub fn list(&self, task_id: Option<&str>, limit: Option<i64>) -> Result<Vec<RunEvent>> {
        let mut events = self.read_all()?;
        if let Some(t) = task_id {
            events.retain(|e| e.task_id.as_deref() == Some(t));
        }
        if let Some(n) = limit {
            let n = n.max(0) as usize;
            if events.len() > n {
                events = events.split_off(events.len() - n);
            }
        }
        Ok(events)
    }

    /// Every event, insertion order (migration dump).
    pub fn all(&self) -> Result<Vec<RunEvent>> {
        self.read_all()
    }

    /// Append a raw payload line verbatim (migration). The line IS the stored
    /// representation, so no decode/re-encode happens — unknown future event
    /// kinds survive.
    pub fn append_raw(&self, payload: &str) -> Result<()> {
        let _g = self.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(payload.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        Ok(())
    }

    /// Raw migration dump: each event's verbatim line + the fields the SQL
    /// backends promote to columns, extracted without decoding into a typed
    /// `RunEvent` (so an unknown `tipo` is preserved, not flattened).
    pub fn all_raw(&self) -> Result<Vec<super::RawEvent>> {
        let f = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "skipping malformed raw event line");
                    continue;
                }
            };
            let Some(id) = v.get("id").and_then(|x| x.as_str()) else {
                tracing::warn!("skipping raw event line without id");
                continue;
            };
            out.push(super::RawEvent {
                id: id.to_string(),
                task_id: v
                    .get("task_id")
                    .and_then(|x| x.as_str())
                    .map(str::to_string),
                kind: v
                    .get("kind")
                    .and_then(|k| k.get("tipo"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("desconhecido")
                    .to_string(),
                ts_ms: v.get("ts_ms").and_then(|x| x.as_i64()).unwrap_or(0),
                payload: line,
            });
        }
        Ok(out)
    }

    fn read_all(&self) -> Result<Vec<RunEvent>> {
        let f = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<RunEvent>(&line) {
                Ok(ev) => out.push(ev),
                // Don't abort the whole list on one bad line.
                Err(e) => tracing::warn!(error = %e, "skipping malformed event line"),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadenza_proto::RunEventKind;
    use tempfile::TempDir;

    fn ev(id: &str, task: Option<&str>, ts: i64) -> RunEvent {
        RunEvent::new(
            id.into(),
            ts,
            task.map(|t| t.to_string()),
            RunEventKind::DoneEnviado {
                resumo: Some("x".into()),
                com_evidencia: false,
            },
        )
    }

    #[test]
    fn append_then_list_preserves_insertion_order() {
        let dir = TempDir::new().unwrap();
        let store = EventStore::new(dir.path().join("events")).unwrap();
        store.append(&ev("E-1", Some("T-1"), 10)).unwrap();
        store.append(&ev("E-2", Some("T-2"), 20)).unwrap();
        store.append(&ev("E-3", Some("T-1"), 30)).unwrap();
        let all = store.list(None, None).unwrap();
        let ids: Vec<&str> = all.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["E-1", "E-2", "E-3"]);
    }

    #[test]
    fn list_filters_by_task_and_caps_to_most_recent() {
        let dir = TempDir::new().unwrap();
        let store = EventStore::new(dir.path().join("events")).unwrap();
        for (i, t) in [("E-1", "T-1"), ("E-2", "T-1"), ("E-3", "T-1")]
            .iter()
            .enumerate()
        {
            store.append(&ev(t.0, Some(t.1), i as i64)).unwrap();
        }
        let only_t1 = store.list(Some("T-1"), Some(2)).unwrap();
        let ids: Vec<&str> = only_t1.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["E-2", "E-3"]); // most-recent 2, still oldest-first
    }

    #[test]
    fn empty_when_no_file() {
        let dir = TempDir::new().unwrap();
        let store = EventStore::new(dir.path().join("events")).unwrap();
        assert!(store.all().unwrap().is_empty());
    }
}
