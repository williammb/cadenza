//! Diagnostics export — bundle the rolling logs plus environment info into
//! a single `.zip` a user can attach to a support request.
//!
//! # Redaction manifest (READ THIS BEFORE CHANGING THE BUNDLE)
//!
//! A diagnostics bundle is meant to be shared with strangers, so the
//! contents are governed by an **explicit allow/deny manifest** rather than
//! a vague "we don't log secrets" assumption. Anything that lands in the
//! zip is either inert by construction (logs are English, structured, and
//! secret-free by policy) or is run through [`redact_log_line`] before it
//! is written.
//!
//! ## What the bundle CONTAINS
//!
//! - `manifest.txt` — a copy of this redaction policy, so whoever opens
//!   the bundle knows exactly what was and wasn't scrubbed.
//! - `env.txt` — app version, protocol version, OS family / arch, and the
//!   resolved data-dir / log-dir **paths** (see "home-dir paths" below).
//! - `logs/cadenza.*.log` — every rolling log file from
//!   [`observ::log_dir`], with each line passed through
//!   [`redact_log_line`].
//!
//! ## What is REDACTED (masked, never written verbatim)
//!
//! The patterns in [`REDACTION_RULES`] are applied to every log line and to
//! the environment report. Each rule masks the *value* while keeping enough
//! structure for a reader to understand a line was redacted:
//!
//! 1. **CLI auth token** — the bearer token from `~/.cadenza/auth`. The
//!    auth file itself is NEVER added to the bundle, and any `token=…` /
//!    `Bearer …` / `Authorization: …` occurrence in a log line is masked.
//! 2. **Keyring / Postgres password** — masked on any `password=…` /
//!    `pwd=…` occurrence. The password lives only in the OS keyring and is
//!    never on disk, but log lines that ever interpolate one are scrubbed
//!    defensively.
//! 3. **Jira API token** — masked on `token=…` (shared with rule 1) and on
//!    `api_token=…`. The Jira token also lives only in the keyring.
//! 4. **`Authorization` / `Cookie` / `Set-Cookie` HTTP headers** — masked
//!    wholesale, since HTTP debugging logs can echo them.
//!
//! ## What is NOT redacted (and why that's acceptable)
//!
//! - **The OS username embedded in home-dir paths.** Paths like
//!   `C:\Users\<name>\.cadenza\logs` and the Windows named-pipe name
//!   `\\.\pipe\cadenza-<name>` reveal the local account name. This is
//!   low-sensitivity (it is not a credential) and is frequently *needed*
//!   to diagnose path / permission issues, so it is kept. Users who
//!   consider their username sensitive should review `env.txt` before
//!   sharing the bundle — `manifest.txt` says so explicitly.
//! - **Jira `base_url` / `email` and Postgres `host` / `user` /
//!   `database`.** These are connection metadata, not secrets, and are
//!   often essential to reproduce an issue. They are NOT masked.
//! - **Task titles / bodies and project names** do not appear in the logs
//!   (log lines are English and reference ids, not free text), so they are
//!   not part of the bundle at all.
//!
//! Log lines are always English (per `observ.rs`); this module never adds a
//! second i18n surface and never logs the bundle contents.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;

/// One redaction rule: a human-readable name (documented in the module
/// manifest and copied into `manifest.txt`) plus the masking transform.
///
/// Rules are pure `fn(&str) -> String` so they compose and stay trivially
/// unit-testable without any I/O.
struct RedactionRule {
    /// Stable identifier shown in `manifest.txt` so a bundle reader can map
    /// a masked value back to the policy entry that masked it.
    name: &'static str,
    /// What the rule scrubs, in one line, for `manifest.txt`.
    description: &'static str,
    /// Mask transform applied to a single line.
    apply: fn(&str) -> String,
}

/// The placeholder substituted for any redacted value. Distinct and
/// greppable so a reader can spot redactions at a glance.
const REDACTED: &str = "[REDACTED]";

