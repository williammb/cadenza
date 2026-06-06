//! Analysis-run capability secrets and decomposition validation (Slice 2).
//!
//! An *analysis run* is authorized by a single-use capability secret. The
//! secret is generated once (CSPRNG), returned to the caller exactly once,
//! and only its SHA-256 hash + expiry + lifecycle status are persisted on
//! the `JiraIssueRecord`. The plaintext is NEVER persisted, logged, or
//! returned again.
//!
//! Secret transport (see the materialize op): the plaintext travels via the
//! `$CADENZA_RUN_SECRET` env var (or STDIN) on the CLI side and rides the
//! already-authenticated local socket to the server, which verifies it
//! against the stored hash with a constant-time compare. It never appears
//! in argv or in JSON-on-disk.
//!
//! Slice 2 wires the verify/revoke/validate path (used by `jira_materialize`)
//! and the `create_analysis_run` minting path (used by tests now; the import
//! surface that mints runs in production lands in a later slice). The mint
//! helpers are therefore `#[allow(dead_code)]` until then, mirroring
//! `store::jira_inner`'s Slice-1 allow.
#![allow(dead_code)]

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::Rng;
use sha2::{Digest, Sha256};

use cadenza_proto::ops;

/// Capability-secret time-to-live: 1 hour from mint.
pub const RUN_SECRET_TTL_MS: i64 = 60 * 60 * 1000;

/// Decomposition validation caps.
pub const MAX_SUBTASKS: usize = 50;
pub const MAX_TITLE_LEN: usize = 200;
pub const MAX_BODY_LEN: usize = 8192;

/// Number of CSPRNG bytes in a freshly minted secret (256-bit).
const SECRET_BYTES: usize = 32;

/// Plaintext capability secret. Returned exactly once from
/// `create_analysis_run`. Deliberately has NO derived `Debug`/`Serialize`;
/// the only way to read the value is [`RunSecret::expose`], and its `Debug`
/// impl redacts the value so it can never leak through `{:?}` formatting.
pub struct RunSecret(String);

impl RunSecret {
    /// The plaintext. ONLY accessor — used solely to hand the value to the
    /// immediate caller (which returns it to the operator once).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for RunSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RunSecret(<redacted>)")
    }
}

/// Identity recovered from a successful secret verification.
#[derive(Debug, Clone)]
pub struct VerifiedRun {
    pub jira_site: String,
    pub jira_issue_id: String,
    pub project_id: Option<String>,
}

/// Typed verification failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunSecretError {
    /// No record matched the analysis_run_id, or the record carries no hash.
    NotFound,
    /// Hash mismatch (wrong secret).
    Invalid,
    /// Past `secret_expiry_ms`.
    Expired,
    /// `secret_status == revoked`.
    Revoked,
}

/// Decomposition (subtasks payload) validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecompError {
    Empty,
    TooMany,
    EmptyTitle { index: usize },
    TitleTooLong { index: usize },
    BodyTooLong { index: usize },
    DuplicateTitle { index: usize },
}

impl DecompError {
    /// Human-readable reason string for the `invalid_decomposition` error
    /// body. Logs/messages stay in English.
    pub fn reason(self) -> String {
        match self {
            DecompError::Empty => "subtasks must not be empty".to_string(),
            DecompError::TooMany => format!("too many subtasks (max {MAX_SUBTASKS})"),
            DecompError::EmptyTitle { index } => format!("subtask {index}: title is empty"),
            DecompError::TitleTooLong { index } => {
                format!("subtask {index}: title exceeds {MAX_TITLE_LEN} chars")
            }
            DecompError::BodyTooLong { index } => {
                format!("subtask {index}: body exceeds {MAX_BODY_LEN} chars")
            }
            DecompError::DuplicateTitle { index } => {
                format!("subtask {index}: duplicate title")
            }
        }
    }
}

