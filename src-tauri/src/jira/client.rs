//! Jira HTTP client: the transport seam (`JiraTransport`), the concrete
//! reqwest implementation (`JiraClient`), and the typed endpoint methods.
//!
//! Security invariants enforced here:
//! - `base_url` is validated `https://*.atlassian.net` (see `config.rs`).
//! - the reqwest client follows NO redirects and is `https_only`.
//! - the host is re-checked before every send (defense in depth).
//! - the `Authorization` header is never logged and never stored anywhere
//!   that derives `Debug` over it (the `JiraClient` `Debug` redacts it).

use std::sync::Once;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::config::validate_base_url;
use super::error::JiraError;
use super::model::{FetchedIssue, ListAssignedResult, Myself};
use super::parse;

/// Caller cancellation handle. Plumbed through every endpoint so a later
/// orchestration slice can cancel in-flight fetches. In Slice 3 the token
/// is created fresh per call and never actually fires.
pub type CancelToken = CancellationToken;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
/// Retry only on HTTP 429, at most this many times.
const MAX_RETRIES: u32 = 3;
/// Never wait longer than this between 429 retries.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

/// One raw authenticated GET. The implementation owns auth, timeouts,
/// retries, and redirect policy. This is the test seam — a fake impl lets
/// the endpoint methods be exercised without a live Jira.
#[async_trait::async_trait]
pub trait JiraTransport: Send + Sync {
    async fn get_json(
        &self,
        path_and_query: &str,
        cancel: &CancelToken,
    ) -> Result<Value, JiraError>;
}

/// Concrete Jira Cloud client.
pub struct JiraClient {
    /// Validated `https://*.atlassian.net` origin (no trailing slash issues:
    /// callers pass absolute `/rest/...` paths).
    base_url: String,
    http: reqwest::Client,
    /// "Basic <base64(email:token)>" — NEVER logged, NEVER in Debug output.
    auth_header: String,
}

// Manual Debug that redacts the auth header. A derived Debug would print
// `auth_header`, leaking the base64 token into any `{:?}` / tracing call.
impl std::fmt::Debug for JiraClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JiraClient")
            .field("base_url", &self.base_url)
            .field("auth_header", &"<redacted>")
            .finish()
    }
}

/// Build the `Authorization` value. Pure so a test can assert the token is
/// base64-encoded and that it never leaks via the client's Debug.
pub(crate) fn basic_auth_header(email: &str, token: &str) -> String {
    let encoded = STANDARD.encode(format!("{email}:{token}"));
    format!("Basic {encoded}")
}

impl JiraClient {
    /// Build from config + keyring. Validates `base_url`, reads the token.
    pub fn from_config(cfg: &crate::config::JiraConfig) -> Result<Self, JiraError> {
        let url = validate_base_url(&cfg.base_url)
            .map_err(|e| JiraError::Config(format!("base_url: {e}")))?;
        if cfg.email.trim().is_empty() {
            return Err(JiraError::Config("jira email is empty".to_string()));
        }
        // Normalize the origin (scheme://host[:port]) without a trailing
        // slash so absolute `/rest/...` paths concatenate cleanly.
        let base_url = url.origin().ascii_serialization();
        let token = crate::secrets::get_jira_token(&cfg.base_url).map_err(|e| match e {
            crate::secrets::SecretsError::NotFound(_) => {
                JiraError::Config("Jira API token not set in keyring".to_string())
            }
            other => JiraError::Config(format!("keyring error: {other}")),
        })?;
        let http = Self::build_http()?;
        Ok(Self {
            base_url,
            http,
            auth_header: basic_auth_header(&cfg.email, &token),
        })
    }

    /// Build the reqwest client: no redirects, https-only, bounded timeouts.
    fn build_http() -> Result<reqwest::Client, JiraError> {
        ensure_crypto_provider();
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TOTAL_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .https_only(true)
            .build()
            .map_err(|e| JiraError::Transport(sanitize_reqwest(&e)))
    }

    // ───────── endpoints (IO thin; parsing pure in parse.rs) ─────────

    /// GET `/rest/api/3/myself`.
    pub async fn test_connection(&self, cancel: &CancelToken) -> Result<Myself, JiraError> {
        let v = self.get_json("/rest/api/3/myself", cancel).await?;
        parse::parse_myself(&v)
    }

    /// GET `/rest/api/3/issue/{key}?fields=summary,description`.
    pub async fn fetch_issue(
        &self,
        key: &str,
        cancel: &CancelToken,
    ) -> Result<FetchedIssue, JiraError> {
        let encoded_key: String = url::form_urlencoded::byte_serialize(key.as_bytes()).collect();
        let path = format!("/rest/api/3/issue/{encoded_key}?fields=summary,description");
        let v = match self.get_json(&path, cancel).await {
            Ok(v) => v,
            // Carry the human key (not the encoded form) into NotFound.
            Err(JiraError::NotFound(_)) => return Err(JiraError::NotFound(key.to_string())),
            Err(e) => return Err(e),
        };
        parse::parse_issue(&v)
    }

