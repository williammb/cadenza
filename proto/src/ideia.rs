//! Ideia wire types — entidade da coluna "Inbox" do board.
//!
//! Uma `Ideia` é uma anotação solta esperando para ser destrinchada em
//! tasks por um agente. Vive em `~/.cadenza/inbox/<id>.json` na backend
//! `Files` e em tabela própria nas demais. Diferentemente das tasks,
//! ideias **não** existem no formato Node.js legacy — então o schema
//! pode evoluir sem restrição de compatibilidade.
//!
//! `status` segue o padrão PT canônico do resto da wire (ver
//! `task::Estado`, `triage::Decisao`). Valores: `pendente`,
//! `destrinchada`, `arquivada`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdeiaStatus {
    Pendente,
    Destrinchada,
    Arquivada,
}

impl IdeiaStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            IdeiaStatus::Pendente => "pendente",
            IdeiaStatus::Destrinchada => "destrinchada",
            IdeiaStatus::Arquivada => "arquivada",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pendente" => Some(IdeiaStatus::Pendente),
            "destrinchada" => Some(IdeiaStatus::Destrinchada),
            "arquivada" => Some(IdeiaStatus::Arquivada),
            _ => None,
        }
    }
}

impl Default for IdeiaStatus {
    fn default() -> Self {
        IdeiaStatus::Pendente
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ideia {
    pub id: String,
    pub titulo: String,
    #[serde(default)]
    pub body: String,
    pub project_id: String,
    #[serde(default)]
    pub status: IdeiaStatus,
    pub created_at_ms: i64,
}

/// Inputs para criação via CLI/IPC. O servidor mintava o `id` e
/// `created_at_ms` quando ausentes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewIdeia {
    pub titulo: String,
    #[serde(default)]
    pub body: String,
    pub project_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips_pt_canonical() {
        for &s in &[
            IdeiaStatus::Pendente,
            IdeiaStatus::Destrinchada,
            IdeiaStatus::Arquivada,
        ] {
            assert_eq!(IdeiaStatus::parse(s.as_str()), Some(s));
        }
    }

    #[test]
    fn status_serializes_pt_canonical() {
        let json = serde_json::to_string(&IdeiaStatus::Destrinchada).unwrap();
        assert_eq!(json, "\"destrinchada\"");
    }

    #[test]
    fn status_default_is_pendente() {
        assert_eq!(IdeiaStatus::default(), IdeiaStatus::Pendente);
    }

    #[test]
    fn status_rejects_unknown() {
        assert_eq!(IdeiaStatus::parse("WIP"), None);
    }
}
