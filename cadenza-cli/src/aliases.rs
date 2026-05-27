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
}