/// The explicit, ordered redaction manifest. Every line written into the
/// bundle is passed through each rule, in order. Adding data to the bundle
/// that could carry a secret REQUIRES a matching rule here.
const REDACTION_RULES: &[RedactionRule] = &[
    RedactionRule {
        name: "auth-token",
        description: "CLI/Jira bearer token: token=…, Bearer …, api_token=…",
        apply: mask_tokens,
    },
    RedactionRule {
        name: "password",
        description: "Postgres / keyring password: password=…, pwd=…",
        apply: mask_passwords,
    },
    RedactionRule {
        name: "auth-headers",
        description: "HTTP Authorization / Cookie / Set-Cookie header values",
        apply: mask_auth_headers,
    },
];

/// Mask `token=…`, `api_token=…`, `Bearer …`, and `Authorization: Bearer …`
/// occurrences. Case-insensitive on the key; preserves everything up to and
/// including the delimiter so the surrounding log structure survives.
fn mask_tokens(line: &str) -> String {
    let mut out = mask_kv(line, &["token", "api_token", "apitoken", "access_token"]);
    out = mask_after_keyword(&out, "bearer");
    out
}

/// Mask `password=…`, `pwd=…`, `passwd=…` occurrences and the password
/// component of any `scheme://user:password@host` connection string — the
/// shape a Postgres DSN takes when it lands in a connection-error log line.
fn mask_passwords(line: &str) -> String {
    mask_url_userinfo(&mask_kv(line, &["password", "passwd", "pwd"]))
}

/// Mask the password in a `scheme://user:password@host` URL so a logged DSN
/// (`postgres://user:secret@host/db`) does not leak the password. Only the
/// segment between the first `:` of the userinfo and the `@` is replaced; the
/// scheme, user, and host are preserved for diagnostic value. Handles multiple
/// URLs on one line.
fn mask_url_userinfo(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        let Some(scheme_end) = rest.find("://") else {
            out.push_str(rest);
            break;
        };
        let auth_start = scheme_end + 3;
        // The authority ends at the first path/query/fragment char or whitespace.
        let auth_end = rest[auth_start..]
            .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
            .map(|off| auth_start + off)
            .unwrap_or(rest.len());
        let authority = &rest[auth_start..auth_end];
        match (authority.find(':'), authority.rfind('@')) {
            // `user:password@host` — the first `:` opens the password, the `@`
            // closes the userinfo. A `:` after the `@` is a host port, not a
            // password, so require `colon < at`.
            (Some(colon), Some(at)) if colon < at => {
                out.push_str(&rest[..auth_start + colon + 1]);
                out.push_str(REDACTED);
                out.push_str(&rest[auth_start + at..auth_end]);
            }
            // No userinfo password (bare host, or `host:port`): emit unchanged.
            _ => out.push_str(&rest[..auth_end]),
        }
        rest = &rest[auth_end..];
    }
    out
}

/// Mask the value of `Authorization:`, `Cookie:`, and `Set-Cookie:` headers.
///
/// A header value runs to end of line, so once the EARLIEST header on the
/// line is found everything from its value onward is masked — including any
/// later header on the same line. We must key on the earliest *position*, not
/// a fixed name priority: a line like `cookie: <secret> authorization: <tok>`
/// would otherwise mask `authorization` (first by name) yet leak the `cookie`
/// value that precedes it.
fn mask_auth_headers(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let earliest = ["authorization:", "cookie:", "set-cookie:"]
        .iter()
        .filter_map(|header| lower.find(header).map(|pos| pos + header.len()))
        .min();
    match earliest {
        Some(value_start) => {
            let mut masked = String::with_capacity(line.len());
            masked.push_str(&line[..value_start]);
            masked.push(' ');
            masked.push_str(REDACTED);
            masked
        }
        None => line.to_string(),
    }
}

