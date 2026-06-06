//! Typed return structs for the Jira data layer. These are the
//! app-internal shapes returned by `JiraClient`; the cross-socket
//! mirrors live in `cadenza_proto::ops` (built from these in `commands`).

use serde::{Deserialize, Serialize};

/// `/rest/api/3/myself` — the authenticated account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Myself {
    pub account_id: String,
    pub display_name: String,
}

/// A fetched Jira issue. `raw_adf` is the description ADF retained
/// verbatim (or `Null`); `description_markdown` is the converted form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchedIssue {
    /// Durable numeric id, e.g. "10042".
    pub jira_issue_id: String,
    /// Human key, e.g. "PROJ-123".
    pub jira_key: String,
    pub summary: String,
    /// "" when the description is null.
    pub description_markdown: String,
    /// The description ADF kept verbatim; `Null` when there is none.
    pub raw_adf: serde_json::Value,
}

/// One row in the assigned-issues list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignedIssue {
    pub key: String,
    pub id: String,
    pub summary: String,
}

/// Result of paging the assigned-issues search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListAssignedResult {
    pub issues: Vec<AssignedIssue>,
    /// `true` if the page cap was hit before the API reported `isLast`.
    pub partial: bool,
}
