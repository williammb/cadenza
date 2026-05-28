//! Task CRUD over `.md` + YAML frontmatter with file-level locks.
//!
//! Per DESIGN-desktop-v2.md § "store.rs" and § "Concorrência e
//! integridade do store":
//! - Writes go through an `fs2` advisory exclusive lock with 500ms→3s
//!   exponential backoff; lock failure returns `Busy`.
//! - `append_log` skips the lock and relies on `O_APPEND` atomicity
//!   for writes <4KB (POSIX + NTFS).
//!
//! Wired into Tauri commands in Phase 2-3; allow dead_code until then.
#![allow(dead_code)]

use cadenza_proto::{Estado, Task};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("task not found: {0}")]
    NotFound(String),
    #[error("task already exists: {0}")]
    AlreadyExists(String),
    #[error("task busy: failed to acquire lock within 3s")]
    Busy,
    #[error("invalid frontmatter: {0}")]
    BadFrontmatter(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// YAML frontmatter view — the on-disk fields, body excluded.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskMeta {
    id: String,
    titulo: String,
    estado: Estado,
    responsavel: String,
}

impl TaskMeta {
    fn from_task(t: &Task) -> Self {
        Self {
            id: t.id.clone(),
            titulo: t.titulo.clone(),
            estado: t.estado,
            responsavel: t.responsavel.clone(),
        }
    }
}

/// Filesystem-backed task store rooted at a single directory.
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, id: &str) -> PathBuf {
        self.root.join(format!("{}.md", id))
    }

    /// Side-car lock file sitting next to `<id>.md`. Held during
    /// read-modify-write so we can release the data file before the
    /// atomic `rename` (Windows refuses to replace an open file
    /// without `FILE_SHARE_DELETE`, which Rust's default open doesn't
    /// request).
    fn lock_path_for(&self, id: &str) -> PathBuf {
        self.root.join(format!("{}.md.lock", id))
    }

    fn tmp_path_for(&self, id: &str) -> PathBuf {
        self.root.join(format!("{}.md.tmp", id))
    }

    pub fn list_tasks(&self, filter: Option<Estado>) -> Result<Vec<Task>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            match read_file(&path) {
                Ok(task) => {
                    if filter.map_or(true, |f| f == task.estado) {
                        out.push(task);
                    }
                }
                Err(e) => {
                    tracing::warn!(path = ?path, error = %e, "skipping malformed task file");
                }
            }
        }
        Ok(out)
    }

    pub fn read_task(&self, id: &str) -> Result<Task> {
        let path = self.path_for(id);
        if !path.exists() {
            return Err(StoreError::NotFound(id.to_string()));
        }
        read_file(&path)
    }

    pub fn create_task(&self, task: &Task) -> Result<()> {
        let path = self.path_for(&task.id);
        let content = render(&TaskMeta::from_task(task), &task.body);
        // `create_new` is atomic — two racing creators can't both
        // observe "doesn't exist" and silently overwrite each other
        // (the previous `exists()` + `write` had a TOCTOU window and
        // diverged from the DB backends' PRIMARY KEY semantics).
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => StoreError::AlreadyExists(task.id.clone()),
                _ => StoreError::Io(e),
            })?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    pub fn set_estado(&self, id: &str, estado: Estado) -> Result<()> {
        self.update_meta(id, |m| m.estado = estado)
    }

    pub fn set_titulo(&self, id: &str, titulo: &str) -> Result<()> {
        let new = titulo.to_string();
        self.update_meta(id, |m| m.titulo = new)
    }

    pub fn set_responsavel(&self, id: &str, responsavel: &str) -> Result<()> {
        let new = responsavel.to_string();
        self.update_meta(id, |m| m.responsavel = new)
    }

    pub fn update_task_body(&self, id: &str, body: &str) -> Result<()> {
        let new_body = body.to_string();
        self.with_locked(id, |_meta, body_slot| {
            *body_slot = new_body;
            Ok(())
        })
    }

    pub fn delete_task(&self, id: &str) -> Result<()> {
        let path = self.path_for(id);
        if !path.exists() {
            return Err(StoreError::NotFound(id.to_string()));
        }
        fs::remove_file(&path)?;
        Ok(())
    }

    /// Append a log line. Per design, this uses `O_APPEND` without a
    /// lock — POSIX and NTFS both serialize sub-4KB appends.
    pub fn append_log(&self, id: &str, line: &str) -> Result<()> {
        let path = self.path_for(id);
        if !path.exists() {
            return Err(StoreError::NotFound(id.to_string()));
        }
        let mut file = OpenOptions::new().append(true).open(&path)?;
        let mut payload = String::with_capacity(line.len() + 1);
        payload.push_str(line);
        if !line.ends_with('\n') {
            payload.push('\n');
        }
        file.write_all(payload.as_bytes())?;
        Ok(())
    }

    fn update_meta<F: FnOnce(&mut TaskMeta)>(&self, id: &str, mutate: F) -> Result<()> {
        self.with_locked(id, |meta, _body| {
            mutate(meta);
            Ok(())
        })
    }

    /// Read → mutate → atomic write under an exclusive advisory lock.
    /// The lock is held on a side-car `.lock` file (not the data file)
    /// so the data file is closed before `rename`, matching Windows'
    /// move-over semantics. Atomicity: writes hit `<id>.md.tmp` with
    /// `sync_all` then `rename` over `<id>.md`. A crash mid-write
    /// leaves the original intact and a `.tmp` leftover.
    fn with_locked<F, T>(&self, id: &str, f: F) -> Result<T>
    where
        F: FnOnce(&mut TaskMeta, &mut String) -> Result<T>,
    {
        let path = self.path_for(id);
        if !path.exists() {
            return Err(StoreError::NotFound(id.to_string()));
        }
        let lock_path = self.lock_path_for(id);
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&lock_path)?;
        acquire_lock(&lock_file)?;

        let content = fs::read_to_string(&path)?;
        let (mut meta, mut body) = parse_frontmatter(&content)?;
        let result = f(&mut meta, &mut body)?;

        let new_content = render(&meta, &body);
        let tmp = self.tmp_path_for(id);
        {
            let mut tmp_file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            tmp_file.write_all(new_content.as_bytes())?;
            tmp_file.sync_all()?;
        }
        fs::rename(&tmp, &path)?;

        let _ = FileExt::unlock(&lock_file);
        Ok(result)
    }
}

