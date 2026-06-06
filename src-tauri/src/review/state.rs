//! Pure `evidence_state` derivation (PLAN §C.11.d).
//!
//! This is a deterministic function of: the resolved contract, the
//! agent-reported checks, the changed-file set, and whether that set is
//! trustworthy enough to evaluate conditional (`required_if_changed`)
//! checks. It NEVER runs git and NEVER touches I/O — every git result is
//! pre-computed by the orchestrator and passed in.

use crate::config::QualityProfile;
use crate::review::{EvidenceState, RiskFlag};
use cadenza_proto::ops::done::Evidence;
use globset::{Glob, GlobSetBuilder};

/// Inputs to [`derive`].
pub(crate) struct StateInputs<'a> {
    /// The live contract, when the project resolved. `None` here with
    /// `contract_resolved == false` ⇒ `contract_unavailable`.
    pub contract: Option<&'a QualityProfile>,
    /// Whether the project (and thus its contract) was resolvable.
    pub contract_resolved: bool,
    /// Live contract hash (for drift comparison). `None` when no profile.
    pub current_contract_version: Option<String>,
    /// Agent-reported evidence (untrusted).
    pub reported: &'a Evidence,
    /// Changed-file paths; used to evaluate `required_if_changed` globs.
    /// `None` when the scope is unknown (missing worktree / unresolved).
    pub changed_paths: Option<&'a [String]>,
    /// False when the changed set is unavailable or truncated beyond trust
    /// (missing worktree, base_unresolved, diff_unavailable, truncated).
    pub scope_trustworthy: bool,
    /// Risk flags that fired (drives the focused-human-review overlay).
    pub risks: &'a [RiskFlag],
}

/// Output of [`derive`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StateOutcome {
    pub state: EvidenceState,
    pub validation_scope_unknown: bool,
    pub needs_focused_human_review: bool,
}

/// Whether any `required_if_changed` glob matches any changed path.
fn conditional_triggered(patterns: &[String], changed: &[String]) -> bool {
    if patterns.is_empty() || changed.is_empty() {
        return false;
    }
    let mut b = GlobSetBuilder::new();
    let mut any = false;
    for p in patterns {
        if let Ok(g) = Glob::new(p) {
            b.add(g);
            any = true;
        }
    }
    if !any {
        return false;
    }
    let Ok(set) = b.build() else { return false };
    changed.iter().any(|c| set.is_match(c))
}

