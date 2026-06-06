//! Typed op args and results.
//!
//! Wire `Request.args` is `serde_json::Value`; the server `from_value`s
//! it into the op-specific `Args` struct, and `to_value`s the matching
//! `Result` for the `Response.result`. The op name (string) is the only
//! discriminator on the wire.
//!
//! Op string constants live as `pub const NAME: &str`. Use them in both
//! server dispatch and client request construction so a typo fails to
//! compile rather than silently routing nowhere.

use serde::{Deserialize, Serialize};

use crate::{
    DecisaoRegistro, Ideia, IdeiaStatus, MemoryItem, NewProposta, ProjectInfo, SuggestionKind, Task,
};

// ───────── op name constants

pub const OP_HELLO: &str = "hello";
pub const OP_LIST_TASKS: &str = "list_tasks";
pub const OP_CURRENT_TASK: &str = "current_task";
pub const OP_APPEND_LOG: &str = "append_log";
pub const OP_PROPOSE: &str = "propose";
pub const OP_AWAIT_DECISION: &str = "await_decision";
pub const OP_DONE: &str = "done";
pub const OP_BYE: &str = "bye";

// Adicionados no protocolo v2 — Inbox + criação de task via CLI.
pub const OP_CREATE_TASK: &str = "create_task";
pub const OP_LIST_IDEIAS: &str = "list_ideias";
pub const OP_READ_IDEIA: &str = "read_ideia";
pub const OP_CREATE_IDEIA: &str = "create_ideia";
pub const OP_DELETE_IDEIA: &str = "delete_ideia";
pub const OP_SET_IDEIA_STATUS: &str = "set_ideia_status";

// Worktree System. Adicionado sob o protocolo atual (sem bump de
// MIN/MAX_PROTOCOL): o dispatch casa pelo nome da op, não por número de
// versão negociado, então qualquer par dentro da janela atual pode
// chamá-la. Se a semântica algum dia exigir negociação, suba MAX_PROTOCOL.
pub const OP_SET_TASK_WORKTREE: &str = "set_task_worktree";

// Plan mode: rewrite a task's body (used by `cadenza-cli plan`). Added
// under the current protocol window, same rationale as the worktree op
// above — dispatch matches on the op name, not a negotiated version.
pub const OP_UPDATE_BODY: &str = "update_body";

// Read a single task by id (`cadenza-cli get`) and list configured
// projects (`cadenza-cli projects`). Same op-name dispatch rationale as
// the ops above — no MIN/MAX_PROTOCOL bump.
pub const OP_READ_TASK: &str = "read_task";
pub const OP_LIST_PROJECTS: &str = "list_projects";

// Memória compartilhada por projeto (T-34). Mesmo racional de dispatch
// por nome de op das adições acima — sem bump de MIN/MAX_PROTOCOL.
// `OP_LIST_MEMORY` é a releitura da memória oficial pelo agente;
// `OP_SUGGEST_LEARNING` é o aprendizado proposto pelo agente de execução;
// `OP_REVISE_MEMORY` é uma operação de reavaliação proposta pelo agente
// de reeval. Aprendizados/ops só viram memória após curadoria na UI.
pub const OP_LIST_MEMORY: &str = "list_memory";
pub const OP_SUGGEST_LEARNING: &str = "suggest_learning";
pub const OP_REVISE_MEMORY: &str = "revise_memory";

// Review package (PLAN §B/§E). `quality` returns the per-project contract;
// `review_decision` is the human approve/request-changes op. Evidence on
// `done` reuses OP_DONE with extended args (see done mod). These ops gate on
// MAX_PROTOCOL = 3 (see lib.rs); negotiation per PLAN §B.6.
pub const OP_QUALITY: &str = "quality";
pub const OP_REVIEW_DECISION: &str = "review_decision";

// Jira (Slice 2): server-stamped materialization of an analysis run into
// proposals. The run is authorized by a capability secret (verified
// server-side), and the server — not the caller — stamps the Jira identity
// onto each created proposal. Same op-name dispatch rationale as the ops
// above — no MIN/MAX_PROTOCOL bump.
pub const OP_JIRA_MATERIALIZE: &str = "jira_materialize";