/// Mask `key=value` / `key: value` / `key"="value"` for any of `keys`
/// (case-insensitive key match). A quoted value is masked through its closing
/// quote (honoring `\"` escapes); a bare value runs to the next whitespace or
/// quote. We deliberately do NOT end a bare value at `,`/`}`/`)`: a secret can
/// contain those, and stopping early would leak the tail. Over-masking a
/// trailing structural char is safe; under-masking a credential is not.
fn mask_kv(line: &str, keys: &[&str]) -> String {
    let lower = line.to_ascii_lowercase();
    let mut result = line.to_string();
    let mut result_lower = lower;

    loop {
        let mut best: Option<(usize, usize)> = None; // (key_pos, key_len)
        for key in keys {
            if let Some(pos) = result_lower.find(key) {
                // Only treat as a key if followed by an assignment delimiter
                // (optionally through a closing quote: `"token":`).
                let after = &result[pos + key.len()..];
                let trimmed = after.trim_start_matches(['"', '\'', ' ']);
                let is_assignment = trimmed.starts_with('=') || trimmed.starts_with(':');
                let earlier = best.map(|(bp, _)| pos < bp).unwrap_or(true);
                if is_assignment && earlier {
                    best = Some((pos, key.len()));
                }
            }
        }
        let Some((key_pos, key_len)) = best else {
            break;
        };

        // Find the delimiter (`=` or `:`) after the key, skipping any
        // closing quote/space, then the start of the value.
        let after_key = key_pos + key_len;
        let bytes = result.as_bytes();
        let mut i = after_key;
        while i < bytes.len() && matches!(bytes[i], b'"' | b'\'' | b' ') {
            i += 1;
        }
        // i is at the delimiter
        if i >= bytes.len() || !matches!(bytes[i], b'=' | b':') {
            // Not actually an assignment; blank the key in the lowercase
            // mirror so it is not rematched, then keep scanning for other keys.
            result_lower.replace_range(key_pos..after_key, &"#".repeat(key_len));
            continue;
        }
        i += 1; // past delimiter
                // Skip spaces after the delimiter, but NOT a quote — an opening quote
                // begins the value, and the whole quoted run (quotes included) is
                // replaced so `"key":"v"` scrubs to `"key":[REDACTED]`.
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        let value_start = i;
        let value_end = if value_start < bytes.len() && matches!(bytes[value_start], b'"' | b'\'') {
            // Quoted value: span through the matching closing quote (inclusive),
            // skipping `\"`/`\\` escapes so `key="a\"b"` masks the whole value
            // instead of stopping at the escaped inner quote (which would leak
            // the tail `b"`).
            let quote = bytes[value_start];
            let mut j = value_start + 1;
            while j < bytes.len() && bytes[j] != quote {
                if bytes[j] == b'\\' && j + 1 < bytes.len() {
                    j += 2;
                } else {
                    j += 1;
                }
            }
            if j < bytes.len() {
                j + 1
            } else {
                bytes.len()
            }
        } else {
            // Bare value: ends only at whitespace or a quote. NOT at `,`/`}`/`)`
            // — a secret may contain those, and stopping there leaks the tail.
            result[value_start..]
                .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\''))
                .map(|off| value_start + off)
                .unwrap_or(result.len())
        };

        if value_end <= value_start {
            // Empty value; blank the key so the loop terminates.
            result_lower.replace_range(key_pos..after_key, &"#".repeat(key_len));
            continue;
        }

        let mut next = String::with_capacity(result.len());
        next.push_str(&result[..value_start]);
        next.push_str(REDACTED);
        next.push_str(&result[value_end..]);
        result = next;
        // Rebuild the lowercase mirror, then blank the just-processed key so
        // the loop cannot rematch it and re-redact `key=[REDACTED]` forever.
        // (REDACTED itself contains no key substrings, so other keys remain
        // discoverable.)
        result_lower = result.to_ascii_lowercase();
        result_lower.replace_range(key_pos..after_key, &"#".repeat(key_len));
    }

    result
}