/// Derive the evidence state (PLAN §C.11.d). See the module docs.
pub(crate) fn derive(inputs: &StateInputs) -> StateOutcome {
    let needs_focused_human_review = inputs.risks.iter().any(|r| {
        matches!(
            r,
            RiskFlag::Auth
                | RiskFlag::Migration
                | RiskFlag::PublicContract
                | RiskFlag::PossibleSecret
        )
    });

    // 1. Project unresolved ⇒ contract_unavailable.
    if !inputs.contract_resolved {
        return StateOutcome {
            state: EvidenceState::ContractUnavailable,
            validation_scope_unknown: false,
            needs_focused_human_review,
        };
    }

    // 2. Drift: reported contract_version ≠ current ⇒ contract_changed.
    //    Only meaningful when the agent reported one. (Absent reported
    //    version is not drift — falls through to the no_validation /
    //    pass/fail path.)
    if let Some(reported_ver) = inputs.reported.contract_version.as_deref() {
        let current = inputs.current_contract_version.as_deref();
        if Some(reported_ver) != current {
            return StateOutcome {
                state: EvidenceState::ContractChanged,
                validation_scope_unknown: false,
                needs_focused_human_review,
            };
        }
    }

    // 3. No checks defined in the contract ⇒ no_validation.
    let checks = match inputs.contract {
        Some(c) if !c.checks.is_empty() => &c.checks,
        _ => {
            return StateOutcome {
                state: EvidenceState::NoValidation,
                validation_scope_unknown: false,
                needs_focused_human_review,
            };
        }
    };

    // Index reported checks by id for exit-code lookup.
    let reported_exit = |id: &str| -> Option<i32> {
        inputs
            .reported
            .checks
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.exit)
    };

    // Determine the required set + conditional-scope guard.
    let mut validation_scope_unknown = false;
    let mut any_required = false;
    let mut any_required_missing = false;
    let mut any_required_failed = false;

    for chk in checks {
        let mut required = chk.required;
        if !chk.required_if_changed.is_empty() {
            if inputs.scope_trustworthy {
                if let Some(changed) = inputs.changed_paths {
                    if conditional_triggered(&chk.required_if_changed, changed) {
                        required = true;
                    }
                }
            } else {
                // CONDITIONAL-CHECK GUARD: scope untrustworthy ⇒ we cannot
                // know whether this check was owed. Treat as required and
                // unknown (not satisfied); the state can never be `passed`.
                required = true;
                validation_scope_unknown = true;
            }
        }
        if required {
            any_required = true;
            match reported_exit(&chk.id) {
                None => any_required_missing = true,
                Some(0) => {}
                Some(_) => any_required_failed = true,
            }
        }
    }

    let state = if !any_required {
        // No required checks owed → having any reported checks is enough to
        // be "passed"; otherwise no_validation. A failing non-required
        // check does not fail the state (only required checks gate).
        if inputs.reported.checks.is_empty() {
            EvidenceState::NoValidation
        } else if inputs.reported.checks.iter().any(|c| c.exit != 0) {
            // Reported a failing (optional) check: surface as partial so it
            // is not silently "passed".
            EvidenceState::Partial
        } else {
            EvidenceState::Passed
        }
    } else if any_required_failed {
        EvidenceState::Failed
    } else if any_required_missing || validation_scope_unknown {
        EvidenceState::Partial
    } else {
        EvidenceState::Passed
    };

    StateOutcome {
        state,
        validation_scope_unknown,
        needs_focused_human_review,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QualityCheck;
    use cadenza_proto::ops::done::EvidenceCheck;

    fn check(id: &str, required: bool, cond: &[&str]) -> QualityCheck {
        QualityCheck {
            id: id.into(),
            name: id.into(),
            cmd: format!("run {id}"),
            required,
            required_if_changed: cond.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn profile(checks: Vec<QualityCheck>) -> QualityProfile {
        QualityProfile { checks }
    }

    fn evidence(ver: Option<&str>, checks: &[(&str, i32)]) -> Evidence {
        Evidence {
            contract_version: ver.map(|s| s.to_string()),
            checks: checks
                .iter()
                .map(|(id, exit)| EvidenceCheck {
                    id: id.to_string(),
                    exit: *exit,
                    log_excerpt: String::new(),
                    log_path: None,
                })
                .collect(),
            groups: vec![],
            open_questions: vec![],
        }
    }

    fn inputs<'a>(
        contract: Option<&'a QualityProfile>,
        resolved: bool,
        cur_ver: Option<String>,
        reported: &'a Evidence,
        changed: Option<&'a [String]>,
        trustworthy: bool,
        risks: &'a [RiskFlag],
    ) -> StateInputs<'a> {
        StateInputs {
            contract,
            contract_resolved: resolved,
            current_contract_version: cur_ver,
            reported,
            changed_paths: changed,
            scope_trustworthy: trustworthy,
            risks,
        }
    }

    #[test]
    fn contract_unavailable_when_project_unresolved() {
        let ev = evidence(None, &[]);
        let out = derive(&inputs(None, false, None, &ev, None, true, &[]));
        assert_eq!(out.state, EvidenceState::ContractUnavailable);
    }

    #[test]
    fn contract_changed_on_drift() {
        let p = profile(vec![check("a", true, &[])]);
        let ev = evidence(Some("sha256:old"), &[("a", 0)]);
        let out = derive(&inputs(
            Some(&p),
            true,
            Some("sha256:new".into()),
            &ev,
            Some(&[]),
            true,
            &[],
        ));
        assert_eq!(out.state, EvidenceState::ContractChanged);
    }

    #[test]
    fn no_validation_when_empty_profile() {
        let p = profile(vec![]);
        let ev = evidence(None, &[]);
        let out = derive(&inputs(Some(&p), true, None, &ev, Some(&[]), true, &[]));
        assert_eq!(out.state, EvidenceState::NoValidation);
    }

    #[test]
    fn passed_when_all_required_zero() {
        let p = profile(vec![check("a", true, &[])]);
        let cv = p.contract_version();
        let ev = evidence(Some(&cv), &[("a", 0)]);
        let out = derive(&inputs(Some(&p), true, Some(cv), &ev, Some(&[]), true, &[]));
        assert_eq!(out.state, EvidenceState::Passed);
    }

    #[test]
    fn failed_when_required_nonzero() {
        let p = profile(vec![check("a", true, &[])]);
        let cv = p.contract_version();
        let ev = evidence(Some(&cv), &[("a", 1)]);
        let out = derive(&inputs(Some(&p), true, Some(cv), &ev, Some(&[]), true, &[]));
        assert_eq!(out.state, EvidenceState::Failed);
    }

    #[test]
    fn partial_when_required_missing() {
        let p = profile(vec![check("a", true, &[]), check("b", true, &[])]);
        let cv = p.contract_version();
        let ev = evidence(Some(&cv), &[("a", 0)]); // b missing
        let out = derive(&inputs(Some(&p), true, Some(cv), &ev, Some(&[]), true, &[]));
        assert_eq!(out.state, EvidenceState::Partial);
    }

    #[test]
    fn conditional_required_when_path_matches() {
        let p = profile(vec![check("sql", false, &["**/*.sql"])]);
        let cv = p.contract_version();
        let ev = evidence(Some(&cv), &[]); // sql not reported
        let changed = vec!["migrations/001.sql".to_string()];
        let out = derive(&inputs(
            Some(&p),
            true,
            Some(cv),
            &ev,
            Some(&changed),
            true,
            &[],
        ));
        // sql became required (path matched) but missing → partial.
        assert_eq!(out.state, EvidenceState::Partial);
        assert!(!out.validation_scope_unknown);
    }

    #[test]
    fn scope_unknown_degrades_passed_to_partial() {
        // A conditional check, all unconditional checks pass, but scope is
        // untrustworthy → cannot be passed; degrades to partial + flag.
        let p = profile(vec![
            check("a", true, &[]),
            check("sql", false, &["**/*.sql"]),
        ]);
        let cv = p.contract_version();
        let ev = evidence(Some(&cv), &[("a", 0)]);
        let out = derive(&inputs(
            Some(&p),
            true,
            Some(cv),
            &ev,
            None,  // scope unknown
            false, // untrustworthy
            &[],
        ));
        assert_eq!(out.state, EvidenceState::Partial);
        assert!(out.validation_scope_unknown);
    }

    #[test]
    fn focused_review_overlay_on_auth_risk() {
        let p = profile(vec![]);
        let ev = evidence(None, &[]);
        let out = derive(&inputs(
            Some(&p),
            true,
            None,
            &ev,
            Some(&[]),
            true,
            &[RiskFlag::Auth],
        ));
        assert!(out.needs_focused_human_review);
        // Overlay does not change the base state.
        assert_eq!(out.state, EvidenceState::NoValidation);
    }

    #[test]
    fn large_file_risk_does_not_set_overlay() {
        let p = profile(vec![]);
        let ev = evidence(None, &[]);
        let out = derive(&inputs(
            Some(&p),
            true,
            None,
            &ev,
            Some(&[]),
            true,
            &[RiskFlag::LargeFile, RiskFlag::NewDependency],
        ));
        assert!(!out.needs_focused_human_review);
    }
}
