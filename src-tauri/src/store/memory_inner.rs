//! File-backed memória compartilhada por projeto.
//!
//! Layout sob `<home>/memory/`:
//! ```text
//! memory/
//!   items/<project_id>.json     -> ProjectMemory (lista de MemoryItem)
//!   suggestions/<id>.json        -> MemorySuggestion (pendente)
//! ```
//! Itens são agrupados por projeto num único arquivo; sugestões pendentes
//! são um arquivo por sugestão (como ideias na Inbox) para que aprovar /
//! rejeitar seja só remover o arquivo. Schema novo, livre — não passa
//! pelo formato Node.js legacy.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use thiserror::Error;

pub use cadenza_proto::{MemoryItem, MemorySuggestion, ProjectMemory};

#[derive(Error, Debug)]
pub enum MemoryError {
    #[error("memory item not found: {0}")]
    ItemNotFound(String),
    #[error("memory suggestion not found: {0}")]
    SuggestionNotFound(String),
    #[error("memory suggestion already exists: {0}")]
    SuggestionExists(String),
    #[error("memory item already exists: {0}")]
    ItemExists(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, MemoryError>;

pub struct MemoryStore {
    items_dir: PathBuf,
    suggestions_dir: PathBuf,
    /// Serializes the read-modify-write of a project's `items/<id>.json`.
    /// Unlike tasks/ideias (one file per entity), all of a project's items
    /// share one file, so concurrent `add/update/delete` would otherwise
    /// race on read → mutate → atomic-rename and lose a write.
    items_write_lock: Mutex<()>,
}

impl MemoryStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let items_dir = root.join("items");
        let suggestions_dir = root.join("suggestions");
        fs::create_dir_all(&items_dir)?;
        fs::create_dir_all(&suggestions_dir)?;
        Ok(Self {
            items_dir,
            suggestions_dir,
            items_write_lock: Mutex::new(()),
        })
    }

    /// Acquire the items write lock, recovering from a poisoned mutex — the
    /// guarded section only does file IO, so a panic mid-write leaves no
    /// in-memory invariant to protect.
    fn lock_items(&self) -> std::sync::MutexGuard<'_, ()> {
        self.items_write_lock
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn items_path(&self, project_id: &str) -> PathBuf {
        self.items_dir.join(format!("{project_id}.json"))
    }

    fn suggestion_path(&self, id: &str) -> PathBuf {
        self.suggestions_dir.join(format!("{id}.json"))
    }

