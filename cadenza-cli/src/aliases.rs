//! EN aliases → PT canonical state mapping.
//!
//! Per DESIGN-desktop-v2.md § "CLI — argumentos bilíngues":
//! `--estado` accepts EN aliases mapped to PT canonical on disk.
//! `--json` output always emits PT canonical for parsing stability.
//!
//! Wired into clap value-parsing in Phase 4; allow dead_code until then.
#![allow(dead_code)]

/// Canonical PT state values used on disk.
pub const ESTADOS: &[&str] = &["a_fazer", "fazendo", "aguardando_revisao", "feito"];

/// Resolve an EN alias or pass through PT canonical value.
pub fn canonicalize(input: &str) -> Option<&'static str> {
    match input {
        // PT canonical (pass-through)
        "a_fazer" => Some("a_fazer"),
        "fazendo" => Some("fazendo"),
        "aguardando_revisao" => Some("aguardando_revisao"),
        "feito" => Some("feito"),
        // EN aliases
        "todo" => Some("a_fazer"),
        "doing" => Some("fazendo"),
        "review" => Some("aguardando_revisao"),
        "done" => Some("feito"),
        _ => None,
    }
}

/// Return the EN display alias for a PT canonical state.
pub fn display_en(estado: &str) -> Option<&'static str> {
    match estado {
        "a_fazer" => Some("todo"),
        "fazendo" => Some("doing"),
        "aguardando_revisao" => Some("review"),
        "feito" => Some("done"),
        _ => None,
    }
}

/// Validate a client-generated idempotency key.
///
/// This is the **identical** rule the app enforces in
/// `src-tauri/src/store/mod.rs::validate_idempotency_key` (PLAN §B.6 requires
/// both sides agree). Kept duplicated here because `cadenza-cli` cannot depend
/// on the `src-tauri` crate, and the rule is tiny and stable. Any change here
/// MUST mirror the app-side function.
///
/// Rules: non-empty, ≤ 128 bytes, charset `[A-Za-z0-9._-]`, not `.`/`..`, and
/// no Windows-reserved device stem (CON, PRN, AUX, NUL, COM1..9, LPT1..9). The
/// key is used as a filesystem-safe component app-side, hence the restrictions.
pub fn validate_idempotency_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("idempotency_key must not be empty".into());
    }
    if key.len() > 128 {
        return Err(format!("idempotency_key too long: {} bytes", key.len()));
    }
    if !key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(format!("idempotency_key has invalid characters: {key}"));
    }
    if key == ".." || key == "." {
        return Err(format!("idempotency_key has invalid value: {key}"));
    }
    let upper = key.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.len() == 4
            && matches!(upper.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        return Err(format!("idempotency_key is a reserved device name: {key}"));
    }
    Ok(())
}

/// Local evidence.json caps (PLAN §B.7). The app re-enforces these as defense
/// in depth (`review::caps`), but the CLI validates first so a malformed file
/// fails fast as a usage error (exit 2) without a round-trip.
pub mod evidence_caps {
    /// Whole evidence.json file size on disk.
    pub const MAX_FILE_BYTES: u64 = 256 * 1024;
    /// Maximum number of reported checks.
    pub const MAX_CHECKS: usize = 64;
    /// Maximum number of intent groups.
    pub const MAX_GROUPS: usize = 64;
    /// Per-string cap for labels, file paths, and open questions.
    pub const MAX_STRING_BYTES: usize = 1024;
    /// Per-check log excerpt cap.
    pub const MAX_LOG_EXCERPT_BYTES: usize = 8 * 1024;
}

