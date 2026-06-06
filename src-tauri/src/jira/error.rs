//! `JiraError` — the single error type for the Jira HTTP/data layer.
//!
//! HARD constraint: the API token / `Authorization` header must NEVER
//! appear in a `JiraError`, its `Display`, or its `Debug`. Variants only
//! ever carry sanitized status text, keys, or our own messages — the
//! client (`client.rs`) constructs them from sanitized inputs only.

/// Errors from building or driving the Jira client.
#[derive(Debug)]
pub enum JiraError {
    /// Missing/invalid base_url, email, or token not set in the keyring.
    Config(String),
    /// HTTP 401/403.
    Auth,
    /// HTTP 404 — carries the resource key (e.g. issue key), never a token.
    NotFound(String),
    /// HTTP 429 after the retry budget was exhausted.
    RateLimited,
    /// Caller cancellation token fired.
    Cancelled,
    /// Other non-2xx; carries status text only (NO token, NO auth header).
    Http(String),
    /// reqwest send/connect/timeout error (sanitized — no URL with userinfo).
    Transport(String),
    /// Malformed JSON / unexpected response shape.
    Parse(String),
}

impl JiraError {
    /// `(wire code, message)` for the IPC `ErrorBody` and the CLI exit-code
    /// table (see `cadenza-cli/src/client.rs`). The message never contains
    /// the token or the `Authorization` header.
    pub fn code_message(&self) -> (&'static str, String) {
        match self {
            JiraError::Config(m) => ("jira_config", m.clone()),
            JiraError::Auth => (
                "jira_auth",
                "Jira authentication failed (check email/token)".to_string(),
            ),
            JiraError::NotFound(key) => {
                ("jira_not_found", format!("Jira resource not found: {key}"))
            }
            JiraError::RateLimited => (
                "jira_rate_limited",
                "Jira rate limit exceeded; try again later".to_string(),
            ),
            JiraError::Cancelled => ("jira_cancelled", "Jira request cancelled".to_string()),
            JiraError::Http(s) => ("jira_http", format!("Jira HTTP error: {s}")),
            JiraError::Transport(s) => ("jira_transport", format!("Jira transport error: {s}")),
            JiraError::Parse(s) => ("jira_parse", format!("Jira response parse error: {s}")),
        }
    }
}

impl std::fmt::Display for JiraError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (code, msg) = self.code_message();
        write!(f, "[{code}] {msg}")
    }
}

impl std::error::Error for JiraError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_message_auth_is_jira_auth() {
        let (code, _) = JiraError::Auth.code_message();
        assert_eq!(code, "jira_auth");
    }

    #[test]
    fn code_message_not_found_is_jira_not_found() {
        let (code, msg) = JiraError::NotFound("PROJ-1".to_string()).code_message();
        assert_eq!(code, "jira_not_found");
        assert!(msg.contains("PROJ-1"));
    }

    #[test]
    fn code_message_rate_limited() {
        let (code, _) = JiraError::RateLimited.code_message();
        assert_eq!(code, "jira_rate_limited");
    }

    #[test]
    fn code_message_config() {
        let (code, msg) = JiraError::Config("bad url".to_string()).code_message();
        assert_eq!(code, "jira_config");
        assert!(msg.contains("bad url"));
    }

    #[test]
    fn jira_error_display_never_contains_token() {
        // Construct every variant; assert none of their Display/Debug forms
        // ever surface a token-shaped string. We never put a token into any
        // variant, so this guards against a future regression.
        let token = "SuperSecretToken12345";
        let variants = vec![
            JiraError::Config("invalid base_url".to_string()),
            JiraError::Auth,
            JiraError::NotFound("PROJ-1".to_string()),
            JiraError::RateLimited,
            JiraError::Cancelled,
            JiraError::Http("503 Service Unavailable".to_string()),
            JiraError::Transport("connection reset".to_string()),
            JiraError::Parse("missing field `id`".to_string()),
        ];
        for v in &variants {
            let disp = format!("{v}");
            let dbg = format!("{v:?}");
            assert!(!disp.contains(token), "Display leaked token: {disp}");
            assert!(!dbg.contains(token), "Debug leaked token: {dbg}");
            assert!(
                !disp.to_lowercase().contains("authorization"),
                "Display leaked auth header: {disp}"
            );
        }
    }
}