    fn read_memory(&self, project_id: &str) -> Result<ProjectMemory> {
        let path = self.items_path(project_id);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ProjectMemory {
                project_id: project_id.to_string(),
                items: Vec::new(),
            }),
            Err(e) => Err(e.into()),
        }
    }

    fn write_memory(&self, mem: &ProjectMemory) -> Result<()> {
        let path = self.items_path(&mem.project_id);
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(mem)?;
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
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn list(&self, project_id: &str) -> Result<Vec<MemoryItem>> {
        Ok(self.read_memory(project_id)?.items)
    }

    pub fn add_item(&self, project_id: &str, item: &MemoryItem) -> Result<()> {
        let _guard = self.lock_items();
        let mut mem = self.read_memory(project_id)?;
        // Reject a duplicate id so this matches the SQLite/Pg primary-key
        // contract: `copy_all` relies on `AlreadyExists` to skip rows when
        // re-migrating, and `update_item`/`delete_item` assume ids are unique.
        if mem.items.iter().any(|i| i.id == item.id) {
            return Err(MemoryError::ItemExists(item.id.clone()));
        }
        mem.items.push(item.clone());
        self.write_memory(&mem)
    }

    pub fn update_item(&self, project_id: &str, item_id: &str, texto: &str) -> Result<()> {
        let _guard = self.lock_items();
        let mut mem = self.read_memory(project_id)?;
        let item = mem
            .items
            .iter_mut()
            .find(|i| i.id == item_id)
            .ok_or_else(|| MemoryError::ItemNotFound(item_id.to_string()))?;
        item.texto = texto.to_string();
        self.write_memory(&mem)
    }

    pub fn delete_item(&self, project_id: &str, item_id: &str) -> Result<()> {
        let _guard = self.lock_items();
        let mut mem = self.read_memory(project_id)?;
        let before = mem.items.len();
        mem.items.retain(|i| i.id != item_id);
        if mem.items.len() == before {
            return Err(MemoryError::ItemNotFound(item_id.to_string()));
        }
        self.write_memory(&mem)
    }

    pub fn read_suggestion(&self, id: &str) -> Result<Option<MemorySuggestion>> {
        let path = self.suggestion_path(id);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn list_suggestions(&self, project_id: &str) -> Result<Vec<MemorySuggestion>> {
        let mut out = self.all_suggestions()?;
        out.retain(|s| s.project_id == project_id);
        Ok(out)
    }

    pub fn all_suggestions(&self) -> Result<Vec<MemorySuggestion>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.suggestions_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let text = fs::read_to_string(&path)?;
            match serde_json::from_str::<MemorySuggestion>(&text) {
                Ok(s) => out.push(s),
                Err(e) => tracing::warn!(
                    error = ?e,
                    path = %path.display(),
                    "skipping malformed memory suggestion json"
                ),
            }
        }
        out.sort_by_key(|s| s.criado_em);
        Ok(out)
    }

    pub fn create_suggestion(&self, s: &MemorySuggestion) -> Result<()> {
        use std::io::Write;
        let path = self.suggestion_path(&s.id);
        let text = serde_json::to_string_pretty(s)?;
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => MemoryError::SuggestionExists(s.id.clone()),
                _ => MemoryError::Io(e),
            })?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        Ok(())
    }

    pub fn delete_suggestion(&self, id: &str) -> Result<()> {
        let path = self.suggestion_path(id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(MemoryError::SuggestionNotFound(id.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Migration helper: every `(project_id, item)` pair across projects.
    pub fn all_items(&self) -> Result<Vec<(String, MemoryItem)>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.items_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let text = fs::read_to_string(&path)?;
            match serde_json::from_str::<ProjectMemory>(&text) {
                Ok(mem) => {
                    for item in mem.items {
                        out.push((mem.project_id.clone(), item));
                    }
                }
                Err(e) => tracing::warn!(
                    error = ?e,
                    path = %path.display(),
                    "skipping malformed project memory json"
                ),
            }
        }
        Ok(out)
    }

    pub fn root_dirs(&self) -> (&Path, &Path) {
        (&self.items_dir, &self.suggestions_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadenza_proto::SuggestionKind;
    use tempfile::TempDir;

    fn mk(d: &TempDir) -> MemoryStore {
        MemoryStore::new(d.path().join("memory")).unwrap()
    }

    fn item(id: &str) -> MemoryItem {
        MemoryItem {
            id: id.into(),
            texto: format!("fato {id}"),
            origem_task: None,
            criado_em: 1,
        }
    }

    #[test]
    fn item_crud_round_trip() {
        let d = TempDir::new().unwrap();
        let s = mk(&d);
        assert!(s.list("p1").unwrap().is_empty());
        s.add_item("p1", &item("M-1")).unwrap();
        s.add_item("p1", &item("M-2")).unwrap();
        assert_eq!(s.list("p1").unwrap().len(), 2);
        s.update_item("p1", "M-1", "novo texto").unwrap();
        assert_eq!(s.list("p1").unwrap()[0].texto, "novo texto");
        s.delete_item("p1", "M-1").unwrap();
        assert_eq!(s.list("p1").unwrap().len(), 1);
        assert!(matches!(
            s.delete_item("p1", "M-1"),
            Err(MemoryError::ItemNotFound(_))
        ));
    }

    #[test]
    fn add_item_rejects_duplicate_id() {
        let d = TempDir::new().unwrap();
        let s = mk(&d);
        s.add_item("p1", &item("M-1")).unwrap();
        // Re-adding the same id must fail (mirrors the SQLite/Pg PK), so
        // re-migration skips instead of duplicating.
        assert!(matches!(
            s.add_item("p1", &item("M-1")),
            Err(MemoryError::ItemExists(_))
        ));
        assert_eq!(s.list("p1").unwrap().len(), 1);
    }

    #[test]
    fn memory_is_scoped_per_project() {
        let d = TempDir::new().unwrap();
        let s = mk(&d);
        s.add_item("p1", &item("M-1")).unwrap();
        s.add_item("p2", &item("M-2")).unwrap();
        assert_eq!(s.list("p1").unwrap().len(), 1);
        assert_eq!(s.list("p2").unwrap().len(), 1);
        assert_eq!(s.list("p1").unwrap()[0].id, "M-1");
    }

    #[test]
    fn suggestion_crud_round_trip() {
        let d = TempDir::new().unwrap();
        let s = mk(&d);
        let sug = MemorySuggestion {
            id: "S-1".into(),
            project_id: "p1".into(),
            criado_em: 5,
            kind: SuggestionKind::Nova {
                texto: "convenção".into(),
            },
        };
        s.create_suggestion(&sug).unwrap();
        assert!(matches!(
            s.create_suggestion(&sug),
            Err(MemoryError::SuggestionExists(_))
        ));
        assert_eq!(s.list_suggestions("p1").unwrap().len(), 1);
        assert_eq!(s.list_suggestions("p2").unwrap().len(), 0);
        assert_eq!(s.read_suggestion("S-1").unwrap().unwrap().project_id, "p1");
        s.delete_suggestion("S-1").unwrap();
        assert!(s.read_suggestion("S-1").unwrap().is_none());
        assert!(matches!(
            s.delete_suggestion("S-1"),
            Err(MemoryError::SuggestionNotFound(_))
        ));
    }

    #[test]
    fn all_items_spans_projects() {
        let d = TempDir::new().unwrap();
        let s = mk(&d);
        s.add_item("p1", &item("M-1")).unwrap();
        s.add_item("p2", &item("M-2")).unwrap();
        let all = s.all_items().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|(p, i)| p == "p1" && i.id == "M-1"));
        assert!(all.iter().any(|(p, i)| p == "p2" && i.id == "M-2"));
    }
}
