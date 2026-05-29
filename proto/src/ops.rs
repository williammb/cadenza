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

use crate::{DecisaoRegistro, Ideia, IdeiaStatus, NewProposta, Task};

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

// ───────── event names

pub const EV_PROPOSTA_PENDENTE: &str = "proposta_pendente";
pub const EV_PROPOSTA_DECIDIDA: &str = "proposta_decidida";
/// Emitido pelo servidor depois de qualquer create/delete de task vinda
/// pela superfície IPC (CLI). A UI escuta e re-roda `list_tasks`.
pub const EV_TASKS_CHANGED: &str = "tasks_changed";
/// Emitido depois de qualquer create/delete/set_status de ideia via IPC.
pub const EV_IDEIAS_CHANGED: &str = "ideias_changed";

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

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Args {
        pub task_id: String,
        pub summary: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Result {
        pub ok: bool,
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
