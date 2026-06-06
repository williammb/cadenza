//! Capped, secret-redacted uncommitted-patch snapshot (PLAN §C.11.e).
//!
//! The committed diff is reproducible later from `base_sha..head_sha`, but
//! the *uncommitted* diff (staged + unstaged + untracked) is ephemeral, so
//! we persist a bounded, redacted snapshot of it. Caps are dedicated to
//! this snapshot and distinct from the agent-JSON caps in §B.7:
//!
//! - total ≤ 512 KiB, per-file ≤ 64 KiB, ≤ 200 files,
//! - binary files excluded,
//! - secret matches redacted on `+` lines using `risk.rs` findings,
//! - explicit `truncated` / `files_omitted` markers.
//!
//! Untracked files are read **directly** (never `git add -N`, which mutates
//! the index) and rendered as synthetic all-added hunks.

use super::git::GitCtx;
use crate::review::{CappedPatch, CappedPatchFile, SecretMatch};
use std::collections::HashMap;

/// Total snapshot byte cap.
const TOTAL_CAP: usize = 512 * 1024;
/// Per-file byte cap.
const PER_FILE_CAP: usize = 64 * 1024;
/// Maximum number of files in the snapshot.
const MAX_FILES: usize = 200;
/// Redaction placeholder substituted for a flagged `+` line's content.
const REDACTED: &str = "+[REDACTED possible secret]";

/// Build the capped, redacted uncommitted snapshot (PLAN §C.11.e).
/// `findings` are the redaction positions from `risk.rs` (file + 1-based
/// added-line number).
pub(crate) async fn capture_uncommitted(git: &GitCtx, findings: &[SecretMatch]) -> CappedPatch {
    // Index findings by file → set of redacted added-line numbers.
    let mut redact: HashMap<&str, Vec<u32>> = HashMap::new();
    for m in findings {
        redact.entry(m.file.as_str()).or_default().push(m.line);
    }

    let mut files: Vec<CappedPatchFile> = Vec::new();
    let mut total: usize = 0;
    let mut truncated = false;
    let mut files_omitted: u32 = 0;

    // Staged + unstaged tracked diff in one read (HEAD..worktree). Using a
    // combined `git diff HEAD` captures both staged and unstaged changes
    // for tracked files; untracked files are appended separately.
    let mut per_file: Vec<(String, String)> = Vec::new();
    if let Ok(o) = git
        .run_diff(&["diff", "HEAD", "--", "."])
        .await
        .or(git.run_diff(&["diff", "--", "."]).await)
    {
        let text = String::from_utf8_lossy(&o.stdout);
        per_file.extend(split_unified_per_file(&text));
    }

    // Untracked files: list via porcelain, read bodies directly (read-only).
    if let Ok(o) = git.run(&["status", "--porcelain=v1", "-z"]).await {
        for record in o.stdout.split(|&b| b == 0).filter(|r| r.len() > 3) {
            if &record[..2] == b"??" {
                let path = String::from_utf8_lossy(&record[3..]).to_string();
                if let Some(synthetic) = synthesize_untracked(git, &path).await {
                    per_file.push((path, synthetic));
                }
            }
        }
    }

    for (path, raw) in per_file {
        if files.len() >= MAX_FILES {
            files_omitted = files_omitted.saturating_add(1);
            continue;
        }
        if is_binary_patch(&raw) {
            continue; // exclude binaries
        }
        let lines = redact
            .get(path.as_str())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let mut redacted = redact_added_lines(&raw, lines);
        let mut file_truncated = false;
        if redacted.len() > PER_FILE_CAP {
            redacted.truncate(PER_FILE_CAP);
            redacted.push_str("\n[... file truncated ...]\n");
            file_truncated = true;
            truncated = true;
        }
        if total.saturating_add(redacted.len()) > TOTAL_CAP {
            // No room for this file's content → omit it entirely.
            files_omitted = files_omitted.saturating_add(1);
            truncated = true;
            continue;
        }
        total = total.saturating_add(redacted.len());
        files.push(CappedPatchFile {
            path,
            patch: redacted,
            truncated: file_truncated,
        });
    }

    CappedPatch {
        files,
        truncated,
        files_omitted,
    }
}

/// Split a multi-file unified diff into (path, per-file-text) chunks keyed
/// by the new-side path from each `diff --git a/… b/…` header.
fn split_unified_per_file(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur_path: Option<String> = None;
    let mut buf = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(path) = cur_path.take() {
                out.push((path, std::mem::take(&mut buf)));
            }
            cur_path = parse_diff_git_path(rest);
        }
        if cur_path.is_some() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(path) = cur_path.take() {
        out.push((path, buf));
    }
    out
}

