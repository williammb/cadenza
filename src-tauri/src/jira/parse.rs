//! Pure response parsers + JQL pagination, split from IO so the loop
//! logic and JSON shapes are unit-testable without a live Jira.

use serde_json::Value;

use super::adf;
use super::error::JiraError;
use super::model::{AssignedIssue, FetchedIssue, Myself};

/// Max pages the assigned-issues loop will fetch before giving up and
/// flagging the result `partial`.
pub const MAX_PAGES: usize = 20;

/// Parse `/rest/api/3/myself`.
pub fn parse_myself(v: &Value) -> Result<Myself, JiraError> {
    let account_id = v
        .get("accountId")
        .and_then(Value::as_str)
        .ok_or_else(|| JiraError::Parse("myself: missing accountId".to_string()))?
        .to_string();
    let display_name = v
        .get("displayName")
        .and_then(Value::as_str)
        .ok_or_else(|| JiraError::Parse("myself: missing displayName".to_string()))?
        .to_string();
    Ok(Myself {
        account_id,
        display_name,
    })
}

/// Parse `/rest/api/3/issue/{key}?fields=summary,description`.
pub fn parse_issue(v: &Value) -> Result<FetchedIssue, JiraError> {
    let jira_issue_id = v
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| JiraError::Parse("issue: missing id".to_string()))?
        .to_string();
    let jira_key = v
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| JiraError::Parse("issue: missing key".to_string()))?
        .to_string();
    let fields = v.get("fields");
    let summary = fields
        .and_then(|f| f.get("summary"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // Description is an ADF object or null. Retain it verbatim in raw_adf.
    let raw_adf = fields
        .and_then(|f| f.get("description"))
        .cloned()
        .unwrap_or(Value::Null);
    let description_markdown = adf::adf_to_markdown(&raw_adf);
    Ok(FetchedIssue {
        jira_issue_id,
        jira_key,
        summary,
        description_markdown,
        raw_adf,
    })
}

/// One page of `/rest/api/3/search/jql`.
pub struct SearchPage {
    pub issues: Vec<AssignedIssue>,
    pub next_page_token: Option<String>,
    pub is_last: bool,
}

/// Build the path+query for the assigned-issues search. JQL is
/// percent-encoded via `url`. `next_page_token`, when present, paginates.
pub fn build_search_query(next_page_token: Option<&str>) -> String {
    // JQL: issues assigned to me that are not Done.
    const JQL: &str = "assignee = currentUser() AND statusCategory != Done";
    // Percent-encode each query value with url's serializer.
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("jql", JQL);
    serializer.append_pair("fields", "summary");
    if let Some(token) = next_page_token {
        serializer.append_pair("nextPageToken", token);
    }
    format!("/rest/api/3/search/jql?{}", serializer.finish())
}

/// Parse one search page. Reads `issues[]` (with nested `fields.summary`),
/// `nextPageToken`, and `isLast`.
pub fn parse_search_page(v: &Value) -> Result<SearchPage, JiraError> {
    let issues_arr = v
        .get("issues")
        .and_then(Value::as_array)
        .ok_or_else(|| JiraError::Parse("search: missing issues array".to_string()))?;
    let mut issues = Vec::with_capacity(issues_arr.len());
    for item in issues_arr {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| JiraError::Parse("search: issue missing id".to_string()))?
            .to_string();
        let key = item
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| JiraError::Parse("search: issue missing key".to_string()))?
            .to_string();
        let summary = item
            .get("fields")
            .and_then(|f| f.get("summary"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        issues.push(AssignedIssue { key, id, summary });
    }
    let next_page_token = v
        .get("nextPageToken")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    // `isLast` may be absent; absence + no token ⇒ treat as last.
    let is_last = match v.get("isLast").and_then(Value::as_bool) {
        Some(b) => b,
        None => next_page_token.is_none(),
    };
    Ok(SearchPage {
        issues,
        next_page_token,
        is_last,
    })
}

/// Decide whether the pagination loop should keep going, given the page
/// just parsed and how many pages have been consumed so far. Pure so the
/// loop can be unit-tested without IO.
///
/// Returns `(should_continue, partial)`:
/// - stop with `partial=false` when `is_last`,
/// - stop with `partial=true` when the page cap is hit while `!is_last`,
/// - otherwise continue.
pub fn loop_decision(page: &SearchPage, pages_consumed: usize) -> (bool, bool) {
    if page.is_last || page.next_page_token.is_none() {
        return (false, false);
    }
    if pages_consumed >= MAX_PAGES {
        return (false, true); // cap hit before isLast ⇒ partial
    }
    (true, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_myself_ok() {
        let v = json!({"accountId": "abc-123", "displayName": "Dev Person"});
        let m = parse_myself(&v).unwrap();
        assert_eq!(m.account_id, "abc-123");
        assert_eq!(m.display_name, "Dev Person");
    }

    #[test]
    fn parse_myself_missing_fields_errors() {
        assert!(parse_myself(&json!({"accountId": "x"})).is_err());
        assert!(parse_myself(&json!({})).is_err());
    }

    #[test]
    fn parse_issue_ok_with_description() {
        let v = json!({
            "id": "10042",
            "key": "PROJ-123",
            "fields": {
                "summary": "Fix the bug",
                "description": {
                    "type": "doc",
                    "content": [
                        { "type": "paragraph", "content": [
                            { "type": "text", "text": "Repro steps" }
                        ]}
                    ]
                }
            }
        });
        let issue = parse_issue(&v).unwrap();
        assert_eq!(issue.jira_issue_id, "10042");
        assert_eq!(issue.jira_key, "PROJ-123");
        assert_eq!(issue.summary, "Fix the bug");
        assert_eq!(issue.description_markdown, "Repro steps");
    }

    #[test]
    fn parse_issue_null_description_empty_markdown() {
        let v = json!({
            "id": "1",
            "key": "P-1",
            "fields": { "summary": "s", "description": null }
        });
        let issue = parse_issue(&v).unwrap();
        assert_eq!(issue.description_markdown, "");
        assert_eq!(issue.raw_adf, Value::Null);
    }

    #[test]
    fn parse_issue_retains_raw_adf() {
        let adf = json!({
            "type": "doc",
            "content": [
                { "type": "paragraph", "content": [{"type": "text", "text": "hi"}] }
            ]
        });
        let v = json!({
            "id": "1", "key": "P-1",
            "fields": { "summary": "s", "description": adf }
        });
        let issue = parse_issue(&v).unwrap();
        assert_eq!(issue.raw_adf, adf);
    }

    #[test]
    fn parse_search_page_single_page_is_last() {
        let v = json!({
            "issues": [
                { "id": "1", "key": "P-1", "fields": {"summary": "a"} },
                { "id": "2", "key": "P-2", "fields": {"summary": "b"} }
            ],
            "isLast": true
        });
        let page = parse_search_page(&v).unwrap();
        assert_eq!(page.issues.len(), 2);
        assert!(page.is_last);
        assert!(page.next_page_token.is_none());
    }

    #[test]
    fn parse_search_page_extracts_next_token() {
        let v = json!({
            "issues": [ { "id": "1", "key": "P-1", "fields": {"summary": "a"} } ],
            "isLast": false,
            "nextPageToken": "tok-xyz"
        });
        let page = parse_search_page(&v).unwrap();
        assert_eq!(page.next_page_token.as_deref(), Some("tok-xyz"));
        assert!(!page.is_last);
    }

    #[test]
    fn build_search_query_first_page_no_token() {
        let q = build_search_query(None);
        assert!(q.starts_with("/rest/api/3/search/jql?"), "got: {q}");
        assert!(q.contains("jql="), "got: {q}");
        assert!(q.contains("fields=summary"), "got: {q}");
        assert!(!q.contains("nextPageToken"), "got: {q}");
        // JQL must be percent-encoded (spaces → '+' or %20, '=' encoded).
        assert!(!q.contains("currentUser() AND"), "unencoded JQL: {q}");
    }

    #[test]
    fn build_search_query_with_token() {
        let q = build_search_query(Some("tok-abc"));
        assert!(q.contains("nextPageToken=tok-abc"), "got: {q}");
    }

    #[test]
    fn list_pagination_stops_on_is_last() {
        // Two canned pages: second is last. Drive loop_decision like the
        // client loop does and assert partial=false.
        let pages = [
            SearchPage {
                issues: vec![AssignedIssue {
                    key: "P-1".into(),
                    id: "1".into(),
                    summary: "a".into(),
                }],
                next_page_token: Some("t1".into()),
                is_last: false,
            },
            SearchPage {
                issues: vec![AssignedIssue {
                    key: "P-2".into(),
                    id: "2".into(),
                    summary: "b".into(),
                }],
                next_page_token: None,
                is_last: true,
            },
        ];
        let mut all = Vec::new();
        let mut partial = false;
        for (i, page) in pages.iter().enumerate() {
            all.extend(page.issues.iter().cloned());
            let (cont, p) = loop_decision(page, i + 1);
            if p {
                partial = true;
            }
            if !cont {
                break;
            }
        }
        assert_eq!(all.len(), 2);
        assert!(!partial);
    }

    #[test]
    fn list_pagination_cap_sets_partial() {
        // Feed MAX_PAGES+5 pages that each claim there is more. The loop must
        // stop at the cap and flag partial=true.
        let total = MAX_PAGES + 5;
        let mut consumed = 0usize;
        let mut partial = false;
        for i in 0..total {
            let page = SearchPage {
                issues: vec![AssignedIssue {
                    key: format!("P-{i}"),
                    id: i.to_string(),
                    summary: "x".into(),
                }],
                next_page_token: Some(format!("t{i}")),
                is_last: false,
            };
            consumed += 1;
            let (cont, p) = loop_decision(&page, consumed);
            if p {
                partial = true;
            }
            if !cont {
                break;
            }
        }
        assert_eq!(consumed, MAX_PAGES);
        assert!(partial);
    }
}
