//! Locale resolution chain: flag → env → config → OS → `en`.
//!
//! Per DESIGN-desktop-v2.md § "Cadeia de resolução do locale".

use crate::{DEFAULT_LOCALE, PRIMARY_LOCALE};

/// Inputs to the resolver. Each layer is optional; the first non-empty
/// hit (after normalization) wins. Callers pass `env` explicitly so the
/// resolver itself is pure and parallel-test-safe.
#[derive(Debug, Default, Clone)]
pub struct LocaleSources<'a> {
    pub flag: Option<&'a str>,
    pub env: Option<&'a str>,
    pub config: Option<&'a str>,
}

/// Read `CADENZA_LANG` from the process environment. Returns `None`
/// when unset or empty.
pub fn read_env() -> Option<String> {
    std::env::var("CADENZA_LANG").ok().filter(|s| !s.is_empty())
}

/// Resolve the active locale.
///
/// Order: `flag` → `env` (`CADENZA_LANG`) → `config` → OS locale → `en`.
/// All inputs are normalized: `pt_BR.UTF-8` → `pt-BR`, `pt_PT` → `pt-BR`
/// (only PT variant we package), `en_US` → `en`. Anything outside the
/// packaged locales falls back to `en`.
pub fn resolve(sources: LocaleSources<'_>) -> String {
    if let Some(s) = sources.flag.filter(|s| !s.is_empty()) {
        return normalize(s);
    }
    if let Some(s) = sources.env.filter(|s| !s.is_empty()) {
        return normalize(s);
    }
    if let Some(s) = sources.config.filter(|s| !s.is_empty()) {
        return normalize(s);
    }
    if let Some(s) = sys_locale::get_locale() {
        if !s.is_empty() {
            return normalize(&s);
        }
    }
    DEFAULT_LOCALE.to_string()
}

/// Normalize an OS or user-supplied locale string into one of the
/// packaged locales. Unknown locales fall back to `en`.
pub fn normalize(input: &str) -> String {
    let stripped = input.split('.').next().unwrap_or(input);
    let dashed = stripped.replace('_', "-");
    let lower = dashed.to_ascii_lowercase();

    if lower.starts_with("pt") {
        PRIMARY_LOCALE.to_string()
    } else if lower.starts_with("en") {
        DEFAULT_LOCALE.to_string()
    } else {
        DEFAULT_LOCALE.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_posix_pt_br() {
        assert_eq!(normalize("pt_BR.UTF-8"), "pt-BR");
        assert_eq!(normalize("pt_BR"), "pt-BR");
        assert_eq!(normalize("pt-br"), "pt-BR");
    }

    #[test]
    fn pt_pt_falls_through_to_pt_br() {
        assert_eq!(normalize("pt_PT"), "pt-BR");
        assert_eq!(normalize("pt-PT"), "pt-BR");
    }

    #[test]
    fn normalizes_en_variants() {
        assert_eq!(normalize("en_US.UTF-8"), "en");
        assert_eq!(normalize("en-GB"), "en");
        assert_eq!(normalize("EN"), "en");
    }

    #[test]
    fn unknown_locale_falls_back_to_en() {
        assert_eq!(normalize("ja_JP"), "en");
        assert_eq!(normalize("es-ES"), "en");
        assert_eq!(normalize(""), "en");
    }

    #[test]
    fn flag_wins_over_env() {
        let r = resolve(LocaleSources {
            flag: Some("pt-BR"),
            env: Some("en"),
            config: None,
        });
        assert_eq!(r, "pt-BR");
    }

    #[test]
    fn env_wins_over_config() {
        let r = resolve(LocaleSources {
            flag: None,
            env: Some("pt-BR"),
            config: Some("en"),
        });
        assert_eq!(r, "pt-BR");
    }

    #[test]
    fn config_used_when_no_flag_no_env() {
        let r = resolve(LocaleSources {
            flag: None,
            env: None,
            config: Some("en"),
        });
        assert_eq!(r, "en");
    }

    #[test]
    fn empty_strings_treated_as_absent() {
        let r = resolve(LocaleSources {
            flag: Some(""),
            env: Some(""),
            config: Some("pt-BR"),
        });
        assert_eq!(r, "pt-BR");
    }

    #[test]
    fn all_none_resolves_to_supported_locale() {
        // All explicit sources absent: falls through to OS locale then "en".
        let r = resolve(LocaleSources::default());
        assert!(r == "pt-BR" || r == "en", "unexpected locale: {r}");
    }
}
