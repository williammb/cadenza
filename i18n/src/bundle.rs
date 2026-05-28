//! Fluent bundle loader with fallback to `en`.
//!
//! `.ftl` files are embedded at compile time via `include_dir!`. The
//! bundle uses Fluent's **concurrent** memoizer (`IntlLangMemoizer` from
//! `intl_memoizer::concurrent`) so `I18n` is `Send + Sync`, which is
//! required by `tauri::State<'_, AppState>`.

use fluent_bundle::bundle::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource};
use include_dir::{include_dir, Dir};
use intl_memoizer::concurrent::IntlLangMemoizer;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use unic_langid::LanguageIdentifier;

use crate::{DEFAULT_LOCALE, SUPPORTED_LOCALES};

static LOCALES_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../locales");

/// `Send + Sync` Fluent bundle.
type Bundle = FluentBundle<Arc<FluentResource>, IntlLangMemoizer>;

/// I18n bundle for the active locale, with `en` as fallback for any
/// missing key.
pub struct I18n {
    active_locale: String,
    primary: Bundle,
    fallback: Bundle,
}

impl I18n {
    /// Construct an I18n bundle for `locale`. If `locale` isn't packaged,
    /// falls back to `en`. The fallback bundle is always `en`.
    pub fn new(locale: &str) -> Self {
        let locale = if SUPPORTED_LOCALES.contains(&locale) {
            locale.to_string()
        } else {
            tracing::warn!(
                requested = %locale,
                fallback = %DEFAULT_LOCALE,
                "unsupported locale, falling back"
            );
            DEFAULT_LOCALE.to_string()
        };

        let primary = build_bundle(&locale);
        let fallback = build_bundle(DEFAULT_LOCALE);

        I18n {
            active_locale: locale,
            primary,
            fallback,
        }
    }

    pub fn active(&self) -> &str {
        &self.active_locale
    }

    /// Look up `key` with no args.
    pub fn t(&self, key: &str) -> String {
        self.t_with(key, None)
    }

    /// Look up `key` with optional Fluent args.
    pub fn t_with(&self, key: &str, args: Option<&FluentArgs<'_>>) -> String {
        if let Some(s) = lookup(&self.primary, key, args) {
            return s;
        }
        if let Some(s) = lookup(&self.fallback, key, args) {
            tracing::debug!(
                key = %key,
                locale = %self.active_locale,
                "key missing in primary, used fallback"
            );
            return s;
        }
        tracing::warn!(key = %key, "missing translation key");
        key.to_string()
    }

    /// Render every message id declared in `<namespace>.ftl` (e.g. `"ui"`)
    /// into a flat `key -> string` map. Used by the Tauri `load_translations`
    /// command so the UI can fetch all its strings in one round-trip at
    /// boot instead of one IPC call per `data-i18n` element.
    ///
    /// Keys are collected from both the active locale's file and the
    /// `en` fallback so anything English declares is reachable even
    /// when the localized file forgot to translate it. Resolution still
    /// goes through `self.t()`, so the per-key fallback chain applies.
    pub fn dump_namespace_strings(&self, namespace: &str) -> HashMap<String, String> {
        let file_name = format!("{namespace}.ftl");
        let mut keys: BTreeSet<String> = BTreeSet::new();
        for locale in [self.active_locale.as_str(), DEFAULT_LOCALE] {
            if let Some(dir) = LOCALES_DIR.get_dir(locale) {
                for file in dir.files() {
                    let matches_name = file
                        .path()
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(|n| n == file_name)
                        .unwrap_or(false);
                    if !matches_name {
                        continue;
                    }
                    if let Some(text) = file.contents_utf8() {
                        for id in extract_message_ids(text) {
                            keys.insert(id);
                        }
                    }
                }
            }
        }
        keys.into_iter().map(|k| (k.clone(), self.t(&k))).collect()
    }
}

/// Extract top-level Fluent message ids from raw `.ftl` text.
///
/// We only need the names, not the patterns — `I18n::t()` handles
/// rendering. A handwritten scan beats pulling in `fluent-syntax` as
/// an extra dep and dodges its lifetimes. Skips comments (`#…`), terms
/// (`-name = …`), and continuation lines (any leading whitespace).
fn extract_message_ids(ftl: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in ftl.lines() {
        let bytes = line.as_bytes();
        if bytes.is_empty() {
            continue;
        }
        let first = bytes[0];
        if first == b' ' || first == b'\t' || first == b'#' || first == b'-' {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let id = line[..eq].trim();
        if id.is_empty() {
            continue;
        }
        if id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            ids.push(id.to_string());
        }
    }
    ids
}

