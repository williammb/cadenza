//! Filesystem-backed `Repository` impl.
//!
//! Wraps the original sync engines (`files_inner::Store`,
//! `triage_inner::Triage`) and exposes their surfaces through the async
//! trait. Each method just `?`-converts the inner error into the
//! unified `StoreError`.
//!
//! No `spawn_blocking`: filesystem ops on a desktop are sub-millisecond,
//! Tauri commands already run on a worker thread, and adding a thread
//! hop would cost more than it saves. If profiling later shows real
//! contention, switch hot paths to `tokio::task::spawn_blocking`.

use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::{
    files_inner::Store as FileStore, ideias_inner::IdeiaStore, memory_inner::MemoryStore,
    triage_inner::Triage as FileTriage, validate_id, DecisaoRegistro, Estado, Ideia, IdeiaStatus,
    MemoryItem, MemorySuggestion, NewProposta, Proposta, Repository, Result, StoreError, Task,
};

/// Tasks live under `<home>/tasks/`, triage under `<home>/triage/`,
/// ideias under `<home>/inbox/`, memória sob `<home>/memory/`.
pub struct FileRepository {
    tasks: Arc<FileStore>,
    triage: Arc<FileTriage>,
    ideias: Arc<IdeiaStore>,
    memory: Arc<MemoryStore>,
}

impl FileRepository {
    pub fn new(home: &Path) -> Result<Self> {
        let tasks = FileStore::new(home.join("tasks")).map_err(StoreError::Io)?;
        let triage = FileTriage::new(home.join("triage"))?;
        let ideias = IdeiaStore::new(home.join("inbox"))?;
        let memory = MemoryStore::new(home.join("memory"))?;
        Ok(Self {
            tasks: Arc::new(tasks),
            triage: Arc::new(triage),
            ideias: Arc::new(ideias),
            memory: Arc::new(memory),
        })
    }
}

#[async_trait]
impl Repository for FileRepository {
    async fn list_tasks(&self, filter: Option<Estado>) -> Result<Vec<Task>> {
        Ok(self.tasks.list_tasks(filter)?)
    }

    async fn read_task(&self, id: &str) -> Result<Task> {
        Ok(self.tasks.read_task(id)?)
    }

    async fn create_task(&self, task: &Task) -> Result<()> {
        Ok(self.tasks.create_task(task)?)
    }

    async fn set_estado(&self, id: &str, estado: Estado) -> Result<()> {
        Ok(self.tasks.set_estado(id, estado)?)
    }

    async fn set_titulo(&self, id: &str, titulo: &str) -> Result<()> {
        Ok(self.tasks.set_titulo(id, titulo)?)
    }

    async fn update_task_body(&self, id: &str, body: &str) -> Result<()> {
        Ok(self.tasks.update_task_body(id, body)?)
    }

    async fn delete_task(&self, id: &str) -> Result<()> {
        Ok(self.tasks.delete_task(id)?)
    }

    async fn append_log(&self, id: &str, text: &str) -> Result<()> {
        Ok(self.tasks.append_log(id, text)?)
    }

    async fn propose(&self, args: NewProposta) -> Result<Proposta> {
        Ok(self.triage.propose(args)?)
    }

    async fn read_proposta(&self, proposta_id: &str) -> Result<Option<Proposta>> {
        Ok(self.triage.read_proposta(proposta_id)?)
    }

    async fn read_decisao(&self, proposta_id: &str) -> Result<Option<DecisaoRegistro>> {
        Ok(self.triage.read_decisao(proposta_id)?)
    }

    async fn list_pending_propostas(&self) -> Result<Vec<Proposta>> {
        Ok(self.triage.list_pending()?)
    }

    async fn write_decisao(&self, registro: DecisaoRegistro) -> Result<()> {
        Ok(self.triage.write_decisao(registro)?)
    }

    async fn await_decisao(
        &self,
        proposta_id: &str,
        timeout: Duration,
    ) -> Result<Option<DecisaoRegistro>> {
        Ok(self.triage.await_decisao(proposta_id, timeout).await?)
    }

    async fn list_ideias(&self) -> Result<Vec<Ideia>> {
        Ok(self.ideias.list()?)
    }

    async fn read_ideia(&self, id: &str) -> Result<Option<Ideia>> {
        Ok(self.ideias.read(id)?)
    }

    async fn create_ideia(&self, ideia: &Ideia) -> Result<()> {
        // `create` (atomic via OpenOptions::create_new) replaces the
        // earlier `read + write` check-then-act, which had a TOCTOU
        // race where two concurrent creators with the same id both
        // saw `read == None` and the second silently overwrote the
        // first. DB backends enforce this via PRIMARY KEY.
        Ok(self.ideias.create(ideia)?)
    }

    async fn delete_ideia(&self, id: &str) -> Result<()> {
        Ok(self.ideias.delete(id)?)
    }

    async fn set_ideia_status(&self, id: &str, status: IdeiaStatus) -> Result<()> {
        Ok(self.ideias.set_status(id, status)?)
    }

    // ─── memória ───────────────────────────────────────────────────
    // `project_id` é o nome do arquivo no backend de arquivos, então
    // validamos contra path traversal antes de qualquer `path_for`.

    async fn list_memory(&self, project_id: &str) -> Result<Vec<MemoryItem>> {
        validate_id(project_id)?;
        Ok(self.memory.list(project_id)?)
    }

    async fn add_memory_item(&self, project_id: &str, item: &MemoryItem) -> Result<()> {
        validate_id(project_id)?;
        Ok(self.memory.add_item(project_id, item)?)
    }

    async fn update_memory_item(&self, project_id: &str, item_id: &str, texto: &str) -> Result<()> {
        validate_id(project_id)?;
        Ok(self.memory.update_item(project_id, item_id, texto)?)
    }

    async fn delete_memory_item(&self, project_id: &str, item_id: &str) -> Result<()> {
        validate_id(project_id)?;
        Ok(self.memory.delete_item(project_id, item_id)?)
    }

    async fn list_memory_suggestions(&self, project_id: &str) -> Result<Vec<MemorySuggestion>> {
        Ok(self.memory.list_suggestions(project_id)?)
    }

    async fn read_memory_suggestion(&self, id: &str) -> Result<Option<MemorySuggestion>> {
        validate_id(id)?;
        Ok(self.memory.read_suggestion(id)?)
    }

    async fn create_memory_suggestion(&self, suggestion: &MemorySuggestion) -> Result<()> {
        validate_id(&suggestion.id)?;
        Ok(self.memory.create_suggestion(suggestion)?)
    }

    async fn delete_memory_suggestion(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        Ok(self.memory.delete_suggestion(id)?)
    }

    async fn all_memory_items(&self) -> Result<Vec<(String, MemoryItem)>> {
        Ok(self.memory.all_items()?)
    }

    async fn all_memory_suggestions(&self) -> Result<Vec<MemorySuggestion>> {
        Ok(self.memory.all_suggestions()?)
    }
}
