//! Risk heuristics + secret redaction over the changed set (PLAN §C.11.c).
//!
//! Every classification is heuristic and versioned via
//! [`RISK_HEURISTIC_VERSION`] (defined in `crate::review`). Risks NEVER
//! block `done`; they only annotate the package and (for a subset) drive
//! the `needs_focused_human_review` overlay in `state.rs`.
//!
//! Secret detection runs on **added lines only** (the `+` side of a
//! unified diff) and stores ONLY redacted [`SecretMatch`] metadata
//! (`kind`, `file`, `line`, `confidence`) — the matched text is never
//! retained anywhere.

use super::base::BaseResolution;
use super::git::GitCtx;
use crate::review::{ChangedFile, FileChange, RiskFlag, SecretConfidence, SecretMatch};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use std::sync::OnceLock;

/// Files larger than this many changed lines flag `large-file`.
const LARGE_FILE_LINES: u32 = 500;
/// Per-file diff byte ceiling for the secret scan; larger files are
/// skipped (treated as binary/oversized) to bound work.
const SECRET_SCAN_BYTE_CAP: usize = 1024 * 1024;

/// Outcome of the risk pass.
#[derive(Debug, Clone, Default)]
pub(crate) struct RiskOutcome {
    pub risks: Vec<RiskFlag>,
    pub secret_findings: Vec<SecretMatch>,
}

/// Lowercased basename of a POSIX-ish path.
fn basename_lower(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase()
}

fn migration_globs() -> &'static GlobSet {
    static G: OnceLock<GlobSet> = OnceLock::new();
    G.get_or_init(|| {
        let mut b = GlobSetBuilder::new();
        b.add(Glob::new("**/migrations/**").unwrap());
        b.add(Glob::new("**/migrations/*.sql").unwrap());
        b.add(Glob::new("migrations/**").unwrap());
        b.build().unwrap()
    })
}

fn public_contract_globs() -> &'static GlobSet {
    static G: OnceLock<GlobSet> = OnceLock::new();
    G.get_or_init(|| {
        let mut b = GlobSetBuilder::new();
        b.add(Glob::new("proto/**").unwrap());
        b.add(Glob::new("**/proto/**").unwrap());
        b.add(Glob::new("**/*.proto").unwrap());
        b.build().unwrap()
    })
}

/// Auth-related path/identifier substrings (lowercased), heuristic and
/// versioned with the risk pass.
const AUTH_NEEDLES: &[&str] = &[
    "auth",
    "token",
    "secret",
    "keyring",
    "password",
    "credential",
];

/// New-dependency manifests/locks (matched on basename).
fn is_dependency_manifest(path: &str) -> bool {
    matches!(
        basename_lower(path).as_str(),
        "cargo.toml"
            | "cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
    )
}

/// One compiled secret pattern with its kind + confidence bucket.
struct SecretPattern {
    kind: &'static str,
    confidence: SecretConfidence,
    re: Regex,
}

fn secret_patterns() -> &'static [SecretPattern] {
    static P: OnceLock<Vec<SecretPattern>> = OnceLock::new();
    P.get_or_init(|| {
        vec![
            SecretPattern {
                kind: "private_key",
                confidence: SecretConfidence::High,
                re: Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----").unwrap(),
            },
            SecretPattern {
                kind: "aws_access_key",
                confidence: SecretConfidence::High,
                re: Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap(),
            },
            SecretPattern {
                kind: "github_token",
                confidence: SecretConfidence::High,
                re: Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b").unwrap(),
            },
            SecretPattern {
                kind: "slack_token",
                confidence: SecretConfidence::High,
                re: Regex::new(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b").unwrap(),
            },
            SecretPattern {
                kind: "generic_secret_assignment",
                confidence: SecretConfidence::Low,
                // key-ish identifier = quoted/long value.
                re: Regex::new(
                    r#"(?i)(api[_-]?key|secret|token|password|passwd)\s*[:=]\s*['"][^'"]{8,}['"]"#,
                )
                .unwrap(),
            },
        ]
    })
}

/// Classify path-based risks over the (already collected) changed set.
fn path_risks(files: &[ChangedFile]) -> Vec<RiskFlag> {
    let mut out: Vec<RiskFlag> = Vec::new();
    let push = |r: RiskFlag, out: &mut Vec<RiskFlag>| {
        if !out.contains(&r) {
            out.push(r);
        }
    };
    let mig = migration_globs();
    let pubc = public_contract_globs();
    for f in files {
        let p = &f.path;
        let pl = p.to_ascii_lowercase();
        if is_dependency_manifest(p) {
            push(RiskFlag::NewDependency, &mut out);
        }
        if mig.is_match(p) {
            push(RiskFlag::Migration, &mut out);
        }
        if pubc.is_match(p) {
            push(RiskFlag::PublicContract, &mut out);
        }
        if AUTH_NEEDLES.iter().any(|n| pl.contains(n)) {
            push(RiskFlag::Auth, &mut out);
        }
        if f.lines_added.saturating_add(f.lines_deleted) > LARGE_FILE_LINES {
            push(RiskFlag::LargeFile, &mut out);
        }
    }
    out
}