    /// GET `/rest/api/3/search/jql` for assigned, not-Done issues. Pages via
    /// `nextPageToken`/`isLast`; caps at `parse::MAX_PAGES` and flags
    /// `partial` if the cap is hit before the API reports `isLast`.
    pub async fn list_assigned(
        &self,
        cancel: &CancelToken,
    ) -> Result<ListAssignedResult, JiraError> {
        let mut all = Vec::new();
        let mut token: Option<String> = None;
        let mut partial = false;
        let mut pages_consumed = 0usize;
        loop {
            let path = parse::build_search_query(token.as_deref());
            let v = self.get_json(&path, cancel).await?;
            let page = parse::parse_search_page(&v)?;
            all.extend(page.issues.iter().cloned());
            pages_consumed += 1;
            let (should_continue, p) = parse::loop_decision(&page, pages_consumed);
            if p {
                partial = true;
            }
            if !should_continue {
                break;
            }
            token = page.next_page_token;
        }
        Ok(ListAssignedResult {
            issues: all,
            partial,
        })
    }
}

/// Install the process-wide rustls crypto provider exactly once. reqwest's
/// `rustls-no-provider` feature (reused from the updater so no second TLS
/// stack is pulled) requires a provider to be installed before the first
/// `Client` is built. We use `ring`, which is already in the dependency
/// graph. Idempotent and safe if another component installed one first.
fn ensure_crypto_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Ignore the error: it only fails if a default provider is already
        // installed, which is exactly the state we want.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Strip anything URL/userinfo-shaped from a reqwest error before it goes
/// into a `JiraError::Transport`, so a token embedded in a URL (it never is
/// here, but defense in depth) can't leak into an error message.
fn sanitize_reqwest(e: &reqwest::Error) -> String {
    // reqwest's Display can include the URL; we deliberately use only the
    // coarse error class, never the full Display (which may carry the URL).
    if e.is_timeout() {
        "request timed out".to_string()
    } else if e.is_connect() {
        "connection failed".to_string()
    } else if e.is_request() {
        "request build/send failed".to_string()
    } else if e.is_body() || e.is_decode() {
        "response body error".to_string()
    } else {
        "transport error".to_string()
    }
}

#[async_trait::async_trait]
impl JiraTransport for JiraClient {
    async fn get_json(
        &self,
        path_and_query: &str,
        cancel: &CancelToken,
    ) -> Result<Value, JiraError> {
        let full = format!("{}{}", self.base_url, path_and_query);
        // Re-validate the host before every send: even with redirects
        // disabled, this guarantees we never hit a non-atlassian host.
        let parsed = url::Url::parse(&full)
            .map_err(|e| JiraError::Config(format!("bad request URL: {e}")))?;
        let host_ok = parsed
            .host_str()
            .map(|h| {
                let h = h.to_ascii_lowercase();
                h.ends_with(".atlassian.net") && h != "atlassian.net"
            })
            .unwrap_or(false);
        if parsed.scheme() != "https" || !host_ok {
            return Err(JiraError::Config(
                "refusing to call a non-atlassian.net host".to_string(),
            ));
        }

        let mut attempt: u32 = 0;
        loop {
            if cancel.is_cancelled() {
                return Err(JiraError::Cancelled);
            }
            // Never log the request: it carries the Authorization header.
            let send = self
                .http
                .get(parsed.clone())
                .header(reqwest::header::AUTHORIZATION, &self.auth_header)
                .header(reqwest::header::ACCEPT, "application/json")
                .send();

            let resp = tokio::select! {
                r = send => r,
                _ = cancel.cancelled() => return Err(JiraError::Cancelled),
            };
            let resp = resp.map_err(|e| JiraError::Transport(sanitize_reqwest(&e)))?;
            let status = resp.status();

            if status.is_success() {
                let body = tokio::select! {
                    b = resp.json::<Value>() => b,
                    _ = cancel.cancelled() => return Err(JiraError::Cancelled),
                };
                return body.map_err(|e| JiraError::Parse(sanitize_reqwest(&e)));
            }

            let code = status.as_u16();
            match code {
                401 | 403 => return Err(JiraError::Auth),
                404 => return Err(JiraError::NotFound(String::new())),
                429 => {
                    if attempt >= MAX_RETRIES {
                        return Err(JiraError::RateLimited);
                    }
                    let wait = retry_after(&resp).unwrap_or_else(|| {
                        // Exponential backoff base 1s: 1, 2, 4 …
                        Duration::from_secs(1u64 << attempt.min(5))
                    });
                    let wait = wait.min(MAX_RETRY_WAIT);
                    attempt += 1;
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {},
                        _ = cancel.cancelled() => return Err(JiraError::Cancelled),
                    }
                    continue;
                }
                _ => {
                    // Status text only — never the body (may echo the URL).
                    return Err(JiraError::Http(format!(
                        "{code} {}",
                        status.canonical_reason().unwrap_or("error")
                    )));
                }
            }
        }
    }
}