// Jira (Slice 3): pure HTTP/data building blocks. `jira_test_connection`
// checks credentials, `jira_fetch_issue` fetches one issue, and
// `jira_list_assigned` lists the caller's open issues. These RETURN data
// only — no persistence, no run minting. Same op-name dispatch rationale
// as the ops above — no MIN/MAX_PROTOCOL bump.
pub const OP_JIRA_TEST_CONNECTION: &str = "jira_test_connection";
pub const OP_JIRA_FETCH_ISSUE: &str = "jira_fetch_issue";
pub const OP_JIRA_LIST_ASSIGNED: &str = "jira_list_assigned";

// Jira (Slice 5): build + persist the aggregate (issue-owned) review — the
// committed branch diff of the shared per-issue worktree. STATE-NEUTRAL: it
// never moves any subtask through an estado. The returned package type lives
// in `src-tauri` (like `ReviewPackage`), so the wire `Result` is a
// `serde_json::Value` passthrough (the package serializes to JSON identically)
// rather than a duplicated struct. Same op-name dispatch rationale as the ops
// above — no MIN/MAX_PROTOCOL bump.
pub const OP_JIRA_REVIEW: &str = "jira_review";

// Jira (Slice 6a): import orchestration + discard lifecycle. `jira_import`
// resolves+fetches an issue, upserts its record, mints an analysis run, and
// spawns the analyst agent (the capability secret reaches the analyst via
// ENV only, never the wire `Result`). `jira_discard` tears an imported issue
// down: it refuses a dirty worktree unless forced, removes the worktree,
// revokes the run secret, deletes the record, and forgets subtask sidecars.
// Same op-name dispatch rationale as the ops above — no MIN/MAX_PROTOCOL bump.
pub const OP_JIRA_IMPORT: &str = "jira_import";
pub const OP_JIRA_DISCARD: &str = "jira_discard";

// ───────── event names

pub const EV_PROPOSTA_PENDENTE: &str = "proposta_pendente";
pub const EV_PROPOSTA_DECIDIDA: &str = "proposta_decidida";
/// Emitido pelo servidor depois de qualquer create/delete de task vinda
/// pela superfície IPC (CLI). A UI escuta e re-roda `list_tasks`.
pub const EV_TASKS_CHANGED: &str = "tasks_changed";
/// Emitido depois de qualquer create/delete/set_status de ideia via IPC.
pub const EV_IDEIAS_CHANGED: &str = "ideias_changed";
/// Emitido depois de qualquer mudança na memória de um projeto ou na
/// fila de sugestões pendentes (aprendizado ou reeval) via IPC. A UI
/// escuta para re-puxar a aba de Memória e o review da task.
pub const EV_MEMORY_CHANGED: &str = "memory_changed";

// ───────── empty args helper

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmptyArgs {}

// ───────── hello

pub mod hello {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub protocol: u32,
        pub client: String,
        pub token: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub protocol: u32,
        pub app: String,
    }
}

// ───────── list_tasks

pub mod list_tasks {
    use super::Task;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct Args {
        #[serde(default)]
        pub estado: Option<String>,
    }

    pub type Result = Vec<Task>;
}

// ───────── current_task

pub mod current_task {
    use super::{EmptyArgs, Task};

    pub type Args = EmptyArgs;
    pub type Result = Option<Task>;
}

// ───────── append_log

pub mod append_log {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub task_id: String,
        pub text: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub ok: bool,
    }
}

// ───────── propose

pub mod propose {
    use super::NewProposta;
    use serde::{Deserialize, Serialize};

    pub type Args = NewProposta;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub proposta_id: String,
    }
}

// ───────── await_decision

