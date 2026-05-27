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
    files_inner::Store as FileStore,
    ideias_inner::IdeiaStore,
    triage_inner::Triage as FileTriage,
    DecisaoRegistro, Estado, Ideia, IdeiaStatus, NewProposta, Proposta, Repository, Result,
    StoreError, Task,
};

/// Tasks live under `<home>/tasks/`, triage under `<home>/triage/`,
/// ideias under `<home>/inbox/`.
pub struct FileRepository {
    tasks: Arc<FileStore>,
    triage: Arc<FileTriage>,
    ideias: Arc<IdeiaStore>,
}

impl FileRepository {
    pub fn new(home: &Path) -> Result<Self> {
        let tasks = FileStore::new(home.join("tasks"))
            .map_err(StoreError::Io)?;
        let triage = FileTriage::new(home.join("triage"))?;
        let ideias = IdeiaStore::new(home.join("inbox"))?;
        Ok(Self {
            tasks: Arc::new(tasks),
            triage: Arc::new(triage),
            ideias: Arc::new(ideias),
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
}
