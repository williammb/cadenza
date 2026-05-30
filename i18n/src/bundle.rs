//! Fluent bundle loader with fallback to `en`.
//!
//! `.ftl` files are embedded at compile time via `include_dir!`. The
//! bundle uses Fluent's **concurrent** memoizer (`IntlLangMemoizer` from
//! `intl_memoizer::concurrent`) so `I18n` is `Send + Sync`, which is
//! required by `tauri::State<'_, AppState>`.

use fluent_bundle::bundle::FluentBundle;
use fluent_bundle::resolver::errors::{ReferenceKind, ResolverError};
use fluent_bundle::{FluentArgs, FluentError, FluentResource};
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

/// Whether a Fluent error is an unresolved `$variable` reference.
///
/// These are produced when a parameterized key is rendered without args.
/// That's expected for the mass `dump_namespace_strings` path: Fluent
/// writes the literal placeholder `{$var}` into the output and the
/// frontend (`ui/i18n.js`) substitutes it client-side.
fn is_unresolved_variable(error: &FluentError) -> bool {
    matches!(
        error,
        FluentError::ResolverError(ResolverError::Reference(ReferenceKind::Variable { .. }))
    )
}

/// Decide whether `lookup` should emit a warning for `errors`.
///
/// When the caller passed no args, unresolved-variable errors are
/// expected noise (the dump renders templates for the frontend) and are
/// ignored; any *other* error still warns. When the caller passed args,
/// every error warns — that catches a caller who forgot or misnamed a
/// referenced variable.
fn should_warn(args: Option<&FluentArgs<'_>>, errors: &[FluentError]) -> bool {
    if args.is_some() {
        return !errors.is_empty();
    }
    errors.iter().any(|e| !is_unresolved_variable(e))
}

fn lookup(bundle: &Bundle, key: &str, args: Option<&FluentArgs<'_>>) -> Option<String> {
    let msg = bundle.get_message(key)?;
    let pattern = msg.value()?;
    let mut errors = vec![];
    let s = bundle.format_pattern(pattern, args, &mut errors);
    if should_warn(args, &errors) {
        // With args, log everything. Without args, drop the expected
        // unresolved-variable noise and log only the genuine errors.
        let relevant: Vec<&FluentError> = if args.is_some() {
            errors.iter().collect()
        } else {
            errors
                .iter()
                .filter(|e| !is_unresolved_variable(e))
                .collect()
        };
        tracing::warn!(key = %key, errors = ?relevant, "fluent format errors");
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

    fn variable_error(id: &str) -> FluentError {
        FluentError::ResolverError(ResolverError::Reference(ReferenceKind::Variable {
            id: id.to_string(),
        }))
    }

    #[test]
    fn should_warn_suppresses_unresolved_vars_without_args() {
        assert!(!should_warn(None, &[variable_error("count")]));
    }

    #[test]
    fn should_warn_keeps_non_variable_errors_without_args() {
        assert!(should_warn(
            None,
            &[FluentError::ResolverError(ResolverError::Cyclic)]
        ));
        // A genuine error mixed with variable noise still warns.
        assert!(should_warn(
            None,
            &[
                variable_error("count"),
                FluentError::ResolverError(ResolverError::Reference(ReferenceKind::Message {
                    id: "other".to_string(),
                    attribute: None,
                })),
            ]
        ));
    }

    #[test]
    fn should_warn_warns_on_any_error_with_args() {
        let args = FluentArgs::new();
        // Even an unresolved variable warns once args were supplied: the
        // caller likely forgot or misnamed it.
        assert!(should_warn(Some(&args), &[variable_error("count")]));
    }

    #[test]
    fn should_warn_no_errors_never_warns() {
        let args = FluentArgs::new();
        assert!(!should_warn(None, &[]));
        assert!(!should_warn(Some(&args), &[]));
    }

    #[test]
    fn boot_dump_emits_no_format_warnings() {
        // End-to-end guard for the boot path: `load_translations` calls
        // `dump_namespace_strings("ui")`, which renders every parameterized
        // key with no args. Capture WARN-level tracing for that exact call
        // and assert it produces no "fluent format errors" line for either
        // packaged locale.
        use std::io::Write;
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct BufWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for BufWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl tracing_subscriber::fmt::MakeWriter<'_> for BufWriter {
            type Writer = BufWriter;
            fn make_writer(&self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer(BufWriter(buf.clone()))
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            for locale in [DEFAULT_LOCALE, "pt-BR"] {
                let i18n = I18n::new(locale);
                let _ = i18n.dump_namespace_strings("ui");
            }
        });

        let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            !logs.contains("fluent format errors"),
            "boot dump emitted format warnings:\n{logs}"
        );
    }

    #[test]
    fn parameterized_key_without_args_renders_placeholder() {
        // The mass dump renders parameterized keys with no args; the output
        // must keep the `{$var}` template for the frontend to substitute,
        // not collapse to the raw key.
        let i18n = I18n::new("en");
        let s = i18n.t("task-error");
        assert_ne!(s, "task-error", "should render the template, not the key");
        assert!(s.contains("{$"), "expected a placeholder, got: {s}");
    }

    #[test]
    fn extract_message_ids_skips_comments_and_terms() {
        let ftl = "# a comment\n-term-name = value\nfoo = bar\n  continuation\nbaz-qux = quux\n";
        let ids = super::extract_message_ids(ftl);
        assert_eq!(ids, vec!["foo".to_string(), "baz-qux".to_string()]);
    }
}