pub mod await_decision {
    use super::DecisaoRegistro;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub proposta_id: String,
        #[serde(default = "default_timeout_ms")]
        pub timeout_ms: u64,
    }

    fn default_timeout_ms() -> u64 {
        300_000 // 5 min, per DESIGN
    }

    /// Server reuses `DecisaoRegistro` as the success payload.
    pub type Result = DecisaoRegistro;
}

// ───────── done

pub mod done {
    use serde::{Deserialize, Serialize};

    /// `done` args. Backward compatible: `task_id` + `summary` stay
    /// positional/required, so an old CLI sending `{task_id, summary}`
    /// still deserializes into this struct (new fields are
    /// `#[serde(default)]` ⇒ `None`/absent) and a v3 app produces a
    /// `no_validation` package. The evidence payload lives in `proto`
    /// because it crosses the socket (CLI → app).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub task_id: String,
        /// Human summary — positional, unchanged. Backward compatible with
        /// the pre-evidence `{task_id, summary}` shape.
        pub summary: String,
        /// Optional validation evidence (PLAN §B.6). Absent ⇒ no_validation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub evidence: Option<Evidence>,
        /// Client-generated key forming the done-attempt identity (PLAN §C.9),
        /// mirroring `propose`'s `idempotency_key`. Wire arg, NOT inside
        /// `evidence`. Validated app-side (bounded len, restricted charset).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub idempotency_key: Option<String>,
    }

    /// Agent-reported evidence (PLAN §B.6 evidence.json schema). Caps are
    /// enforced in the CLI AND re-enforced app-side (PLAN §B.7, §C.10):
    /// ≤ 64 checks, ≤ 64 groups, label/path/question ≤ 1 KiB,
    /// log_excerpt ≤ 8 KiB, whole file ≤ 256 KiB.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct Evidence {
        /// Contract hash the agent validated against ("sha256:…"); compared
        /// to the live contract for drift (→ contract_changed, PLAN §C.11.d).
        #[serde(default)]
        pub contract_version: Option<String>,
        #[serde(default)]
        pub checks: Vec<EvidenceCheck>,
        #[serde(default)]
        pub groups: Vec<EvidenceGroup>,
        #[serde(default)]
        pub open_questions: Vec<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct EvidenceCheck {
        pub id: String,
        pub exit: i32,
        #[serde(default)]
        pub log_excerpt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub log_path: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct EvidenceGroup {
        pub label: String,
        #[serde(default)]
        pub files: Vec<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub ok: bool,
    }
}

// ───────── quality (PLAN §B.5)

pub mod quality {
    use serde::{Deserialize, Serialize};

    /// Resolve order app-side: explicit `project` → `TASKAI_PROJECT_ID` /
    /// `TASKAI_TASK_ID` env → app `active_project_id` (PLAN §B.5). The CLI
    /// passes whatever it resolved locally; the app does the final resolution.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct Args {
        #[serde(default)]
        pub task: Option<String>,
        #[serde(default)]
        pub project: Option<String>,
    }

    /// On resolution failure the app returns an explicit
    /// `ErrorBody::new("unknown_project", …)` (CLI exit 30). An **empty
    /// `checks` list** is reserved for "resolved project has no profile"
    /// (→ `no_validation`); do not conflate the two.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        /// Current contract hash ("sha256:…"). Echoed back by the agent in
        /// `done` evidence so drift surfaces as `contract_changed`.
        pub contract_version: String,
        pub checks: Vec<Check>,
    }

    /// Wire view of a `QualityCheck` (the app's config struct stays in
    /// src-tauri; this mirrors only the fields the agent needs). Same
    /// app-vs-wire split as `ProjectInfo` vs `Project`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Check {
        pub id: String,
        pub name: String,
        pub cmd: String,
        #[serde(default)]
        pub required: bool,
        #[serde(default)]
        pub required_if_changed: Vec<String>,
    }
}

// ───────── review_decision (PLAN §E.16)

