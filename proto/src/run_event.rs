//! Run timeline — append-only audit event log (wire types).
//!
//! Um `RunEvent` é um registro **append-only** das transições relevantes
//! de um run de agente / task: agente iniciado, sessão PTY encerrada,
//! `done` enviado, revisão decidida e proposta decidida. É a **fundação**
//! sobre a qual se constroem telemetria de custo, analytics, rollback e
//! comentários→resume — por isso o schema precisa sobreviver a leitura por
//! binários mais antigos/novos.
//!
//! Como `Ideia`/`MemorySuggestion`, é um tipo novo (não passa pelo formato
//! Node.js legacy). Regras de compatibilidade para um log durável:
//! - `schema_version` explícito (com `#[serde(default)]`) deixa leitores
//!   futuros ramificarem; registros escritos antes do campo leem como `0`.
//! - Toda adição de campo é aditiva e `#[serde(default)]`; nunca renomear
//!   nem remover um campo já serializado.
//! - `RunEventKind` tem um catch-all `#[serde(other)] Desconhecido` — um
//!   leitor antigo replayando um log mais novo decodifica tipos de evento
//!   desconhecidos aqui em vez de falhar (é o único ponto do código que
//!   precisa de `serde(other)`).

use serde::{Deserialize, Serialize};

/// Versão de schema do registro corrente. Incrementada quando a forma do
/// envelope/payload muda de forma que um leitor queira ramificar.
pub const RUN_EVENT_SCHEMA_VERSION: u32 = 1;

/// Observação de uso (tokens) de um run, MEDIDA da telemetria nativa do
/// agente — nunca estimada (feature #1). Custo em $ é deixado de fora de
/// propósito: depende de uma tabela de preços por modelo que mudaria sozinha;
/// expomos contagens medidas e dizemos "indisponível" quando não há fonte.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageObservation {
    #[serde(default)]
    pub schema_version: u32,
    /// De onde os números vieram, ex.: `claude_session_jsonl`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
}

impl UsageObservation {
    pub fn new(source: String) -> Self {
        Self {
            schema_version: RUN_EVENT_SCHEMA_VERSION,
            source,
            ..Default::default()
        }
    }

    /// Total de tokens NOVOS do run: entrada não-cacheada + saída + escrita de
    /// cache. **Exclui `cache_read_tokens`** de propósito — leituras de cache
    /// são o contexto re-enviado a cada turno (cresce a cada turno), então
    /// somá-las contaria o mesmo contexto N vezes e inflaria o "total".
    /// `cache_read_tokens` é o tamanho do contexto cacheado (snapshot final),
    /// reportado à parte.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_creation_tokens)
    }
}

/// Um registro append-only do log de auditoria de runs.
///
/// `ts_ms` é epoch-ms cunhado no servidor
/// (`chrono::Utc::now().timestamp_millis()`). `task_id` é opcional porque
/// nem todo evento é escopado a uma task conhecida no momento da emissão.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvent {
    pub id: String,
    /// Versão de schema explícita. `#[serde(default)]` para que registros
    /// escritos antes do campo existir leiam como `0`.
    #[serde(default)]
    pub schema_version: u32,
    pub ts_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub kind: RunEventKind,
}

impl RunEvent {
    /// Constrói um evento com a versão de schema corrente. O chamador
    /// fornece `id` e `ts_ms` (cunhados no servidor) para manter este tipo
    /// livre de dependências de relógio/uuid.
    pub fn new(id: String, ts_ms: i64, task_id: Option<String>, kind: RunEventKind) -> Self {
        Self {
            id,
            schema_version: RUN_EVENT_SCHEMA_VERSION,
            ts_ms,
            task_id,
            kind,
        }
    }

    /// Tag estável do tipo de evento, para colunas indexadas / filtros sem
    /// reabrir o payload. Casa com o token `tipo` serializado.
    pub fn kind_tag(&self) -> &'static str {
        self.kind.tag()
    }
}