fn acquire_lock(file: &File) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut backoff = Duration::from_millis(20);
    loop {
        match FileExt::try_lock_exclusive(file) {
            Ok(()) => return Ok(()),
            Err(_) => {
                if Instant::now() >= deadline {
                    return Err(StoreError::Busy);
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(500));
            }
        }
    }
}

fn read_file(path: &Path) -> Result<Task> {
    let content = fs::read_to_string(path)?;
    let (meta, body) = parse_frontmatter(&content)?;
    Ok(Task {
        id: meta.id,
        titulo: meta.titulo,
        estado: meta.estado,
        responsavel: meta.responsavel,
        body,
    })
}

fn parse_frontmatter(content: &str) -> Result<(TaskMeta, String)> {
    // Strip BOM + leading whitespace before checking for the opener.
    let s = content.trim_start_matches('\u{FEFF}');
    let s = s.trim_start_matches(['\r', '\n', ' ', '\t']);
    if !s.starts_with("---") {
        return Err(StoreError::BadFrontmatter("missing opening ---".into()));
    }
    let after_open = &s[3..];
    let after_open = after_open.strip_prefix('\r').unwrap_or(after_open);
    let after_open = after_open
        .strip_prefix('\n')
        .ok_or_else(|| StoreError::BadFrontmatter("opener not followed by newline".into()))?;
    let close = after_open
        .find("\n---")
        .ok_or_else(|| StoreError::BadFrontmatter("missing closing ---".into()))?;
    let yaml = &after_open[..close];
    let rest = &after_open[close + 4..];
    let body = rest.trim_start_matches(['\r', '\n']).to_string();
    let meta: TaskMeta = serde_yaml::from_str(yaml)?;
    Ok((meta, body))
}

