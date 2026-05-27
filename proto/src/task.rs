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
}
