//! Task / Estado wire types.
//!
//! Values stay in PT canonical on disk and on the wire — see
//! DESIGN-desktop-v2.md § "Hard constraints".

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Estado {
    AFazer,
    Fazendo,
    AguardandoRevisao,
    Feito,
}

impl Estado {
    pub fn as_str(&self) -> &'static str {
        match self {
            Estado::AFazer => "a_fazer",
            Estado::Fazendo => "fazendo",
            Estado::AguardandoRevisao => "aguardando_revisao",
            Estado::Feito => "feito",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "a_fazer" => Some(Estado::AFazer),
            "fazendo" => Some(Estado::Fazendo),
            "aguardando_revisao" => Some(Estado::AguardandoRevisao),
            "feito" => Some(Estado::Feito),
            _ => None,
        }
    }

    /// Whether a task in this state satisfies a downstream blocker — i.e. it
    /// has reached review or completion, so tasks blocked by it may start.
    /// The frontend mirrors this set in `BLOCKER_SATISFIED_ESTADOS`
    /// (`ui/app.js`); keep the two in sync.
    pub fn satisfies_blocker(&self) -> bool {
        matches!(self, Estado::AguardandoRevisao | Estado::Feito)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub titulo: String,
    pub estado: Estado,
    pub responsavel: String,

    /// Markdown body — comes from the file content after the YAML frontmatter.
    /// `store.rs` writes YAML via `TaskMeta` (no body field) so the YAML
    /// round-trip stays clean; for IPC over JSON we keep body as a
    /// real serde field so the frontend can read/send it.
    #[serde(default)]
    pub body: String,

    /// Absolute path to the git worktree for this task. Stored in the
    /// sidecar `~/.cadenza/task-worktrees.json` (not in the YAML
    /// frontmatter — that format is frozen for Node.js compat). `None`
    /// when no worktree is associated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,

    /// Git branch associated with this task. Stored alongside
    /// `worktree_path` in the sidecar. `None` when not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,

    /// Task ids that must be at least `aguardando_revisao` before this
    /// task can be started for execution. Stored in the Cadenza sidecar
    /// `task-blockers.json`, not in legacy task frontmatter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estado_round_trips_pt_canonical() {
        for &e in &[
            Estado::AFazer,
            Estado::Fazendo,
            Estado::AguardandoRevisao,
            Estado::Feito,
        ] {
            assert_eq!(Estado::parse(e.as_str()), Some(e));
        }
    }

    #[test]
    fn estado_serializes_pt_canonical() {
        let json = serde_json::to_string(&Estado::AguardandoRevisao).unwrap();
        assert_eq!(json, "\"aguardando_revisao\"");
    }

    #[test]
    fn estado_rejects_unknown() {
        assert_eq!(Estado::parse("WIP"), None);
    }

    #[test]
    fn task_defaults_blocked_by_when_absent() {
        let task: Task = serde_json::from_str(
            r#"{
                "id": "T-1",
                "titulo": "Example",
                "estado": "a_fazer",
                "responsavel": "humano",
                "body": "body"
            }"#,
        )
        .unwrap();

        assert!(task.blocked_by.is_empty());
    }
}
