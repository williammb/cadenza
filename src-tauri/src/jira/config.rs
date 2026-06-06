//! Pure `base_url` validation — the single source of truth for the
//! "https + *.atlassian.net only" SSRF guard. Called from both
//! `crate::config::Config::validate` (config load) and the HTTP client
//! builder (`super::client::JiraClient::from_config`), so the host rule
//! is never duplicated.

use url::{Host, Url};

/// Accept iff: scheme is exactly `https` AND host ends with `.atlassian.net`
/// (and is not the bare `atlassian.net`). Rejects http, IP literals,
/// localhost, and any other host. Returns the parsed `Url` for reuse.
///
/// This is the SSRF guard: only Jira Cloud sites are reachable. Ports and
/// userinfo are allowed by the parser but the host rule is what gates
/// reachability, so they cannot redirect us off-host.
pub fn validate_base_url(raw: &str) -> Result<Url, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("base_url is empty".to_string());
    }
    let url = Url::parse(trimmed).map_err(|e| format!("not a valid URL: {e}"))?;

    if url.scheme() != "https" {
        return Err(format!("scheme must be https, got `{}`", url.scheme()));
    }

    // Reject IP literals outright (Host::Ipv4/Ipv6) — defense against
    // numeric SSRF targets that happen to satisfy a naive suffix check.
    match url.host() {
        None => return Err("missing host".to_string()),
        Some(Host::Ipv4(_)) | Some(Host::Ipv6(_)) => {
            return Err("host must not be an IP literal".to_string());
        }
        Some(Host::Domain(_)) => {}
    }

    let host = url
        .host_str()
        .ok_or_else(|| "missing host".to_string())?
        .to_ascii_lowercase();

    if host == "localhost" {
        return Err("localhost is not allowed".to_string());
    }
    if host == "atlassian.net" {
        return Err("bare atlassian.net is not allowed; use <site>.atlassian.net".to_string());
    }
    if !host.ends_with(".atlassian.net") {
        return Err(format!(
            "host must be a *.atlassian.net Jira Cloud site, got `{host}`"
        ));
    }

    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_accepts_https_atlassian_net() {
        assert!(validate_base_url("https://acme.atlassian.net").is_ok());
    }

    #[test]
    fn base_url_accepts_subdomain_atlassian_net() {
        // Multi-label subdomain still ends with .atlassian.net.
        assert!(validate_base_url("https://team.acme.atlassian.net").is_ok());
    }

    #[test]
    fn base_url_rejects_http() {
        assert!(validate_base_url("http://acme.atlassian.net").is_err());
    }

    #[test]
    fn base_url_rejects_ipv4() {
        assert!(validate_base_url("https://127.0.0.1").is_err());
    }

    #[test]
    fn base_url_rejects_ipv6() {
        assert!(validate_base_url("https://[::1]").is_err());
    }

    #[test]
    fn base_url_rejects_localhost() {
        assert!(validate_base_url("https://localhost").is_err());
    }

    #[test]
    fn base_url_rejects_other_domain() {
        assert!(validate_base_url("https://evil.example.com").is_err());
        // A host merely *containing* atlassian.net but not ending with it.
        assert!(validate_base_url("https://atlassian.net.evil.com").is_err());
    }

    #[test]
    fn base_url_rejects_bare_atlassian_net() {
        assert!(validate_base_url("https://atlassian.net").is_err());
    }

    #[test]
    fn base_url_rejects_empty_and_garbage() {
        assert!(validate_base_url("").is_err());
        assert!(validate_base_url("   ").is_err());
        assert!(validate_base_url("not a url").is_err());
        assert!(validate_base_url("ftp://acme.atlassian.net").is_err());
    }
}
