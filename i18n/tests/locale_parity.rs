//! Locale parity guard.
//!
//! Every locale Cadenza packages must cover a single canonical set of
//! Fluent message ids, and every shared message must reference the same
//! `{ $variable }` set in each locale. A divergence here means a string
//! silently falls back to `en` at runtime (missing key) or that a
//! parameterized message renders a stray `{$var}` placeholder because one
//! locale forgot an interpolation (variable mismatch).
//!
//! This test parses the `.ftl` files under `locales/` directly rather than
//! going through the runtime [`cadenza_i18n::I18n`] loader: the loader's
//! per-key `en` fallback chain is exactly what would *mask* a parity gap,
//! so the check has to look at the raw, un-merged resources instead.
//!
//! The canonical key set is the union of the two mandatory locales
//! (`pt-BR` and `en`). Both MUST cover it; a key present in only one locale
//! is a failure unless it appears in [`ALLOWLIST`] with a justification.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Locales that MUST exist and MUST fully cover the canonical key set.
///
/// `pt-BR` is the primary locale and `en` is the fallback; both are
/// mandatory per `CLAUDE.md` ("pt-BR is the primary locale; en is the
/// fallback").
const MANDATORY_LOCALES: &[&str] = &["pt-BR", "en"];

/// Intentional, documented exceptions to strict key parity.
///
/// Each entry is `(locale, message_id)` and means "it is acceptable for
/// this message id to be ABSENT from `locale`". Keep this list tiny and
/// always paired with a comment explaining *why* the asymmetry is
/// intentional, so the test stays meaningful instead of brittle.
///
/// Currently empty: the two packaged locales are in full parity.
const ALLOWLIST: &[(&str, &str)] = &[
    // Example shape (do not uncomment without a real reason):
    // ("en", "pt-br-only-legal-notice"), // PT-only legal copy; no English text exists.
];

/// Absolute path to the repository's `locales/` directory.
fn locales_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("locales")
}

/// One parsed Fluent message: its id and the set of `$variable`
/// references found anywhere in its value or attributes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Message {
    vars: BTreeSet<String>,
}

/// Parse every `.ftl` file for `locale` into a `message_id -> Message`
/// map. Message ids collide across files only if the resources genuinely
/// declare a duplicate; we merge variable sets in that case rather than
/// silently dropping one.
fn load_locale(locale: &str) -> BTreeMap<String, Message> {
    let dir = locales_dir().join(locale);
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("ftl"))
        .collect();
    entries.sort();

    let mut messages: BTreeMap<String, Message> = BTreeMap::new();
    for path in entries {
        let text =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for (id, vars) in parse_ftl(&text) {
            let entry = messages.entry(id).or_default();
            entry.vars.extend(vars);
        }
    }
    messages
}

/// Extract `(message_id, variables)` pairs from raw `.ftl` text.
///
/// A message is a top-level `id = ...` declaration plus every following
/// line that is blank or indented (its multi-line value, select/plural
/// blocks, and `.attribute` lines) up to the next top-level declaration.
/// Comments (`#`) and terms (`-name`) are not messages and are skipped.
/// Variable references are any `{ $name }` token (whitespace optional)
/// occurring inside the message block.
fn parse_ftl(ftl: &str) -> Vec<(String, BTreeSet<String>)> {
    let mut out: Vec<(String, BTreeSet<String>)> = Vec::new();
    let mut current: Option<(String, BTreeSet<String>)> = None;

    let is_top_level_id = |line: &str| -> Option<String> {
        let bytes = line.as_bytes();
        if bytes.is_empty() {
            return None;
        }
        let first = bytes[0];
        // Indented => continuation; comment or term => not a message.
        if first == b' ' || first == b'\t' || first == b'#' || first == b'-' {
            return None;
        }
        let eq = line.find('=')?;
        let id = line[..eq].trim();
        if id.is_empty() {
            return None;
        }
        if id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            Some(id.to_string())
        } else {
            None
        }
    };

    for line in ftl.lines() {
        if let Some(id) = is_top_level_id(line) {
            // Flush the previous message before starting a new one.
            if let Some((prev_id, vars)) = current.take() {
                out.push((prev_id, vars));
            }
            let mut vars = BTreeSet::new();
            collect_vars(line, &mut vars);
            current = Some((id, vars));
        } else if let Some((_, vars)) = current.as_mut() {
            // Blank or indented line: part of the current message block.
            collect_vars(line, vars);
        }
        // else: leading comments/blank lines before any message — ignore.
    }
    if let Some((id, vars)) = current.take() {
        out.push((id, vars));
    }
    out
}

