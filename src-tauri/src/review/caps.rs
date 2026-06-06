//! App-side evidence validation + capping (PLAN §B.7, §C.10).
//!
//! Defense in depth: any socket client can call `done`, so the same caps
//! the CLI enforces locally are re-enforced here before any task state is
//! mutated. Malformed or over-cap evidence is rejected with [`CapError`]
//! (surfaced as `bad_args` / CLI exit 2) and produces NO partial done.
//!
//! Caps mirror the agent-JSON limits in the wire contract:
//! - ≤ [`MAX_CHECKS`] checks, ≤ [`MAX_GROUPS`] groups, ≤ [`MAX_OPEN_QUESTIONS`]
//!   open questions,
//! - each label / file path / open question ≤ [`MAX_STRING`] bytes,
//! - each `log_excerpt` ≤ [`MAX_LOG_EXCERPT`] bytes,
//! - each check `id` ≤ [`MAX_STRING`] bytes and non-empty,
//! - the whole serialized evidence ≤ [`MAX_TOTAL`] bytes.
//!
//! These are the **same** numeric limits the CLI applies; keeping one home
//! (this module) for the app side means the two can never silently drift.

use cadenza_proto::ops::done::Evidence;

/// ≤ 64 checks (PLAN §B.7).
pub const MAX_CHECKS: usize = 64;
/// ≤ 64 groups (PLAN §B.7).
pub const MAX_GROUPS: usize = 64;
/// ≤ 64 open questions — same order of magnitude as groups/checks.
pub const MAX_OPEN_QUESTIONS: usize = 64;
/// Per-string cap for labels / file paths / open questions / ids: 1 KiB.
pub const MAX_STRING: usize = 1024;
/// Per-check `log_excerpt` cap: 8 KiB.
pub const MAX_LOG_EXCERPT: usize = 8 * 1024;
/// Whole serialized evidence cap: 256 KiB (keeps the `done` frame under the
/// IPC `MAX_LINE_BYTES` of 1 MiB).
pub const MAX_TOTAL: usize = 256 * 1024;

/// A cap/schema violation. The wire layer maps this to `bad_args` and
/// performs no task mutation (PLAN §C.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapError(pub String);

impl std::fmt::Display for CapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for CapError {}

/// Re-validate + re-cap agent-reported evidence app-side (PLAN §C.10).
///
/// Returns the evidence unchanged when it is within every cap, or
/// [`CapError`] on the first violation. This is a *reject*, not a *truncate*:
/// silently trimming an over-cap excerpt would change the persisted snapshot
/// behind the agent's back, so an over-cap payload is refused outright and
/// the `done` leaves task state untouched.
pub fn validate_and_cap_evidence(evidence: Evidence) -> Result<Evidence, CapError> {
    // Whole-payload size first — cheapest rejection of a runaway blob.
    let total = serde_json::to_vec(&evidence)
        .map(|v| v.len())
        .unwrap_or(usize::MAX);
    if total > MAX_TOTAL {
        return Err(CapError(format!(
            "evidence too large: {total} bytes (cap {MAX_TOTAL})"
        )));
    }

    if evidence.checks.len() > MAX_CHECKS {
        return Err(CapError(format!(
            "too many checks: {} (cap {MAX_CHECKS})",
            evidence.checks.len()
        )));
    }
    if evidence.groups.len() > MAX_GROUPS {
        return Err(CapError(format!(
            "too many groups: {} (cap {MAX_GROUPS})",
            evidence.groups.len()
        )));
    }
    if evidence.open_questions.len() > MAX_OPEN_QUESTIONS {
        return Err(CapError(format!(
            "too many open_questions: {} (cap {MAX_OPEN_QUESTIONS})",
            evidence.open_questions.len()
        )));
    }

    for c in &evidence.checks {
        if c.id.trim().is_empty() {
            return Err(CapError("check id must not be empty".into()));
        }
        check_string("check id", &c.id)?;
        if c.log_excerpt.len() > MAX_LOG_EXCERPT {
            return Err(CapError(format!(
                "check '{}' log_excerpt too large: {} bytes (cap {MAX_LOG_EXCERPT})",
                c.id,
                c.log_excerpt.len()
            )));
        }
        if let Some(p) = &c.log_path {
            check_string("check log_path", p)?;
        }
    }

    for g in &evidence.groups {
        check_string("group label", &g.label)?;
        for f in &g.files {
            check_string("group file", f)?;
        }
    }

    for q in &evidence.open_questions {
        check_string("open_question", q)?;
    }

    Ok(evidence)
}

fn check_string(field: &str, value: &str) -> Result<(), CapError> {
    if value.len() > MAX_STRING {
        return Err(CapError(format!(
            "{field} too long: {} bytes (cap {MAX_STRING})",
            value.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadenza_proto::ops::done::{EvidenceCheck, EvidenceGroup};

    fn ok_check() -> EvidenceCheck {
        EvidenceCheck {
            id: "clippy".into(),
            exit: 0,
            log_excerpt: "ok".into(),
            log_path: None,
        }
    }

    #[test]
    fn accepts_well_formed_evidence() {
        let e = Evidence {
            contract_version: Some("sha256:00".into()),
            checks: vec![ok_check()],
            groups: vec![EvidenceGroup {
                label: "core".into(),
                files: vec!["src/lib.rs".into()],
            }],
            open_questions: vec!["why?".into()],
        };
        assert!(validate_and_cap_evidence(e).is_ok());
    }

    #[test]
    fn rejects_too_many_checks() {
        let e = Evidence {
            checks: vec![ok_check(); MAX_CHECKS + 1],
            ..Default::default()
        };
        assert!(validate_and_cap_evidence(e).is_err());
    }

    #[test]
    fn rejects_oversize_log_excerpt() {
        let mut c = ok_check();
        c.log_excerpt = "x".repeat(MAX_LOG_EXCERPT + 1);
        let e = Evidence {
            checks: vec![c],
            ..Default::default()
        };
        assert!(validate_and_cap_evidence(e).is_err());
    }

    #[test]
    fn rejects_empty_check_id() {
        let mut c = ok_check();
        c.id = "   ".into();
        let e = Evidence {
            checks: vec![c],
            ..Default::default()
        };
        assert!(validate_and_cap_evidence(e).is_err());
    }

    #[test]
    fn rejects_oversize_group_label() {
        let e = Evidence {
            groups: vec![EvidenceGroup {
                label: "x".repeat(MAX_STRING + 1),
                files: vec![],
            }],
            ..Default::default()
        };
        assert!(validate_and_cap_evidence(e).is_err());
    }
}