pub mod review_decision {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub task_id: String,
        pub verdict: Verdict,
        /// Note appended to the `[revisão] …` log line for both outcomes.
        #[serde(default)]
        pub note: Option<String>,
    }

    /// Reviewer outcome. `aprovado` → feito; `pedir_alteracoes` → fazendo
    /// (PLAN §E.16). PT canonical on the wire, matching `Decisao`/`Estado`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Verdict {
        Aprovado,
        PedirAlteracoes,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub ok: bool,
    }
}

// ───────── jira_materialize (Slice 2)

pub mod jira_materialize {
    use serde::{Deserialize, Serialize};

    /// One subtask of an analysis run. Mapped server-side onto a
    /// `NewProposta` (`title → title`, `body → repro`).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Subtask {
        pub title: String,
        pub body: String,
    }

    #[derive(Clone, Serialize, Deserialize)]
    pub struct Args {
        pub analysis_run_id: String,
        /// Capability secret authorizing this run. Transits the
        /// authenticated local socket only; NEVER logged. Sourced from
        /// `$CADENZA_RUN_SECRET` (or STDIN) by the CLI, never from argv.
        pub run_secret: String,
        pub subtasks: Vec<Subtask>,
    }

    // Manual `Debug` redacts `run_secret` so the capability secret can never
    // leak via a future `{:?}`/tracing of the args (defense in depth — the
    // current code already avoids logging args).
    impl std::fmt::Debug for Args {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Args")
                .field("analysis_run_id", &self.analysis_run_id)
                .field("run_secret", &"<redacted>")
                .field("subtasks", &self.subtasks)
                .finish()
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct MaterializedTask {
        pub proposta_id: String,
        pub idempotency_key: String,
        pub subtask_index: u32,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub jira_site: String,
        pub jira_issue_id: String,
        /// One per subtask, in submission order (dedup-stable).
        pub created: Vec<MaterializedTask>,
    }
}

// ───────── jira_test_connection / jira_fetch_issue / jira_list_assigned (Slice 3)

pub mod jira_test_connection {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct Args {}

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub account_id: String,
        pub display_name: String,
    }
}

pub mod jira_fetch_issue {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub key: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub jira_issue_id: String,
        pub jira_key: String,
        pub summary: String,
        pub description_markdown: String,
        pub raw_adf: serde_json::Value,
    }
}

pub mod jira_list_assigned {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct Args {}

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Issue {
        pub key: String,
        pub id: String,
        pub summary: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub issues: Vec<Issue>,
        pub partial: bool,
    }
}

// ───────── jira_review (Slice 5: aggregate issue-owned review)

pub mod jira_review {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub jira_site: String,
        pub jira_issue_id: String,
    }

    /// The built+persisted aggregate review package. Its concrete type
    /// (`IssueReviewPackage`) lives in `src-tauri`; the wire surface is a
    /// JSON passthrough so the proto crate does not duplicate the struct.
    pub type Result = serde_json::Value;
}

// ───────── jira_import (Slice 6a: import orchestration)

pub mod jira_import {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        /// Jira issue key, e.g. "PROJ-123". Trimmed; empty -> jira_config.
        pub issue_ref: String,
        /// Target project to bind. Validated against config.projects.
        pub project_id: String,
        /// Analyst agent kind. Wire string parsed into AgenteKind.
        pub analyst_kind: String,
    }

    /// Discriminated result: a fresh import (record+run created, analyst
    /// spawned) vs. an existing active issue reopened without re-fetch/spawn.
    ///
    /// NOTE: this carries NO capability secret. The minted secret reaches the
    /// analyst process via ENV only (`CADENZA_RUN_SECRET`); it never appears on
    /// the wire `Result`, in argv, or in any log line.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "outcome", rename_all = "snake_case")]
    pub enum Result {
        /// New import: issue fetched, record upserted, run minted, analyst spawned.
        Imported {
            jira_site: String,
            jira_issue_id: String,
            jira_key: String,
            summary: String,
            project_id: String,
            analysis_run_id: String,
            session_id: String,
        },
        /// Idempotent reopen: record already had active work; nothing re-fetched/spawned.
        ExistingActive {
            jira_site: String,
            jira_issue_id: String,
            jira_key: String,
            project_id: Option<String>,
            analysis_run_id: Option<String>,
        },
    }
}