/// O tipo de um evento de run. Enum internamente-tagueado (`tipo`),
/// espelhando `SuggestionKind` em memory.rs. Cada variante carrega só os
/// campos relevantes àquela transição.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tipo", rename_all = "snake_case")]
pub enum RunEventKind {
    /// Um agente foi iniciado para a task (modo Execute/Plan).
    AgenteIniciado {
        agente: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        modo: Option<String>,
        #[serde(default)]
        resumido: bool,
        #[serde(default)]
        session_id: Option<String>,
    },
    /// A sessão PTY do run foi encerrada. `motivo`: `eof` | `erro` |
    /// `encerrada` (kill explícito).
    SessaoEncerrada {
        #[serde(default)]
        session_id: Option<String>,
        motivo: String,
    },
    /// `done` foi enviado (task → aguardando_revisao).
    DoneEnviado {
        #[serde(default)]
        resumo: Option<String>,
        #[serde(default)]
        com_evidencia: bool,
    },
    /// Uma revisão foi decidida pelo humano.
    RevisaoDecidida {
        verdict: String,
        #[serde(default)]
        nota: Option<String>,
        #[serde(default)]
        novo_estado: Option<String>,
    },
    /// Uma proposta de triagem foi decidida pelo humano.
    PropostaDecidida {
        proposta_id: String,
        decisao: String,
    },
    /// Um checkpoint do workspace foi criado antes de um run (feature #6),
    /// para permitir "reverter este run". `dir` é o workspace onde o snapshot
    /// foi tirado; `commit` é o sha imutável usado na restauração.
    CheckpointCriado {
        git_ref: String,
        commit: String,
        dir: String,
    },
    /// O workspace foi revertido a um checkpoint (feature #6). `safety_commit`
    /// é o snapshot do estado pré-revert (a própria reversão é reversível).
    RunRevertido {
        commit: String,
        #[serde(default)]
        safety_commit: Option<String>,
    },
    /// Uso (tokens) medido de um run (feature #1). Cumulativo por conversa no
    /// momento da emissão (tipicamente no fim da sessão). `conversation_id`
    /// permite agregar distinguindo "mesma conversa re-observada" (manter o
    /// último) de "conversas distintas na mesma task" (somar).
    UsoObservado {
        usage: UsageObservation,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation_id: Option<String>,
    },
    /// Catch-all de forward-compat: um leitor antigo decodifica um tipo de
    /// evento futuro aqui em vez de falhar (o payload é descartado).
    #[serde(other)]
    Desconhecido,
}

impl RunEventKind {
    /// Tag estável (casa com o token `tipo` serializado).
    pub fn tag(&self) -> &'static str {
        match self {
            RunEventKind::AgenteIniciado { .. } => "agente_iniciado",
            RunEventKind::SessaoEncerrada { .. } => "sessao_encerrada",
            RunEventKind::DoneEnviado { .. } => "done_enviado",
            RunEventKind::RevisaoDecidida { .. } => "revisao_decidida",
            RunEventKind::PropostaDecidida { .. } => "proposta_decidida",
            RunEventKind::CheckpointCriado { .. } => "checkpoint_criado",
            RunEventKind::RunRevertido { .. } => "run_revertido",
            RunEventKind::UsoObservado { .. } => "uso_observado",
            RunEventKind::Desconhecido => "desconhecido",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agente_iniciado_round_trips_with_tipo_tag() {
        let ev = RunEvent::new(
            "E-1".into(),
            1_700_000_000_000,
            Some("T-42".into()),
            RunEventKind::AgenteIniciado {
                agente: "claude_code".into(),
                model: Some("claude-opus".into()),
                modo: Some("execute".into()),
                resumido: false,
                session_id: Some("S-abc".into()),
            },
        );
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"tipo\":\"agente_iniciado\""));
        assert!(json.contains("\"schema_version\":1"));
        let back: RunEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn all_kinds_round_trip() {
        let kinds = [
            RunEventKind::SessaoEncerrada {
                session_id: Some("S-1".into()),
                motivo: "eof".into(),
            },
            RunEventKind::DoneEnviado {
                resumo: Some("merged".into()),
                com_evidencia: true,
            },
            RunEventKind::RevisaoDecidida {
                verdict: "aprovado".into(),
                nota: None,
                novo_estado: Some("feito".into()),
            },
            RunEventKind::PropostaDecidida {
                proposta_id: "P-9".into(),
                decisao: "aceita".into(),
            },
        ];
        for k in kinds {
            let json = serde_json::to_string(&k).unwrap();
            let back: RunEventKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, k);
        }
    }

    #[test]
    fn unknown_future_kind_decodes_to_desconhecido() {
        // Um leitor antigo replayando um log mais novo não deve falhar.
        let json = r#"{"tipo":"algo_do_futuro","campo_novo":123}"#;
        let back: RunEventKind = serde_json::from_str(json).unwrap();
        assert_eq!(back, RunEventKind::Desconhecido);
    }

    #[test]
    fn record_without_schema_version_defaults_to_zero() {
        let json = r#"{"id":"E-2","ts_ms":1,"kind":{"tipo":"done_enviado"}}"#;
        let back: RunEvent = serde_json::from_str(json).unwrap();
        assert_eq!(back.schema_version, 0);
        assert_eq!(back.task_id, None);
    }
}