/// Generate a 256-bit CSPRNG secret, base64url-encoded (no padding).
pub fn generate_secret() -> RunSecret {
    let mut bytes = [0u8; SECRET_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    RunSecret(URL_SAFE_NO_PAD.encode(bytes))
}

/// `"sha256:<hex>"` of the plaintext. The persisted hash format.
pub fn hash_secret(plaintext: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    format!("sha256:{hex}")
}

/// Constant-time compare of two `"sha256:<hex>"` strings. Length-aware (a
/// length difference short-circuits, but the digest portion is always
/// XOR-folded), avoiding a timing oracle on the digest bytes.
pub fn secret_hash_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Validate the decomposition payload. Returns `Err` on the first violation
/// in declaration order: non-empty list; `len <= MAX_SUBTASKS`; each title
/// trimmed-non-empty and `<= MAX_TITLE_LEN` chars; each body
/// `<= MAX_BODY_LEN` chars; no duplicate (trimmed, case-sensitive) titles.
pub fn validate_decomposition(
    subtasks: &[ops::jira_materialize::Subtask],
) -> Result<(), DecompError> {
    if subtasks.is_empty() {
        return Err(DecompError::Empty);
    }
    if subtasks.len() > MAX_SUBTASKS {
        return Err(DecompError::TooMany);
    }
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (index, st) in subtasks.iter().enumerate() {
        let title = st.title.trim();
        if title.is_empty() {
            return Err(DecompError::EmptyTitle { index });
        }
        if title.chars().count() > MAX_TITLE_LEN {
            return Err(DecompError::TitleTooLong { index });
        }
        if st.body.chars().count() > MAX_BODY_LEN {
            return Err(DecompError::BodyTooLong { index });
        }
        if !seen.insert(title) {
            return Err(DecompError::DuplicateTitle { index });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(title: &str, body: &str) -> ops::jira_materialize::Subtask {
        ops::jira_materialize::Subtask {
            title: title.to_string(),
            body: body.to_string(),
        }
    }

    #[test]
    fn generate_secret_is_unique_and_long() {
        let a = generate_secret();
        let b = generate_secret();
        assert_ne!(a.expose(), b.expose());
        // 32 bytes base64url (no pad) ⇒ 43 chars.
        assert!(
            a.expose().len() >= 40,
            "secret too short: {}",
            a.expose().len()
        );
    }

    #[test]
    fn hash_secret_is_sha256_prefixed() {
        let h = hash_secret("hello");
        assert!(h.starts_with("sha256:"));
        // sha256 hex digest is 64 chars after the prefix.
        assert_eq!(h.len(), "sha256:".len() + 64);
        // Stable for the same input.
        assert_eq!(h, hash_secret("hello"));
        assert_ne!(h, hash_secret("hellp"));
    }

    #[test]
    fn secret_hash_eq_matches_and_rejects() {
        let h = hash_secret("s3cr3t");
        assert!(secret_hash_eq(&h, &hash_secret("s3cr3t")));
        assert!(!secret_hash_eq(&h, &hash_secret("other")));
        assert!(!secret_hash_eq(&h, "sha256:00"));
    }

    #[test]
    fn run_secret_debug_is_redacted() {
        let s = generate_secret();
        let dbg = format!("{s:?}");
        assert!(
            !dbg.contains(s.expose()),
            "plaintext leaked in Debug: {dbg}"
        );
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn validate_decomposition_rejects_empty() {
        assert_eq!(validate_decomposition(&[]), Err(DecompError::Empty));
    }

    #[test]
    fn validate_decomposition_rejects_too_many() {
        let many: Vec<_> = (0..(MAX_SUBTASKS + 1))
            .map(|i| st(&format!("t{i}"), "b"))
            .collect();
        assert_eq!(validate_decomposition(&many), Err(DecompError::TooMany));
    }

    #[test]
    fn validate_decomposition_rejects_empty_title() {
        let v = vec![st("ok", "b"), st("   ", "b")];
        assert_eq!(
            validate_decomposition(&v),
            Err(DecompError::EmptyTitle { index: 1 })
        );
    }

    #[test]
    fn validate_decomposition_rejects_oversize_title() {
        let big = "x".repeat(MAX_TITLE_LEN + 1);
        let v = vec![st(&big, "b")];
        assert_eq!(
            validate_decomposition(&v),
            Err(DecompError::TitleTooLong { index: 0 })
        );
    }

    #[test]
    fn validate_decomposition_rejects_oversize_body() {
        let big = "y".repeat(MAX_BODY_LEN + 1);
        let v = vec![st("t", &big)];
        assert_eq!(
            validate_decomposition(&v),
            Err(DecompError::BodyTooLong { index: 0 })
        );
    }

    #[test]
    fn validate_decomposition_rejects_duplicate_titles() {
        let v = vec![st("dup", "b1"), st(" dup ", "b2")];
        assert_eq!(
            validate_decomposition(&v),
            Err(DecompError::DuplicateTitle { index: 1 })
        );
    }

    #[test]
    fn validate_decomposition_accepts_valid() {
        let v = vec![st("a", "b1"), st("b", "b2"), st("c", "")];
        assert_eq!(validate_decomposition(&v), Ok(()));
    }
}
