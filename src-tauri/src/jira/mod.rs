//! Jira Cloud HTTP/data layer (Slice 3).
//!
//! Pure building blocks: fetch + parse + convert, returning typed data.
//! This module does NOT persist `JiraIssueRecord`, mint analysis runs, or
//! orchestrate imports — those are later slices. The Tauri commands / IPC
//! ops that wrap these (in `commands.rs` / `ipc.rs`) just return data.
//!
//! Security posture:
//! - `config::validate_base_url` is the single source of truth for the
//!   "https + *.atlassian.net only" SSRF guard (called by config load AND
//!   the client builder).
//! - the reqwest client follows no redirects and is `https_only`; the host
//!   is re-checked before every request.
//! - the API token / `Authorization` header is never logged, never put in
//!   a `JiraError`, and redacted from `JiraClient`'s `Debug`.
//!
//! Cancellation: every endpoint takes a `CancelToken`
//! (`tokio_util::sync::CancellationToken`). In this slice the callers
//! create a fresh token that never fires; the parameter exists so a later
//! orchestration slice can cancel in-flight fetches.

#![allow(dead_code)]

pub mod adf;
pub mod client;
pub mod config;
pub mod error;
pub mod model;
pub mod parse;
pub mod worktree;

pub use client::{CancelToken, JiraClient, JiraTransport};
pub use error::JiraError;
pub use model::{AssignedIssue, FetchedIssue, ListAssignedResult, Myself};