/// Collect every `{ $name }` variable reference in `line` into `vars`.
fn collect_vars(line: &str, vars: &mut BTreeSet<String>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() {
                let c = bytes[end];
                if c.is_ascii_alphanumeric() || c == b'_' {
                    end += 1;
                } else {
                    break;
                }
            }
            if end > start {
                vars.insert(line[start..end].to_string());
            }
            i = end;
        } else {
            i += 1;
        }
    }
}

fn allowlisted(locale: &str, key: &str) -> bool {
    ALLOWLIST.iter().any(|(l, k)| *l == locale && *k == key)
}

#[test]
fn locales_cover_canonical_key_set() {
    let loaded: BTreeMap<&str, BTreeMap<String, Message>> = MANDATORY_LOCALES
        .iter()
        .map(|&loc| (loc, load_locale(loc)))
        .collect();

    // Canonical key set = union of all mandatory locales' message ids.
    let canonical: BTreeSet<String> = loaded.values().flat_map(|m| m.keys().cloned()).collect();
    assert!(
        !canonical.is_empty(),
        "no Fluent message ids found under locales/ — loader or path is broken"
    );

    let mut failures: Vec<String> = Vec::new();
    for &locale in MANDATORY_LOCALES {
        let present = &loaded[locale];
        let missing: Vec<&String> = canonical
            .iter()
            .filter(|k| !present.contains_key(*k) && !allowlisted(locale, k))
            .collect();
        if !missing.is_empty() {
            let mut sorted: Vec<&String> = missing;
            sorted.sort();
            let list = sorted
                .iter()
                .map(|k| format!("  - {k}"))
                .collect::<Vec<_>>()
                .join("\n");
            failures.push(format!(
                "locale `{locale}` is missing {} canonical key(s):\n{list}",
                sorted.len()
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "Locale key-set parity failed.\n\n{}\n\nEither add the missing translation(s) \
         or, for an intentional asymmetry, add the (locale, key) pair to ALLOWLIST \
         in this test with a justifying comment.",
        failures.join("\n\n")
    );
}

#[test]
fn shared_keys_have_matching_variable_references() {
    let loaded: BTreeMap<&str, BTreeMap<String, Message>> = MANDATORY_LOCALES
        .iter()
        .map(|&loc| (loc, load_locale(loc)))
        .collect();

    // Use the first mandatory locale as the reference for the comparison;
    // we only compare keys SHARED by every locale so a missing key (caught
    // by the parity test above) doesn't double-report here.
    let shared: BTreeSet<String> = {
        let mut iter = loaded.values();
        let mut acc: BTreeSet<String> = iter
            .next()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        for m in iter {
            let keys: BTreeSet<String> = m.keys().cloned().collect();
            acc = acc.intersection(&keys).cloned().collect();
        }
        acc
    };

    let mut failures: Vec<String> = Vec::new();
    for key in &shared {
        // Compare each locale's variable set against the reference locale.
        let reference_locale = MANDATORY_LOCALES[0];
        let reference = &loaded[reference_locale][key].vars;
        for &locale in &MANDATORY_LOCALES[1..] {
            let other = &loaded[locale][key].vars;
            if reference != other {
                let only_ref: Vec<&String> = reference.difference(other).collect();
                let only_other: Vec<&String> = other.difference(reference).collect();
                failures.push(format!(
                    "key `{key}`: variable references differ between \
                     `{reference_locale}` and `{locale}`\n    \
                     only in {reference_locale}: {only_ref:?}\n    \
                     only in {locale}: {only_other:?}"
                ));
            }
        }
    }

    failures.sort();
    assert!(
        failures.is_empty(),
        "Locale variable-reference parity failed for {} shared key(s).\n\n{}\n\n\
         A shared message must reference the same {{ $variable }} set in every \
         locale, or it renders a stray placeholder at runtime.",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn parse_ftl_handles_multiline_and_attributes() {
    // Sanity check of the parser itself: continuation lines, select
    // blocks, attributes, comments, and terms.
    let ftl = "\
# a comment with { $ignored }
-term = a term { $also_ignored }
simple = hello { $name }
multi =
    { $count ->
        [one] 1 thing
       *[other] { $count } things
    } for { $owner }
attr = base
    .tooltip = tip { $hint }
";
    let parsed: BTreeMap<String, BTreeSet<String>> = parse_ftl(ftl).into_iter().collect();

    assert!(!parsed.contains_key("-term"), "terms must be skipped");
    assert_eq!(
        parsed.get("simple"),
        Some(&BTreeSet::from(["name".to_string()]))
    );
    assert_eq!(
        parsed.get("multi"),
        Some(&BTreeSet::from(["count".to_string(), "owner".to_string()]))
    );
    assert_eq!(
        parsed.get("attr"),
        Some(&BTreeSet::from(["hint".to_string()]))
    );
}
