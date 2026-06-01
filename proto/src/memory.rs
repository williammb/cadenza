//! Memória compartilhada por projeto — wire types.
//!
//! A memória de um projeto é uma **lista de itens estruturados**
//! (`MemoryItem`) que representam fatos, decisões e convenções que os
//! agentes devem conhecer sobre aquele projeto. A fonte da verdade vive
//! **só no store do Cadenza** (como tasks/ideias), nunca no repo do
//! projeto. Nada gerado por agente entra na memória oficial sem
//! aprovação explícita do usuário — daí a entidade separada
//! `MemorySuggestion`, que fica pendente até a curadoria.
//!
//! Como `Ideia`, esses tipos são novos: não passam pelo formato Node.js
//! legacy, então o schema pode evoluir sem restrição de compatibilidade.

use serde::{Deserialize, Serialize};

/// Um item da memória oficial do projeto. `origem_task` aponta para a
/// task que originou o aprendizado promovido (quando houver); itens
/// criados manualmente pelo usuário ou via op `Nova` têm `origem_task`
/// vazio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: String,
    pub texto: String,
    #[serde(default)]
    pub origem_task: Option<String>,
    pub criado_em: i64,
}

/// A memória oficial de um projeto: a lista de itens curados.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMemory {
    pub project_id: String,
    #[serde(default)]
    pub items: Vec<MemoryItem>,
}

/// Uma sugestão pendente, aguardando aprovação do usuário. Cobre tanto
/// os **aprendizados** propostos pelo agente de execução (aparecem no
/// review da task) quanto as **operações de reavaliação** propostas pelo
/// agente de reeval (aparecem na aba de Memória). O discriminador é
/// `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySuggestion {
    pub id: String,
    pub project_id: String,
    pub criado_em: i64,
    pub kind: SuggestionKind,
}

/// O tipo de uma sugestão. `Aprendizado` é proposto pelo agente de
/// execução ao finalizar uma task; as demais variantes (operações de
/// reavaliação) são propostas pelo agente de reeval. `Contradicao` é
/// **informativa** — aprová-la não muda nada por si só; o usuário resolve
/// editando a memória manualmente.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tipo", rename_all = "snake_case")]
pub enum SuggestionKind {
    /// Aprendizado reaproveitável proposto pelo agente de execução.
    Aprendizado {
        texto: String,
        #[serde(default)]
        origem_task: Option<String>,
    },
    /// Remover um item obsoleto.
    Remover { target_id: String },
    /// Reescrever um item confuso.
    Reescrever {
        target_id: String,
        novo_texto: String,
    },
    /// Mesclar duplicatas em um único item novo (remove os `target_ids`).
    Mesclar {
        target_ids: Vec<String>,
        texto_mesclado: String,
    },
    /// Propor um item totalmente novo.
    Nova { texto: String },
    /// Apontar uma contradição entre itens — informativa, não aplica.
    Contradicao {
        target_ids: Vec<String>,
        nota: String,
    },
}

impl SuggestionKind {
    /// `true` para `Aprendizado` (vai para o review da task), `false`
    /// para as operações de reavaliação (vão para a aba de Memória).
    pub fn is_learning(&self) -> bool {
        matches!(self, SuggestionKind::Aprendizado { .. })
    }

    /// `true` quando aprovar a sugestão não altera a memória por si só
    /// (`Contradicao` é informativa).
    pub fn is_informational(&self) -> bool {
        matches!(self, SuggestionKind::Contradicao { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learning_round_trips_with_tipo_tag() {
        let s = SuggestionKind::Aprendizado {
            texto: "usar o pipeline Validator".into(),
            origem_task: Some("T-42".into()),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"tipo\":\"aprendizado\""));
        let back: SuggestionKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn reeval_ops_round_trip() {
        let ops = [
            SuggestionKind::Remover {
                target_id: "M-1".into(),
            },
            SuggestionKind::Reescrever {
                target_id: "M-2".into(),
                novo_texto: "texto claro".into(),
            },
            SuggestionKind::Mesclar {
                target_ids: vec!["M-3".into(), "M-4".into()],
                texto_mesclado: "fundido".into(),
            },
            SuggestionKind::Nova {
                texto: "convenção nova".into(),
            },
            SuggestionKind::Contradicao {
                target_ids: vec!["M-5".into(), "M-6".into()],
                nota: "um diz X, outro diz Y".into(),
            },
        ];
        for op in ops {
            let json = serde_json::to_string(&op).unwrap();
            let back: SuggestionKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, op);
        }
    }

    #[test]
    fn is_learning_and_informational() {
        assert!(SuggestionKind::Aprendizado {
            texto: "x".into(),
            origem_task: None,
        }
        .is_learning());
        assert!(SuggestionKind::Contradicao {
            target_ids: vec![],
            nota: "n".into(),
        }
        .is_informational());
        assert!(!SuggestionKind::Nova { texto: "x".into() }.is_learning());
    }
}