/// Validate a parsed evidence payload against the PLAN §B.7 caps. Returns an
/// error string (CLI maps to exit 2) on the first violation. The file-size cap
/// is checked separately by the caller before parsing.
pub fn validate_evidence(ev: &cadenza_proto::ops::done::Evidence) -> Result<(), String> {
    use evidence_caps::*;

    let check_string = |what: &str, s: &str| -> Result<(), String> {
        if s.len() > MAX_STRING_BYTES {
            return Err(format!(
                "{what} too long: {} bytes (max {MAX_STRING_BYTES})",
                s.len()
            ));
        }
        Ok(())
    };

    if ev.checks.len() > MAX_CHECKS {
        return Err(format!(
            "too many checks: {} (max {MAX_CHECKS})",
            ev.checks.len()
        ));
    }
    if ev.groups.len() > MAX_GROUPS {
        return Err(format!(
            "too many groups: {} (max {MAX_GROUPS})",
            ev.groups.len()
        ));
    }
    if let Some(cv) = ev.contract_version.as_deref() {
        check_string("contract_version", cv)?;
    }
    for c in &ev.checks {
        if c.id.is_empty() {
            return Err("check id must not be empty".into());
        }
        check_string("check id", &c.id)?;
        if c.log_excerpt.len() > MAX_LOG_EXCERPT_BYTES {
            return Err(format!(
                "check '{}' log_excerpt too long: {} bytes (max {MAX_LOG_EXCERPT_BYTES})",
                c.id,
                c.log_excerpt.len()
            ));
        }
        if let Some(p) = c.log_path.as_deref() {
            check_string("check log_path", p)?;
        }
    }
    for g in &ev.groups {
        if g.label.is_empty() {
            return Err("group label must not be empty".into());
        }
        check_string("group label", &g.label)?;
        for f in &g.files {
            check_string("group file path", f)?;
        }
    }
    for q in &ev.open_questions {
        check_string("open_question", q)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn en_aliases_map_to_pt_canonical() {
        assert_eq!(canonicalize("todo"), Some("a_fazer"));
        assert_eq!(canonicalize("doing"), Some("fazendo"));
        assert_eq!(canonicalize("review"), Some("aguardando_revisao"));
        assert_eq!(canonicalize("done"), Some("feito"));
    }

    #[test]
    fn pt_canonical_passes_through() {
        for &e in ESTADOS {
            assert_eq!(canonicalize(e), Some(e));
        }
    }

    #[test]
    fn unknown_returns_none() {
        assert_eq!(canonicalize("WIP"), None);
        assert_eq!(canonicalize(""), None);
    }

    #[test]
    fn pt_canonical_to_en_display() {
        assert_eq!(display_en("a_fazer"), Some("todo"));
        assert_eq!(display_en("fazendo"), Some("doing"));
        assert_eq!(display_en("aguardando_revisao"), Some("review"));
        assert_eq!(display_en("feito"), Some("done"));
        assert_eq!(display_en("unknown"), None);
        assert_eq!(display_en(""), None);
    }

    #[test]
    fn canonicalize_and_display_en_are_inverses() {
        for &en_alias in &["todo", "doing", "review", "done"] {
            let pt = canonicalize(en_alias).unwrap();
            assert_eq!(display_en(pt), Some(en_alias));
        }
    }

    #[test]
    fn idempotency_key_accepts_valid() {
        for k in ["abc", "task-1.attempt_2", "A1-b2.c3", &"x".repeat(128)] {
            assert!(validate_idempotency_key(k).is_ok(), "rejected: {k}");
        }
    }

    #[test]
    fn idempotency_key_rejects_invalid() {
        for k in [
            "",                 // empty
            &"y".repeat(129),   // too long
            "has space",        // space
            "path/sep",         // slash
            "back\\slash",      // backslash
            "ctrl\u{0007}char", // control char
            ".",                // dot
            "..",               // dotdot
            "CON",              // reserved
            "nul",              // reserved (case-insensitive)
            "COM1",             // reserved device
            "LPT9",             // reserved device
        ] {
            assert!(validate_idempotency_key(k).is_err(), "accepted: {k:?}");
        }
    }

    /// COM10 / LPT10 etc. are NOT reserved (only single-digit 1-9 stems).
    #[test]
    fn idempotency_key_allows_non_reserved_lookalikes() {
        assert!(validate_idempotency_key("COM10").is_ok());
        assert!(validate_idempotency_key("COM0").is_ok());
        assert!(validate_idempotency_key("CONSOLE").is_ok());
    }

    fn check(id: &str, excerpt: &str) -> cadenza_proto::ops::done::EvidenceCheck {
        cadenza_proto::ops::done::EvidenceCheck {
            id: id.into(),
            exit: 0,
            log_excerpt: excerpt.into(),
            log_path: None,
        }
    }

    #[test]
    fn evidence_accepts_within_caps() {
        let ev = cadenza_proto::ops::done::Evidence {
            contract_version: Some("sha256:ab".into()),
            checks: vec![check("clippy", "ok")],
            groups: vec![cadenza_proto::ops::done::EvidenceGroup {
                label: "core".into(),
                files: vec!["src/lib.rs".into()],
            }],
            open_questions: vec!["why?".into()],
        };
        assert!(validate_evidence(&ev).is_ok());
    }

    #[test]
    fn evidence_rejects_too_many_checks() {
        let ev = cadenza_proto::ops::done::Evidence {
            checks: (0..evidence_caps::MAX_CHECKS + 1)
                .map(|i| check(&format!("c{i}"), ""))
                .collect(),
            ..Default::default()
        };
        assert!(validate_evidence(&ev).is_err());
    }

    #[test]
    fn evidence_rejects_too_many_groups() {
        let ev = cadenza_proto::ops::done::Evidence {
            groups: (0..evidence_caps::MAX_GROUPS + 1)
                .map(|i| cadenza_proto::ops::done::EvidenceGroup {
                    label: format!("g{i}"),
                    files: vec![],
                })
                .collect(),
            ..Default::default()
        };
        assert!(validate_evidence(&ev).is_err());
    }

    #[test]
    fn evidence_rejects_oversized_log_excerpt() {
        let ev = cadenza_proto::ops::done::Evidence {
            checks: vec![check(
                "big",
                &"x".repeat(evidence_caps::MAX_LOG_EXCERPT_BYTES + 1),
            )],
            ..Default::default()
        };
        assert!(validate_evidence(&ev).is_err());
    }

    #[test]
    fn evidence_rejects_oversized_string() {
        let ev = cadenza_proto::ops::done::Evidence {
            open_questions: vec!["q".repeat(evidence_caps::MAX_STRING_BYTES + 1)],
            ..Default::default()
        };
        assert!(validate_evidence(&ev).is_err());
    }

    #[test]
    fn evidence_rejects_empty_check_id() {
        let ev = cadenza_proto::ops::done::Evidence {
            checks: vec![check("", "ok")],
            ..Default::default()
        };
        assert!(validate_evidence(&ev).is_err());
    }
}
