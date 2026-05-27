//! Triage wire types — Proposta, NewProposta, Decisao, DecisaoRegistro.
//!
//! Shared by `src-tauri/src/triage.rs` (persistence) and `cadenza-cli`
//! (wire). On disk and on the wire these stay in PT canonical (see
//! DESIGN-desktop-v2.md § "Hard constraints").

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decisao {
    Aceita,
    Rejeitada,
    Mesclada,
}

/// Inputs to `propose`. The client mints `idempotency_key`; the app
/// mints the `proposta_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewProposta {
    pub idempotency_key: String,
    #[serde(default)]
    pub parent: Option<String>,
    pub title: String,
    pub repro: String,
    pub file: String,
    pub what_failed: String,
    pub action: String,
}

/// Persisted proposal (`<proposta_id>.proposta.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposta {
    pub proposta_id: String,
    pub idempotency_key: String,
    #[serde(default)]
    pub parent: Option<String>,
    pub title: String,
    pub repro: String,
    pub file: String,
    pub what_failed: String,
    pub action: String,
    pub created_at_ms: i64,
}

/// Persisted decision (`<proposta_id>.decisao.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisaoRegistro {
    pub proposta_id: String,
    pub decisao: Decisao,
    #[serde(default)]
    pub task_id: Option<String>,
    pub autor: String,
    pub decided_at_ms: i64,
}