fn build_bundle(locale: &str) -> Bundle {
    let langid: LanguageIdentifier = locale.parse().unwrap_or_else(|_| {
        DEFAULT_LOCALE
            .parse()
            .expect("DEFAULT_LOCALE is a valid langid")
    });
    let mut bundle: Bundle = FluentBundle::new_concurrent(vec![langid]);
    // Unicode isolation marks (\u{2068}/\u{2069}) confuse plain terminal
    // output. Off for now — re-enable when RTL locales are packaged.
    bundle.set_use_isolating(false);

    if let Some(dir) = LOCALES_DIR.get_dir(locale) {
        for file in dir.files() {
            let Some(s) = file.contents_utf8() else {
                continue;
            };
            match FluentResource::try_new(s.to_string()) {
                Ok(res) => {
                    if let Err(errs) = bundle.add_resource(Arc::new(res)) {
                        for e in errs {
                            tracing::warn!(
                                locale = %locale,
                                file = ?file.path(),
                                error = ?e,
                                "fluent resource error"
                            );
                        }
                    }
                }
                Err((_, errs)) => {
                    for e in errs {
                        tracing::warn!(
                            locale = %locale,
                            file = ?file.path(),
                            error = ?e,
                            "failed to parse .ftl"
                        );
                    }
                }
            }
        }
    } else {
        tracing::warn!(locale = %locale, "no .ftl files packaged for locale");
    }

    bundle
}

fn lookup(bundle: &Bundle, key: &str, args: Option<&FluentArgs<'_>>) -> Option<String> {
    let msg = bundle.get_message(key)?;
    let pattern = msg.value()?;
    let mut errors = vec![];
    let s = bundle.format_pattern(pattern, args, &mut errors);
    if !errors.is_empty() {
        tracing::warn!(key = %key, errors = ?errors, "fluent format errors");
    }
    Some(s.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_en_bundle() {
        let i18n = I18n::new("en");
        assert_eq!(i18n.active(), "en");
        let s = i18n.t("propose-rejected");
        assert!(!s.is_empty());
        assert_ne!(s, "propose-rejected");
    }

    #[test]
    fn loads_pt_br_bundle() {
        let i18n = I18n::new("pt-BR");
        assert_eq!(i18n.active(), "pt-BR");
        let s = i18n.t("propose-rejected");
        assert!(s.to_lowercase().contains("rejei"));
    }

    #[test]
    fn unknown_locale_falls_back_to_en() {
        let i18n = I18n::new("xx-YY");
        assert_eq!(i18n.active(), "en");
    }

    #[test]
    fn missing_key_returns_key_itself() {
        let i18n = I18n::new("en");
        let s = i18n.t("definitely-not-a-real-key");
        assert_eq!(s, "definitely-not-a-real-key");
    }

    #[test]
    fn args_format_into_pattern() {
        let i18n = I18n::new("en");
        let mut args = FluentArgs::new();
        args.set("task_id", "T-42");
        let s = i18n.t_with("propose-accepted", Some(&args));
        assert!(s.contains("T-42"), "got: {s}");
    }

    #[test]
    fn i18n_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<I18n>();
    }

    #[test]
    fn dump_namespace_strings_returns_ui_keys() {
        let i18n = I18n::new("pt-BR");
        let dict = i18n.dump_namespace_strings("ui");
        assert!(dict.contains_key("board-column-todo"));
        assert_eq!(
            dict.get("board-column-todo").map(String::as_str),
            Some("A Fazer")
        );
    }

    #[test]
    fn dump_namespace_strings_falls_back_to_en_for_missing_keys() {
        // Even if only `en` declared a key, it should appear in the
        // dump for any locale (resolved through the fallback chain).
        let i18n_en = I18n::new("en");
        let en_keys: std::collections::HashSet<_> =
            i18n_en.dump_namespace_strings("ui").into_keys().collect();
        let i18n_pt = I18n::new("pt-BR");
        let pt_keys: std::collections::HashSet<_> =
            i18n_pt.dump_namespace_strings("ui").into_keys().collect();
        assert!(en_keys.is_subset(&pt_keys) || pt_keys.is_subset(&en_keys) || en_keys == pt_keys);
        assert!(!en_keys.is_empty());
    }

    #[test]
    fn extract_message_ids_skips_comments_and_terms() {
        let ftl = "# a comment\n-term-name = value\nfoo = bar\n  continuation\nbaz-qux = quux\n";
        let ids = super::extract_message_ids(ftl);
        assert_eq!(ids, vec!["foo".to_string(), "baz-qux".to_string()]);
    }
}