/// Parse a `Retry-After` header value as whole seconds. Ignores HTTP-date
/// form (we fall back to backoff for that).
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    #[test]
    fn basic_auth_header_is_base64_of_email_colon_token() {
        let h = basic_auth_header("dev@acme.io", "tok123");
        let expected = STANDARD.encode("dev@acme.io:tok123");
        assert_eq!(h, format!("Basic {expected}"));
    }

    #[test]
    fn basic_auth_header_debug_does_not_leak_token() {
        // build_http installs the crypto provider that `rustls-no-provider`
        // requires; reqwest::Client::new() would panic without it.
        let client = JiraClient {
            base_url: "https://acme.atlassian.net".to_string(),
            http: JiraClient::build_http().unwrap(),
            auth_header: basic_auth_header("dev@acme.io", "SuperSecretToken"),
        };
        let dbg = format!("{client:?}");
        assert!(dbg.contains("<redacted>"), "got: {dbg}");
        assert!(!dbg.contains("SuperSecretToken"), "leaked token: {dbg}");
        // The base64 form must not leak either.
        let b64 = STANDARD.encode("dev@acme.io:SuperSecretToken");
        assert!(!dbg.contains(&b64), "leaked base64 token: {dbg}");
    }

    /// A canned-response transport: serves queued JSON values in order.
    struct FakeTransport {
        responses: Mutex<std::collections::VecDeque<Value>>,
        seen_paths: Mutex<Vec<String>>,
    }

    impl FakeTransport {
        fn new(values: Vec<Value>) -> Self {
            Self {
                responses: Mutex::new(values.into_iter().collect()),
                seen_paths: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl JiraTransport for FakeTransport {
        async fn get_json(
            &self,
            path_and_query: &str,
            _cancel: &CancelToken,
        ) -> Result<Value, JiraError> {
            self.seen_paths
                .lock()
                .unwrap()
                .push(path_and_query.to_string());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| JiraError::Parse("fake: no more responses".to_string()))
        }
    }

    // Mirror the endpoint logic against the trait so the happy path is
    // testable without a live Jira. (JiraClient's endpoint methods call
    // self.get_json; here we drive the trait directly with the same parse
    // functions, which is what those methods do.)
    async fn fetch_issue_via<T: JiraTransport>(
        t: &T,
        key: &str,
        cancel: &CancelToken,
    ) -> Result<FetchedIssue, JiraError> {
        let encoded: String = url::form_urlencoded::byte_serialize(key.as_bytes()).collect();
        let path = format!("/rest/api/3/issue/{encoded}?fields=summary,description");
        let v = t.get_json(&path, cancel).await?;
        parse::parse_issue(&v)
    }

    async fn list_assigned_via<T: JiraTransport>(
        t: &T,
        cancel: &CancelToken,
    ) -> Result<ListAssignedResult, JiraError> {
        let mut all = Vec::new();
        let mut token: Option<String> = None;
        let mut partial = false;
        let mut consumed = 0usize;
        loop {
            let path = parse::build_search_query(token.as_deref());
            let v = t.get_json(&path, cancel).await?;
            let page = parse::parse_search_page(&v)?;
            all.extend(page.issues.iter().cloned());
            consumed += 1;
            let (cont, p) = parse::loop_decision(&page, consumed);
            if p {
                partial = true;
            }
            if !cont {
                break;
            }
            token = page.next_page_token;
        }
        Ok(ListAssignedResult {
            issues: all,
            partial,
        })
    }

    #[tokio::test]
    async fn fetch_issue_happy_path_with_fake_transport() {
        let canned = json!({
            "id": "10042",
            "key": "PROJ-7",
            "fields": {
                "summary": "Do the thing",
                "description": {
                    "type": "doc",
                    "content": [
                        { "type": "paragraph", "content": [
                            { "type": "text", "text": "Body text" }
                        ]}
                    ]
                }
            }
        });
        let t = FakeTransport::new(vec![canned]);
        let cancel = CancelToken::new();
        let issue = fetch_issue_via(&t, "PROJ-7", &cancel).await.unwrap();
        assert_eq!(issue.jira_issue_id, "10042");
        assert_eq!(issue.jira_key, "PROJ-7");
        assert_eq!(issue.summary, "Do the thing");
        assert_eq!(issue.description_markdown, "Body text");
    }

    #[tokio::test]
    async fn list_assigned_paginates_with_fake_transport() {
        let page1 = json!({
            "issues": [ { "id": "1", "key": "P-1", "fields": {"summary": "a"} } ],
            "isLast": false,
            "nextPageToken": "tok2"
        });
        let page2 = json!({
            "issues": [ { "id": "2", "key": "P-2", "fields": {"summary": "b"} } ],
            "isLast": true
        });
        let t = FakeTransport::new(vec![page1, page2]);
        let cancel = CancelToken::new();
        let res = list_assigned_via(&t, &cancel).await.unwrap();
        assert_eq!(res.issues.len(), 2);
        assert!(!res.partial);
        assert_eq!(res.issues[0].key, "P-1");
        assert_eq!(res.issues[1].key, "P-2");
    }
}