/// Mask the whitespace-delimited token immediately following EVERY
/// occurrence of `keyword` (case-insensitive), e.g.
/// `Bearer abc Bearer def` → `Bearer [REDACTED] Bearer [REDACTED]`.
/// Masking only the first match would leak a second token on the same line.
fn mask_after_keyword(line: &str, keyword: &str) -> String {
    let mut result = line.to_string();
    // Resume scanning past each masked value so the loop always advances and
    // never rematches `[REDACTED]` (it contains no keyword substring anyway).
    let mut search_from = 0usize;
    while search_from < result.len() {
        let lower = result.to_ascii_lowercase();
        let Some(rel) = lower[search_from..].find(keyword) else {
            break;
        };
        let pos = search_from + rel;
        let after = pos + keyword.len();
        let bytes = result.as_bytes();
        // Anchor to a word boundary so a benign substring (`rebearer`, or the
        // word `bearer` glued to other text) never triggers masking: the char
        // before must be a non-alphanumeric boundary, and the auth scheme is
        // always `keyword<space>token`, so a space must follow the keyword.
        let boundary_before = pos == 0 || !bytes[pos - 1].is_ascii_alphanumeric();
        let space_after = bytes.get(after) == Some(&b' ');
        if !(boundary_before && space_after) {
            search_from = after;
            continue;
        }
        let mut i = after;
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        let value_start = i;
        if value_start >= result.len() {
            break;
        }
        let value_end = result[value_start..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '}' | ')'))
            .map(|off| value_start + off)
            .unwrap_or(result.len());
        if value_end <= value_start {
            // No value after this keyword (e.g. trailing `Bearer`); skip past
            // the keyword so the scan can find any later occurrence.
            search_from = after;
            continue;
        }
        let mut next = String::with_capacity(result.len());
        next.push_str(&result[..value_start]);
        next.push_str(REDACTED);
        next.push_str(&result[value_end..]);
        result = next;
        search_from = value_start + REDACTED.len();
    }
    result
}

/// Apply the full [`REDACTION_RULES`] pipeline to a single line. Public so
/// the command layer and tests can reuse the exact same transform that the
/// bundle writer uses.
pub fn redact_log_line(line: &str) -> String {
    REDACTION_RULES
        .iter()
        .fold(line.to_string(), |acc, rule| (rule.apply)(&acc))
}

/// Render the redaction manifest as the `manifest.txt` shipped in every
/// bundle. Built from [`REDACTION_RULES`] so the doc and the code can't
/// drift: whoever opens the bundle sees the live policy.
fn manifest_text() -> String {
    let mut s = String::new();
    s.push_str("Cadenza diagnostics bundle — redaction manifest\n");
    s.push_str("================================================\n\n");
    s.push_str(
        "This bundle was generated for support/debugging. The values below\n\
         were masked as \"[REDACTED]\" before anything was written.\n\n",
    );
    s.push_str("REDACTED (masked everywhere in this bundle):\n");
    for rule in REDACTION_RULES {
        s.push_str(&format!("  - {}: {}\n", rule.name, rule.description));
    }
    s.push('\n');
    s.push_str("NOT redacted (review env.txt before sharing if this matters):\n");
    s.push_str(
        "  - OS username embedded in home-dir paths and the Windows pipe name\n\
         \x20   (e.g. C:\\Users\\<name>\\.cadenza). Not a credential; needed for\n\
         \x20   path/permission diagnosis.\n",
    );
    s.push_str(
        "  - Connection metadata: Jira base_url/email, Postgres host/user/db.\n\
         \x20   Not secrets; often required to reproduce an issue.\n",
    );
    s.push('\n');
    s.push_str("NEVER included in this bundle:\n");
    s.push_str("  - The ~/.cadenza/auth token file (not added at all).\n");
    s.push_str("  - Keyring secrets (PG password, Jira API token) — never on disk.\n");
    s
}

/// Build the `env.txt` report. Paths are intentionally included (see the
/// module redaction manifest), but the whole report is still run through
/// [`redact_log_line`] for defense in depth.
fn env_text(app_version: &str, protocol_version: u32, data_dir: &Path, log_dir: &Path) -> String {
    let raw = format!(
        "app_version: {app_version}\n\
         protocol_version: {protocol_version}\n\
         os_family: {}\n\
         os_arch: {}\n\
         data_dir: {}\n\
         log_dir: {}\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        data_dir.display(),
        log_dir.display(),
    );
    redact_log_line(&raw)
}