/// Extract the new-side path from a `diff --git a/x b/y` header tail.
fn parse_diff_git_path(rest: &str) -> Option<String> {
    // rest = "a/<old> b/<new>" — take the b/ side.
    let b_idx = rest.rfind(" b/")?;
    let new = &rest[b_idx + 3..];
    Some(new.to_string())
}

/// Whether a per-file diff chunk is a binary patch (git emits a
/// "Binary files … differ" or "GIT binary patch" marker).
fn is_binary_patch(chunk: &str) -> bool {
    chunk.contains("\nBinary files ")
        || chunk.starts_with("Binary files ")
        || chunk.contains("GIT binary patch")
}

/// Redact flagged added lines: any `+` content line whose new-file line
/// number is in `redact_lines` is replaced with the placeholder.
fn redact_added_lines(chunk: &str, redact_lines: &[u32]) -> String {
    if redact_lines.is_empty() {
        return chunk.to_string();
    }
    let mut out = String::with_capacity(chunk.len());
    let mut new_line: u32 = 0;
    for line in chunk.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            if let Some(plus) = rest.split('+').nth(1) {
                let num: String = plus.chars().take_while(|c| c.is_ascii_digit()).collect();
                new_line = num.parse::<u32>().unwrap_or(1);
            }
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if line.starts_with("+++") {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if let Some(_added) = line.strip_prefix('+') {
            if redact_lines.contains(&new_line) {
                out.push_str(REDACTED);
            } else {
                out.push_str(line);
            }
            out.push('\n');
            new_line = new_line.saturating_add(1);
        } else if line.starts_with(' ') {
            out.push_str(line);
            out.push('\n');
            new_line = new_line.saturating_add(1);
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Read an untracked file's body (read-only) and render it as a synthetic
/// all-added unified-diff chunk. Returns `None` for unreadable or
/// likely-binary (NUL-containing) files.
async fn synthesize_untracked(git: &GitCtx, path: &str) -> Option<String> {
    // Reuse the worktree dir via a `diff --no-index` against the empty
    // tree would mutate nothing but needs an absolute path; instead read
    // the file directly relative to the worktree. We obtain the worktree
    // root from git to keep the path handling consistent.
    let root = git.line(&["rev-parse", "--show-toplevel"]).await?;
    let full = std::path::Path::new(&root).join(path);
    let bytes = tokio::fs::read(&full).await.ok()?;
    if bytes.contains(&0) {
        return None; // binary
    }
    let body = String::from_utf8_lossy(&bytes);
    let n = body.lines().count();
    let mut chunk = String::new();
    chunk.push_str(&format!("diff --git a/{path} b/{path}\n"));
    chunk.push_str("new file mode 100644\n");
    chunk.push_str("--- /dev/null\n");
    chunk.push_str(&format!("+++ b/{path}\n"));
    chunk.push_str(&format!("@@ -0,0 +1,{n} @@\n"));
    for line in body.lines() {
        chunk.push('+');
        chunk.push_str(line);
        chunk.push('\n');
    }
    Some(chunk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_new_side_path() {
        assert_eq!(
            parse_diff_git_path("a/src/lib.rs b/src/lib.rs").as_deref(),
            Some("src/lib.rs")
        );
    }

    #[test]
    fn splits_multifile_diff() {
        let text =
            "diff --git a/x b/x\n@@ -0,0 +1,1 @@\n+x\ndiff --git a/y b/y\n@@ -0,0 +1,1 @@\n+y\n";
        let parts = split_unified_per_file(text);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].0, "x");
        assert_eq!(parts[1].0, "y");
    }

    #[test]
    fn binary_chunk_detected() {
        assert!(is_binary_patch(
            "diff --git a/x b/x\nBinary files a/x and b/x differ\n"
        ));
    }

    #[test]
    fn redacts_flagged_added_line_only() {
        let chunk = "@@ -0,0 +1,3 @@\n+harmless\n+SECRET=abc\n+also harmless\n";
        let out = redact_added_lines(chunk, &[2]);
        assert!(out.contains("+harmless"));
        assert!(out.contains(REDACTED));
        assert!(!out.contains("SECRET=abc"));
        assert!(out.contains("+also harmless"));
    }

    #[test]
    fn no_redaction_when_no_findings() {
        let chunk = "@@ -0,0 +1,1 @@\n+keep me\n";
        assert_eq!(redact_added_lines(chunk, &[]), chunk);
    }
}