/// Scan the added (`+`) lines of a single file's unified diff for secrets.
/// `unified` is the raw unified-diff text for ONE file (hunks only).
/// Returns redacted findings; the matched text is never returned.
fn scan_added_lines(file: &str, unified: &str) -> Vec<SecretMatch> {
    let mut out = Vec::new();
    if unified.len() > SECRET_SCAN_BYTE_CAP {
        return out; // oversized → skip (mirrors binary skip)
    }
    let pats = secret_patterns();
    // Track the new-file line number as we walk hunks.
    let mut new_line: u32 = 0;
    for raw in unified.lines() {
        if let Some(rest) = raw.strip_prefix("@@") {
            // hunk header: @@ -a,b +c,d @@ ; parse the +c start.
            if let Some(plus) = rest.split('+').nth(1) {
                let num: String = plus.chars().take_while(|c| c.is_ascii_digit()).collect();
                new_line = num.parse::<u32>().unwrap_or(1);
            }
            continue;
        }
        if let Some(added) = raw.strip_prefix('+') {
            if added.starts_with("++") {
                continue; // "+++ b/file" header line
            }
            for p in pats {
                if p.re.is_match(added) {
                    out.push(SecretMatch {
                        kind: p.kind.to_string(),
                        file: file.to_string(),
                        line: new_line,
                        confidence: p.confidence,
                    });
                    break; // one finding per line is enough
                }
            }
            new_line = new_line.saturating_add(1);
        } else if raw.starts_with(' ') {
            new_line = new_line.saturating_add(1);
        }
        // '-' lines and metadata do not advance the new-file counter.
    }
    out
}

/// Run the full risk pass: path heuristics + a per-file secret scan over
/// added lines (PLAN §C.11.c). Git failures are swallowed (best-effort —
/// the orchestrator already records collection errors).
pub(crate) async fn assess(
    git: &GitCtx,
    files: &[ChangedFile],
    base: &BaseResolution,
) -> RiskOutcome {
    let mut risks = path_risks(files);
    let mut secret_findings: Vec<SecretMatch> = Vec::new();

    for f in files {
        if f.binary || matches!(f.change, FileChange::Deleted) {
            continue;
        }
        // Get this file's unified diff (worktree state vs base/HEAD). For
        // an untracked file there is no tracked diff; `diff --no-index`
        // against the empty tree is read-only. We prefer the committed +
        // worktree diff via `git diff <base> -- <path>` which captures
        // staged+unstaged+committed added lines for tracked files.
        let unified = if let Some(b) = base.base_sha.as_deref() {
            git.run_diff(&["diff", b, "--", &f.path])
                .await
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        } else {
            git.run_diff(&["diff", "HEAD", "--", &f.path])
                .await
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        };
        if let Some(u) = unified {
            secret_findings.extend(scan_added_lines(&f.path, &u));
        }
    }

    if !secret_findings.is_empty() && !risks.contains(&RiskFlag::PossibleSecret) {
        risks.push(RiskFlag::PossibleSecret);
    }

    RiskOutcome {
        risks,
        secret_findings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cf(path: &str, added: u32) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            change: FileChange::Modified,
            renamed_from: None,
            lines_added: added,
            lines_deleted: 0,
            binary: false,
        }
    }

    #[test]
    fn dependency_and_migration_and_proto_fire() {
        let files = vec![
            cf("Cargo.toml", 1),
            cf("src-tauri/migrations/004_review.sql", 2),
            cf("proto/src/ops.rs", 3),
        ];
        let r = path_risks(&files);
        assert!(r.contains(&RiskFlag::NewDependency));
        assert!(r.contains(&RiskFlag::Migration));
        assert!(r.contains(&RiskFlag::PublicContract));
    }

    #[test]
    fn auth_path_fires() {
        let files = vec![cf("src/auth/token.rs", 1)];
        assert!(path_risks(&files).contains(&RiskFlag::Auth));
    }

    #[test]
    fn large_file_threshold() {
        let small = vec![cf("a.rs", LARGE_FILE_LINES)];
        assert!(!path_risks(&small).contains(&RiskFlag::LargeFile));
        let big = vec![cf("a.rs", LARGE_FILE_LINES + 1)];
        assert!(path_risks(&big).contains(&RiskFlag::LargeFile));
    }

    #[test]
    fn secret_scan_redacts_and_tracks_line() {
        let diff = "@@ -0,0 +1,3 @@\n+harmless\n+aws = \"AKIAIOSFODNN7EXAMPLE\"\n+more\n";
        let found = scan_added_lines("config.toml", diff);
        assert_eq!(found.len(), 1);
        let m = &found[0];
        assert_eq!(m.kind, "aws_access_key");
        assert_eq!(m.file, "config.toml");
        assert_eq!(m.line, 2); // second added line in the +1 hunk
                               // Redaction invariant: the struct has no field carrying the text.
        let json = serde_json::to_string(m).unwrap();
        assert!(!json.contains("AKIA"));
    }

    #[test]
    fn private_key_detected_high_confidence() {
        let diff = "@@ -0,0 +1,1 @@\n+-----BEGIN RSA PRIVATE KEY-----\n";
        let found = scan_added_lines("id_rsa", diff);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].confidence, SecretConfidence::High);
    }

    #[test]
    fn removed_lines_not_scanned() {
        let diff = "@@ -1,1 +0,0 @@\n-aws = \"AKIAIOSFODNN7EXAMPLE\"\n";
        assert!(scan_added_lines("x", diff).is_empty());
    }
}