// ───────── jira_discard (Slice 6a: discard lifecycle)

pub mod jira_discard {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub jira_site: String,
        pub jira_issue_id: String,
        /// Override the dirty-worktree refusal. Default false.
        #[serde(default)]
        pub force: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub jira_site: String,
        pub jira_issue_id: String,
        /// True if a git worktree was physically removed.
        pub worktree_removed: bool,
        /// Subtask task_worktrees sidecar entries forgotten.
        pub forgotten_task_worktrees: u32,
    }
}

// ───────── bye

pub mod bye {
    use super::EmptyArgs;
    use serde::{Deserialize, Serialize};

    pub type Args = EmptyArgs;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub ok: bool,
    }
}

// ───────── create_task (protocolo v2)

pub mod create_task {
    use serde::{Deserialize, Serialize};

    /// Cria uma task em `a_fazer`, já vinculada ao projeto. Se `id`
    /// não vier o servidor mintava um (`T-<short>`). `from_ideia` é o
    /// id da ideia de origem, opcional — usado para marcar a ideia
    /// como `destrinchada` quando o agente terminar.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        #[serde(default)]
        pub id: Option<String>,
        pub titulo: String,
        #[serde(default)]
        pub body: String,
        pub project_id: String,
        #[serde(default)]
        pub from_ideia: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub task_id: String,
    }
}

// ───────── list_ideias

pub mod list_ideias {
    use super::{EmptyArgs, Ideia};

    pub type Args = EmptyArgs;
    pub type Result = Vec<Ideia>;
}

// ───────── read_ideia

pub mod read_ideia {
    use super::Ideia;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub id: String,
    }

    pub type Result = Option<Ideia>;
}

// ───────── create_ideia

pub mod create_ideia {
    use super::Ideia;
    use serde::{Deserialize, Serialize};

    /// Argumentos para criar uma ideia. O servidor mintava `id` e
    /// `created_at_ms` se ausentes; `status` defaulta para `pendente`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        #[serde(default)]
        pub id: Option<String>,
        pub titulo: String,
        #[serde(default)]
        pub body: String,
        pub project_id: String,
    }

    pub type Result = Ideia;
}

// ───────── delete_ideia

pub mod delete_ideia {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub id: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub ok: bool,
    }
}

// ───────── set_task_worktree

pub mod set_task_worktree {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub task_id: String,
        /// Absolute path to the git worktree. `None` clears the association.
        #[serde(default)]
        pub worktree_path: Option<String>,
        /// Git branch name. `None` clears the association.
        #[serde(default)]
        pub branch: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub ok: bool,
    }
}

// ───────── update_body

pub mod update_body {
    use serde::{Deserialize, Serialize};

    /// Rewrite a task's markdown body. Used by `cadenza-cli plan` so a
    /// planning agent can persist the refined plan. When `append_plan`
    /// is true (default) the server keeps the existing body and appends
    /// (or replaces) a `## Plano` section; when false it overwrites the
    /// whole body.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub task_id: String,
        pub body: String,
        #[serde(default = "default_append_plan")]
        pub append_plan: bool,
    }

    fn default_append_plan() -> bool {
        true
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub ok: bool,
    }
}

// ───────── read_task

pub mod read_task {
    use super::Task;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub task_id: String,
    }

    /// A single task. A missing id is an error (`task_not_found`), not a
    /// `None` — so the result is `Task`, not `Option<Task>`.
    pub type Result = Task;
}

// ───────── list_projects

pub mod list_projects {
    use super::{EmptyArgs, ProjectInfo};

    pub type Args = EmptyArgs;
    pub type Result = Vec<ProjectInfo>;
}

// ───────── list_memory

pub mod list_memory {
    use super::MemoryItem;
    use serde::{Deserialize, Serialize};

