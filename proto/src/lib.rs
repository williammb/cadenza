//! Cadenza IPC wire protocol — NDJSON framed over the local socket.
//!
//! See DESIGN-desktop-v2.md § "Protocolo IPC". This crate carries the
//! types both `src-tauri` (server) and `cadenza-cli` (client) need to
//! agree on, so it is **the source of truth** for the wire format.

pub mod ideia;
pub mod ops;
pub mod project;
pub mod task;
pub mod triage;
pub mod wire;

pub use ideia::{Ideia, IdeiaStatus, NewIdeia};
pub use project::ProjectInfo;
pub use task::{Estado, Task};
pub use triage::{Decisao, DecisaoRegistro, NewProposta, Proposta};
pub use wire::{ErrorBody, Event, Request, Response};

/// Minimum protocol version the current build supports. Incremented on a
/// breaking wire change; older versions outside `[MIN_PROTOCOL,
/// MAX_PROTOCOL]` get a `protocol_too_old` / `protocol_too_new` error
/// during the `hello` handshake.
pub const MIN_PROTOCOL: u32 = 1;
pub const MAX_PROTOCOL: u32 = 2;

/// Wire envelope version. Bumped on a breaking change to the outer
/// frame shape (`{v, id, op, args}` / `{v, id, ok, ...}`), separate from
/// `MIN_PROTOCOL`/`MAX_PROTOCOL`.
pub const WIRE_VERSION: u32 = 1;
