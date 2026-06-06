//! Jira issue wire/record type.
//!
//! A `JiraIssueRecord` is the cached identity + lifecycle state for a Jira
//! issue imported into Cadenza. It is keyed by `(jira_site, jira_issue_id)`
//! and lives in `~/.cadenza/jira/<slug>.json` on the `Files` backend and in
//! the `jira_issues` table on the SQL backends. Like `Ideia`, it does not
//! exist in the Node.js legacy format, so the schema can evolve freely.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JiraIssueRecord {
    pub jira_site: String,
    pub jira_issue_id: String,
    /// Display key, e.g. "PROJ-123".
    pub jira_key: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis_run_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_hash: Option<String>,
    /// Epoch-ms expiry of the cached secret; None = no secret cached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_expiry_ms: Option<i64>,
    /// Free-form status string for the secret lifecycle (e.g. "active","expired").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_status: Option<String>,

    /// Raw Atlassian Document Format payload (JSON text) or a ref to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_adf: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// Worktree lifecycle state string (e.g. "pending","created","removed").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_state: Option<String>,

    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Typed view of the capability-secret lifecycle. The persisted form stays
/// the free-form `JiraIssueRecord.secret_status: Option<String>` column (no
/// schema change); this enum is the in-memory typed bridge to/from that
/// string so call sites match on a closed set rather than raw strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretStatus {
    Active,
    Revoked,
    Expired,
}

impl SecretStatus {
    /// Canonical on-disk/SQL string for this status.
    pub fn as_str(self) -> &'static str {
        match self {
            SecretStatus::Active => "active",
            SecretStatus::Revoked => "revoked",
            SecretStatus::Expired => "expired",
        }
    }

    /// Parse the persisted string back into the typed status. Unknown
    /// strings (including legacy free-form values) return `None`.
    pub fn parse(s: &str) -> Option<SecretStatus> {
        match s {
            "active" => Some(SecretStatus::Active),
            "revoked" => Some(SecretStatus::Revoked),
            "expired" => Some(SecretStatus::Expired),
            _ => None,
        }
    }
}

/// Typed view of the per-issue shared-worktree lifecycle. Like
/// [`SecretStatus`], the persisted form stays the free-form
/// `JiraIssueRecord.worktree_state: Option<String>` column (no schema
/// change); this enum is the in-memory typed bridge to/from that string so
/// the worktree state machine matches on a closed set rather than raw
/// strings.
///
/// State semantics (the `None` field value — absent column — is the
/// conceptual "never reserved" state, distinct from every variant here):
/// - [`Reserved`](WorktreeState::Reserved): an ensure call has claimed the
///   issue but has not touched git yet.
/// - [`Creating`](WorktreeState::Creating): git work (branch / worktree add)
///   is in progress.
/// - [`Ready`](WorktreeState::Ready): `branch_name` + `worktree_path` +
///   `base_sha` are all set and the worktree exists on disk.
/// - [`Failed`](WorktreeState::Failed): terminal-but-retryable; the next
///   ensure call retries creation from scratch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeState {
    Reserved,
    Creating,
    Ready,
    Failed,
}

impl WorktreeState {
    /// Canonical on-disk/SQL string for this state.
    pub fn as_str(self) -> &'static str {
        match self {
            WorktreeState::Reserved => "reserved",
            WorktreeState::Creating => "creating",
            WorktreeState::Ready => "ready",
            WorktreeState::Failed => "failed",
        }
    }

    /// Parse the persisted string back into the typed state. Unknown
    /// strings — including legacy free-form values such as `"pending"`,
    /// `"created"`, or `"removed"` named in older docs — return `None`,
    /// which the ensure path treats as "never reserved" and recreates.
    pub fn parse(s: &str) -> Option<WorktreeState> {
        match s {
            "reserved" => Some(WorktreeState::Reserved),
            "creating" => Some(WorktreeState::Creating),
            "ready" => Some(WorktreeState::Ready),
            "failed" => Some(WorktreeState::Failed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_status_enum_roundtrips_strings() {
        for s in [
            SecretStatus::Active,
            SecretStatus::Revoked,
            SecretStatus::Expired,
        ] {
            assert_eq!(SecretStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(SecretStatus::Active.as_str(), "active");
        assert_eq!(SecretStatus::Revoked.as_str(), "revoked");
        assert_eq!(SecretStatus::Expired.as_str(), "expired");
        assert_eq!(SecretStatus::parse("bogus"), None);
    }

    #[test]
    fn worktree_state_enum_roundtrips_strings() {
        for s in [
            WorktreeState::Reserved,
            WorktreeState::Creating,
            WorktreeState::Ready,
            WorktreeState::Failed,
        ] {
            assert_eq!(WorktreeState::parse(s.as_str()), Some(s));
        }
        assert_eq!(WorktreeState::Reserved.as_str(), "reserved");
        assert_eq!(WorktreeState::Creating.as_str(), "creating");
        assert_eq!(WorktreeState::Ready.as_str(), "ready");
        assert_eq!(WorktreeState::Failed.as_str(), "failed");
        assert_eq!(WorktreeState::parse("bogus"), None);
        // Legacy free-form strings named in older docs must reject.
        assert_eq!(WorktreeState::parse("pending"), None);
        assert_eq!(WorktreeState::parse("created"), None);
        assert_eq!(WorktreeState::parse("removed"), None);
    }

    fn sample() -> JiraIssueRecord {
        JiraIssueRecord {
            jira_site: "https://x.atlassian.net".into(),
            jira_issue_id: "10001".into(),
            jira_key: "PROJ-123".into(),
            project_id: None,
            analysis_run_id: None,
            secret_hash: None,
            secret_expiry_ms: None,
            secret_status: None,
            raw_adf: None,
            branch_name: None,
            worktree_path: None,
            base_sha: None,
            worktree_state: None,
            created_at_ms: 1,
            updated_at_ms: 2,
        }
    }

    #[test]
    fn jira_issue_record_roundtrips_json() {
        let rec = sample();
        let json = serde_json::to_string(&rec).unwrap();
        // Optional `None` fields are omitted via skip_serializing_if.
        assert!(!json.contains("project_id"));
        assert!(!json.contains("raw_adf"));
        let back: JiraIssueRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec, back);
    }
}