    /// O agente lê a memória oficial do projeto em que está rodando. O
    /// CLI resolve `project_id` de `$TASKAI_PROJECT_ID` quando ausente.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub project_id: String,
    }

    pub type Result = Vec<MemoryItem>;
}

// ───────── suggest_learning

pub mod suggest_learning {
    use serde::{Deserialize, Serialize};

    /// Aprendizado proposto pelo agente de execução ao finalizar. Fica
    /// pendente até o usuário promovê-lo no review da task. O servidor
    /// minta `id` e `criado_em`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub project_id: String,
        pub texto: String,
        /// Task de origem — o CLI resolve de `$TASKAI_TASK_ID` quando
        /// ausente para que o review da task correta exiba o aprendizado.
        #[serde(default)]
        pub origem_task: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub suggestion_id: String,
    }
}

// ───────── revise_memory

pub mod revise_memory {
    use super::SuggestionKind;
    use serde::{Deserialize, Serialize};

    /// Operação de reavaliação proposta pelo agente de reeval. `kind`
    /// deve ser uma variante de reeval (não `Aprendizado`); o servidor
    /// rejeita `Aprendizado` aqui. Minta `id` e `criado_em`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub project_id: String,
        pub kind: SuggestionKind,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub suggestion_id: String,
    }
}

// ───────── set_ideia_status

pub mod set_ideia_status {
    use super::IdeiaStatus;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub id: String,
        pub status: IdeiaStatus,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub ok: bool,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_args_deserializes_legacy_summary_only() {
        let v: done::Args = serde_json::from_str(r#"{"task_id":"T-1","summary":"hi"}"#).unwrap();
        assert_eq!(v.task_id, "T-1");
        assert_eq!(v.summary, "hi");
        assert!(v.evidence.is_none());
        assert!(v.idempotency_key.is_none());
    }

    #[test]
    fn done_args_with_evidence_roundtrips() {
        let args = done::Args {
            task_id: "T-1".into(),
            summary: "done".into(),
            evidence: Some(done::Evidence {
                contract_version: Some("sha256:00".into()),
                checks: vec![done::EvidenceCheck {
                    id: "clippy".into(),
                    exit: 0,
                    log_excerpt: "ok".into(),
                    log_path: None,
                }],
                groups: vec![done::EvidenceGroup {
                    label: "core".into(),
                    files: vec!["src/lib.rs".into()],
                }],
                open_questions: vec!["why?".into()],
            }),
            idempotency_key: Some("k1".into()),
        };
        let json = serde_json::to_string(&args).unwrap();
        let back: done::Args = serde_json::from_str(&json).unwrap();
        assert_eq!(back.idempotency_key.as_deref(), Some("k1"));
        assert_eq!(back.evidence.unwrap().checks[0].id, "clippy");
    }

    #[test]
    fn quality_result_roundtrips() {
        let r = quality::Result {
            contract_version: "sha256:ab".into(),
            checks: vec![quality::Check {
                id: "fmt".into(),
                name: "Fmt".into(),
                cmd: "cargo fmt".into(),
                required: true,
                required_if_changed: vec!["**/*.rs".into()],
            }],
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: quality::Result = serde_json::from_str(&json).unwrap();
        assert_eq!(back.contract_version, "sha256:ab");
        assert_eq!(back.checks[0].id, "fmt");
    }

    #[test]
    fn review_decision_verdict_wire_forms() {
        assert_eq!(
            serde_json::to_string(&review_decision::Verdict::Aprovado).unwrap(),
            "\"aprovado\""
        );
        assert_eq!(
            serde_json::to_string(&review_decision::Verdict::PedirAlteracoes).unwrap(),
            "\"pedir_alteracoes\""
        );
        let a: review_decision::Args =
            serde_json::from_str(r#"{"task_id":"T-1","verdict":"pedir_alteracoes"}"#).unwrap();
        assert_eq!(a.verdict, review_decision::Verdict::PedirAlteracoes);
        assert!(a.note.is_none());
    }
}
