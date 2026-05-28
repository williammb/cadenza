//! File-backed `Ideia` store under `<home>/inbox/`.
//!
//! Cada ideia é um JSON `<id>.json` no formato `cadenza_proto::Ideia`.
//! Sem dedup nem waiters — ideias são criadas pela UI/CLI sob demanda
//! e não precisam de recovery especial. Diferentemente das tasks, o
//! schema não é congelado pelo legacy Node.js: o formato pode evoluir.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub use cadenza_proto::{Ideia, IdeiaStatus};

#[derive(Error, Debug)]
pub enum IdeiaError {
    #[error("ideia not found: {0}")]
    NotFound(String),
    #[error("ideia already exists: {0}")]
    AlreadyExists(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, IdeiaError>;

pub struct IdeiaStore {
    root: PathBuf,
}

impl IdeiaStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    pub fn list(&self) -> Result<Vec<Ideia>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.root) {
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
            match serde_json::from_str::<Ideia>(&text) {
                Ok(ideia) => out.push(ideia),
                Err(e) => {
                    tracing::warn!(
                        error = ?e,
                        path = %path.display(),
                        "skipping malformed ideia json"
                    );
                }
            }
        }
        // Ordem estável: mais antigas primeiro (created_at_ms cresce).
        out.sort_by_key(|i| i.created_at_ms);
        Ok(out)
    }

    pub fn read(&self, id: &str) -> Result<Option<Ideia>> {
        let path = self.path_for(id);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomic-uniqueness create: refuses to overwrite an existing
    /// `<id>.json`. Use this for first creation; `write` is the
    /// overwrite variant for updates like `set_status`.
    pub fn create(&self, ideia: &Ideia) -> Result<()> {
        use std::io::Write;
        let path = self.path_for(&ideia.id);
        let text = serde_json::to_string_pretty(ideia)?;
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => IdeiaError::AlreadyExists(ideia.id.clone()),
                _ => IdeiaError::Io(e),
            })?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        Ok(())
    }

    pub fn write(&self, ideia: &Ideia) -> Result<()> {
        let path = self.path_for(&ideia.id);
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(ideia)?;
        // fsync the tmp before rename — without it, a power loss after
        // rename can leave a zero-byte file on the visible path.
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

    pub fn delete(&self, id: &str) -> Result<()> {
        let path = self.path_for(id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(IdeiaError::NotFound(id.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_status(&self, id: &str, status: IdeiaStatus) -> Result<()> {
        let mut ideia = self
            .read(id)?
            .ok_or_else(|| IdeiaError::NotFound(id.to_string()))?;
        ideia.status = status;
        self.write(&ideia)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn mk(d: &TempDir) -> IdeiaStore {
        IdeiaStore::new(d.path().join("inbox")).unwrap()
    }

    fn sample(id: &str, status: IdeiaStatus) -> Ideia {
        Ideia {
            id: id.into(),
            titulo: format!("ideia {id}"),
            body: "corpo".into(),
            project_id: "proj-a".into(),
            status,
            created_at_ms: 42,
        }
    }

    #[test]
    fn write_read_round_trip() {
        let d = TempDir::new().unwrap();
        let store = mk(&d);
        store.write(&sample("I-1", IdeiaStatus::Pendente)).unwrap();
        let got = store.read("I-1").unwrap().unwrap();
        assert_eq!(got.titulo, "ideia I-1");
        assert_eq!(got.status, IdeiaStatus::Pendente);
    }

    #[test]
    fn list_skips_malformed_files() {
        let d = TempDir::new().unwrap();
        let store = mk(&d);
        store.write(&sample("I-1", IdeiaStatus::Pendente)).unwrap();
        fs::write(d.path().join("inbox").join("garbage.json"), "{not json").unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "I-1");
    }

    #[test]
    fn list_orders_by_created_at() {
        let d = TempDir::new().unwrap();
        let store = mk(&d);
        let mut a = sample("A", IdeiaStatus::Pendente);
        a.created_at_ms = 200;
        let mut b = sample("B", IdeiaStatus::Pendente);
        b.created_at_ms = 100;
        store.write(&a).unwrap();
        store.write(&b).unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed[0].id, "B");
        assert_eq!(listed[1].id, "A");
    }

    #[test]
    fn set_status_persists() {
        let d = TempDir::new().unwrap();
        let store = mk(&d);
        store.write(&sample("X", IdeiaStatus::Pendente)).unwrap();
        store.set_status("X", IdeiaStatus::Destrinchada).unwrap();
        assert_eq!(
            store.read("X").unwrap().unwrap().status,
            IdeiaStatus::Destrinchada
        );
    }

    #[test]
    fn delete_missing_errors_not_found() {
        let d = TempDir::new().unwrap();
        let store = mk(&d);
        assert!(matches!(store.delete("ZZ"), Err(IdeiaError::NotFound(_))));
    }
}