/// Errors surfaced while building a diagnostics bundle.
#[derive(Debug, thiserror::Error)]
pub enum DiagnosticsError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
}

/// Inputs the command layer supplies; kept as plain data so the writer is
/// testable without a live `AppHandle`.
pub struct BundleInputs<'a> {
    pub app_version: &'a str,
    pub protocol_version: u32,
    pub data_dir: PathBuf,
    pub log_dir: PathBuf,
}

/// Write a diagnostics zip to `dest`, returning the number of log files
/// included. The bundle always contains `manifest.txt` and `env.txt`; log
/// files are added best-effort (a single unreadable file is logged and
/// skipped rather than failing the whole export).
pub fn write_bundle(dest: &Path, inputs: &BundleInputs<'_>) -> Result<usize, DiagnosticsError> {
    let file = std::fs::File::create(dest)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("manifest.txt", opts)?;
    zip.write_all(manifest_text().as_bytes())?;

    zip.start_file("env.txt", opts)?;
    zip.write_all(
        env_text(
            inputs.app_version,
            inputs.protocol_version,
            &inputs.data_dir,
            &inputs.log_dir,
        )
        .as_bytes(),
    )?;

    let mut log_count = 0usize;
    if inputs.log_dir.is_dir() {
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(&inputs.log_dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file())
                .collect(),
            Err(e) => {
                tracing::warn!(error = ?e, "diagnostics: failed to list log dir");
                Vec::new()
            }
        };
        entries.sort();
        for path in entries {
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let contents = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    // Lossless read may fail on a non-UTF-8 byte; skip the
                    // file rather than abort the whole export.
                    tracing::warn!(error = ?e, file = %name, "diagnostics: skipping unreadable log");
                    continue;
                }
            };
            let redacted: String = contents
                .lines()
                .map(redact_log_line)
                .collect::<Vec<_>>()
                .join("\n");
            zip.start_file(format!("logs/{name}"), opts)?;
            zip.write_all(redacted.as_bytes())?;
            log_count += 1;
        }
    }

    zip.finish()?;
    tracing::info!(logs = log_count, dest = %dest.display(), "diagnostics bundle written");
    Ok(log_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_token_kv() {
        assert_eq!(
            redact_log_line("hello token=abc123 world"),
            "hello token=[REDACTED] world"
        );
    }

    #[test]
    fn redacts_bearer() {
        assert_eq!(
            redact_log_line("Authorization: Bearer eyJhbGciOi.foo"),
            // auth-headers rule wins (masks the whole header value)
            "Authorization: [REDACTED]"
        );
    }

    #[test]
    fn redacts_every_bearer_on_a_line() {
        // A second token on the same line must not survive (single-match
        // masking would leak `tok2`).
        assert_eq!(
            redact_log_line("retry Bearer tok1 then Bearer tok2 done"),
            "retry Bearer [REDACTED] then Bearer [REDACTED] done"
        );
    }

    #[test]
    fn redacts_header_value_preceding_authorization() {
        // The earliest header on the line anchors the mask; a `cookie` value
        // appearing before `authorization` must not leak (a fixed name-priority
        // scan would mask authorization yet keep the cookie).
        let out = redact_log_line("cookie: sess=SECRET1 authorization: Bearer SECRET2");
        assert!(!out.contains("SECRET1"), "cookie value leaked: {out}");
        assert!(!out.contains("SECRET2"), "auth value leaked: {out}");
        assert!(out.starts_with("cookie: [REDACTED]"), "unexpected: {out}");
    }

    #[test]
    fn redacts_password_case_insensitive() {
        assert_eq!(
            redact_log_line("connecting PASSWORD=hunter2 to db"),
            "connecting PASSWORD=[REDACTED] to db"
        );
    }

    #[test]
    fn redacts_json_token() {
        assert_eq!(
            redact_log_line(r#"{"api_token":"secret","host":"x"}"#),
            r#"{"api_token":[REDACTED],"host":"x"}"#
        );
    }

    #[test]
    fn leaves_non_secret_kv_untouched() {
        let line = "GET /rest/api host=example.atlassian.net user=ada status=200";
        assert_eq!(redact_log_line(line), line);
    }

    #[test]
    fn redacts_password_in_dsn() {
        let out = redact_log_line("connect failed: postgres://cadenza:s3cr3t@db.host:5432/app");
        assert!(!out.contains("s3cr3t"), "DSN password leaked: {out}");
        assert!(
            out.contains("postgres://cadenza:[REDACTED]@db.host:5432/app"),
            "unexpected DSN masking: {out}"
        );
    }

    #[test]
    fn dsn_without_password_is_untouched() {
        // `host:port` after `@` (or no userinfo) must not be mistaken for a
        // password.
        let line = "url=postgres://db.host:5432/app pool=8";
        assert_eq!(redact_log_line(line), line);
    }

    #[test]
    fn bare_value_with_special_chars_does_not_leak_tail() {
        // A password containing `)`/`,` must be masked whole, not truncated.
        let out = redact_log_line("password=p@ss)w,0rd next=ok");
        assert!(!out.contains("w,0rd"), "password tail leaked: {out}");
        assert!(out.contains("next=ok"), "trailing field lost: {out}");
    }

    #[test]
    fn quoted_value_with_escaped_quote_does_not_leak_tail() {
        let out = redact_log_line(r#"token="ab\"cd" host=x"#);
        assert!(!out.contains("cd"), "escaped-quote tail leaked: {out}");
        assert!(out.contains("host=x"), "trailing field lost: {out}");
    }

    #[test]
    fn bearer_substring_in_a_word_is_not_masked() {
        // `bearer` glued into another word is not the auth scheme.
        let line = "loaded forbearer rules and 3 more items";
        assert_eq!(redact_log_line(line), line);
    }

    #[test]
    fn manifest_lists_every_rule() {
        let txt = manifest_text();
        for rule in REDACTION_RULES {
            assert!(
                txt.contains(rule.name),
                "manifest missing rule {}",
                rule.name
            );
        }
    }

    #[test]
    fn env_text_includes_version_and_paths() {
        let txt = env_text(
            "1.2.3",
            7,
            Path::new("/home/u/.cadenza"),
            Path::new("/home/u/.cadenza/logs"),
        );
        assert!(txt.contains("app_version: 1.2.3"));
        assert!(txt.contains("protocol_version: 7"));
        assert!(txt.contains(".cadenza/logs"));
    }

    #[test]
    fn write_bundle_includes_manifest_env_and_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let log_dir = tmp.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("cadenza.2026-06-07.log"),
            "started\ntoken=sekret done\n",
        )
        .unwrap();
        let dest = tmp.path().join("diag.zip");

        let inputs = BundleInputs {
            app_version: "9.9.9",
            protocol_version: 3,
            data_dir: tmp.path().to_path_buf(),
            log_dir: log_dir.clone(),
        };
        let n = write_bundle(&dest, &inputs).unwrap();
        assert_eq!(n, 1);

        // Re-open and verify the redaction landed and the structural files
        // are present.
        let f = std::fs::File::open(&dest).unwrap();
        let mut archive = zip::ZipArchive::new(f).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "manifest.txt"));
        assert!(names.iter().any(|n| n == "env.txt"));
        assert!(names.iter().any(|n| n == "logs/cadenza.2026-06-07.log"));

        use std::io::Read as _;
        let mut log = String::new();
        archive
            .by_name("logs/cadenza.2026-06-07.log")
            .unwrap()
            .read_to_string(&mut log)
            .unwrap();
        assert!(log.contains("token=[REDACTED]"));
        assert!(!log.contains("sekret"));
    }
}
