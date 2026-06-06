//! Cadenza IPC wire protocol — NDJSON framed over the local socket.
//!
//! See DESIGN-desktop-v2.md § "Protocolo IPC". This crate carries the
//! types both `src-tauri` (server) and `cadenza-cli` (client) need to
//! agree on, so it is **the source of truth** for the wire format.

pub mod ideia;
pub mod memory;
pub mod ops;
pub mod project;
pub mod task;
pub mod triage;
pub mod wire;

pub use ideia::{Ideia, IdeiaStatus, NewIdeia};
pub use memory::{MemoryItem, MemorySuggestion, ProjectMemory, SuggestionKind};
pub use project::ProjectInfo;
pub use task::{Estado, Task};
pub use triage::{Decisao, DecisaoRegistro, NewProposta, Proposta};
pub use wire::{ErrorBody, Event, Request, Response};

/// Minimum protocol version the current build supports. Incremented on a
/// breaking wire change; older versions outside `[MIN_PROTOCOL,
/// MAX_PROTOCOL]` get a `protocol_too_old` / `protocol_too_new` error
/// during the `hello` handshake.
///
/// # Negotiation / downgrade contract (PLAN §B.6)
///
/// The CLI sends `hello { protocol: MAX_PROTOCOL }`. The app validates
/// against its own `[MIN_PROTOCOL, MAX_PROTOCOL]`: a client `< MIN` gets
/// `protocol_too_old`, `> MAX` gets `protocol_too_new` (both → CLI exit
/// 12). The `hello` reply echoes the **app's** `MAX_PROTOCOL`, so the
/// effective negotiated version is `min(client MAX, app MAX)` and is
/// observable to the CLI via `hello::Result.protocol`.
///
/// Dispatch keys on the **op-name string**, not the negotiated number, so
/// most ops can be added without a bump. The review ops DO bump because
/// they change negotiated *capability*: evidence on `done` must not be
/// silently dropped by an older app. Concretely:
///
/// - **`--evidence` against a `< 3` app:** the CLI fails fast with
///   `protocol_too_old` (exit 12) unless `--legacy-done` is passed, in
///   which case it sends a summary-only `done` and warns. Evidence is
///   never silently dropped.
/// - **No `--evidence`:** positional `done` is sent unchanged and works
///   against any app (v1/v2/v3) — backward compatible.
/// - **Old CLI (≤ v2) against a v3 app:** sends `{task_id, summary}`; the
///   v3 `done::Args` deserializes it (new fields `#[serde(default)]` ⇒
///   `None`), producing a `no_validation` package. No error.
pub const MIN_PROTOCOL: u32 = 1;
/// Was 2: + `quality`, evidence-on-`done`, `review_decision` (PLAN §19).
/// `MIN_PROTOCOL` stays 1 (no wire frame removed; positional `done` still
/// works for v1/v2 clients). `WIRE_VERSION` stays 1 (frame shape unchanged).
pub const MAX_PROTOCOL: u32 = 3;

/// Wire envelope version. Bumped on a breaking change to the outer
/// frame shape (`{v, id, op, args}` / `{v, id, ok, ...}`), separate from
/// `MIN_PROTOCOL`/`MAX_PROTOCOL`.
pub const WIRE_VERSION: u32 = 1;