fn render(meta: &TaskMeta, body: &str) -> String {
    let yaml = serde_yaml::to_string(meta).expect("TaskMeta is always serializable");
    let body = body.trim_end_matches(['\r', '\n']);
    if body.is_empty() {
        format!("---\n{}---\n", yaml)
    } else {
        format!("---\n{}---\n\n{}\n", yaml, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn t(id: &str, estado: Estado) -> Task {
        Task {
            id: id.into(),
            titulo: format!("{} title", id),
            estado,
            responsavel: "humano".into(),
            body: format!("body of {}", id),
        }
    }

    #[test]
    fn create_and_read_round_trip() {
        let dir = TempDir::new().unwrap();
        let s = Store::new(dir.path()).unwrap();
        let task = t("T-1", Estado::Fazendo);
        s.create_task(&task).unwrap();

        let got = s.read_task("T-1").unwrap();
        assert_eq!(got.id, "T-1");
        assert_eq!(got.titulo, "T-1 title");
        assert_eq!(got.estado, Estado::Fazendo);
        assert_eq!(got.responsavel, "humano");
        assert_eq!(got.body.trim(), "body of T-1");
    }

    #[test]
    fn list_filters_by_estado() {
        let dir = TempDir::new().unwrap();
        let s = Store::new(dir.path()).unwrap();
        s.create_task(&t("A", Estado::AFazer)).unwrap();
        s.create_task(&t("B", Estado::Fazendo)).unwrap();
        s.create_task(&t("C", Estado::Fazendo)).unwrap();
        s.create_task(&t("D", Estado::Feito)).unwrap();

        let only_fazendo = s.list_tasks(Some(Estado::Fazendo)).unwrap();
        assert_eq!(only_fazendo.len(), 2);

        let all = s.list_tasks(None).unwrap();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn set_estado_preserves_body_and_other_fields() {
        let dir = TempDir::new().unwrap();
        let s = Store::new(dir.path()).unwrap();
        s.create_task(&t("X", Estado::AFazer)).unwrap();
        s.set_estado("X", Estado::Fazendo).unwrap();

        let got = s.read_task("X").unwrap();
        assert_eq!(got.estado, Estado::Fazendo);
        assert_eq!(got.titulo, "X title");
        assert_eq!(got.responsavel, "humano");
        assert_eq!(got.body.trim(), "body of X");
    }

    #[test]
    fn update_task_body_preserves_meta() {
        let dir = TempDir::new().unwrap();
        let s = Store::new(dir.path()).unwrap();
        s.create_task(&t("U", Estado::Fazendo)).unwrap();
        s.update_task_body("U", "new body content\nmultiline").unwrap();

        let got = s.read_task("U").unwrap();
        assert_eq!(got.estado, Estado::Fazendo);
        assert!(got.body.contains("new body content"));
        assert!(got.body.contains("multiline"));
        assert!(!got.body.contains("body of U"));
    }

    #[test]
    fn append_log_adds_line_to_body() {
        let dir = TempDir::new().unwrap();
        let s = Store::new(dir.path()).unwrap();
        s.create_task(&t("Y", Estado::Fazendo)).unwrap();
        s.append_log("Y", "first log line").unwrap();
        s.append_log("Y", "second").unwrap();

        let got = s.read_task("Y").unwrap();
        assert!(got.body.contains("first log line"));
        assert!(got.body.contains("second"));
    }

    #[test]
    fn delete_removes_file() {
        let dir = TempDir::new().unwrap();
        let s = Store::new(dir.path()).unwrap();
        s.create_task(&t("Z", Estado::AFazer)).unwrap();
        s.delete_task("Z").unwrap();
        assert!(matches!(s.read_task("Z"), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn read_missing_returns_not_found() {
        let dir = TempDir::new().unwrap();
        let s = Store::new(dir.path()).unwrap();
        assert!(matches!(s.read_task("nope"), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn create_duplicate_errors() {
        let dir = TempDir::new().unwrap();
        let s = Store::new(dir.path()).unwrap();
        s.create_task(&t("D", Estado::AFazer)).unwrap();
        assert!(matches!(
            s.create_task(&t("D", Estado::Fazendo)),
            Err(StoreError::AlreadyExists(_))
        ));
    }

    #[test]
    fn lock_blocks_second_writer_then_releases() {
        // Open the same file twice. First handle takes an exclusive lock;
        // second handle's try_lock_exclusive must fail immediately.
        // After the first unlocks, the second succeeds.
        let dir = TempDir::new().unwrap();
        let s = Store::new(dir.path()).unwrap();
        s.create_task(&t("L", Estado::Fazendo)).unwrap();
        let path = s.root.join("L.md");

        let f1 = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let f2 = OpenOptions::new().read(true).write(true).open(&path).unwrap();

        FileExt::try_lock_exclusive(&f1).expect("first lock should succeed");
        assert!(FileExt::try_lock_exclusive(&f2).is_err(), "second lock must fail while first holds");

        FileExt::unlock(&f1).unwrap();
        FileExt::try_lock_exclusive(&f2).expect("second lock should succeed after first unlocks");
        FileExt::unlock(&f2).unwrap();
    }

    #[test]
    fn malformed_frontmatter_is_rejected() {
        let dir = TempDir::new().unwrap();
        let s = Store::new(dir.path()).unwrap();
        let path = s.root.join("BAD.md");
        fs::write(&path, "no frontmatter here").unwrap();
        // list_tasks tolerates malformed files (logs + skips).
        let listed = s.list_tasks(None).unwrap();
        assert!(listed.is_empty());
        // read_task surfaces the error.
        assert!(matches!(s.read_task("BAD"), Err(StoreError::BadFrontmatter(_))));
    }

    // Verifies the retry path inside `acquire_lock`: a background thread holds
    // the sidecar lock file while the main thread's `set_estado` spins with
    // exponential backoff; once the holder releases (~200 ms), the update
    // succeeds rather than timing out.
    #[test]
    fn concurrent_write_retries_after_lock_release() {
        use std::time::Duration;

        let dir = TempDir::new().unwrap();
        let s = Store::new(dir.path()).unwrap();
        s.create_task(&t("L", Estado::Fazendo)).unwrap();

        // Take the sidecar lock so Store::with_locked spins in acquire_lock.
        let lock_path = s.lock_path_for("L");
        let lf = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&lock_path)
            .unwrap();
        FileExt::try_lock_exclusive(&lf).expect("initial lock must succeed");

        let dir_path = dir.path().to_path_buf();
        let handle = std::thread::spawn(move || {
            Store::new(dir_path).unwrap().set_estado("L", Estado::Feito)
        });

        // Release after 200 ms — well within the 3-second deadline.
        std::thread::sleep(Duration::from_millis(200));
        FileExt::unlock(&lf).unwrap();

        handle.join().unwrap().expect("set_estado must succeed once the lock is released");
        assert_eq!(s.read_task("L").unwrap().estado, Estado::Feito);
    }
}
