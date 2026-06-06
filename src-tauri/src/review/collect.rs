//! Changed-file collection + worktree fingerprint (PLAN §C.11.b).
//!
//! Four sources are merged with a documented precedence:
//!   committed `base..head` < staged (`diff --cached`) < unstaged (`diff`)
//!   < untracked (`status --porcelain=v1 -z`).
//! "Later wins" the recorded *status*; line counts are summed across the
//! committed/staged/unstaged diffs for the same path. Untracked files have
//! no diff line counts here (their bodies are captured, capped, in
//! `patch.rs`).
//!
//! The fingerprint is `sha256` of the raw `status --porcelain=v1 -z`
//! bytes; the read path compares it to detect a moved/dirtied worktree
//! (PLAN §D.13).

use super::base::BaseResolution;
use super::git::GitCtx;
use crate::review::{ChangedFile, CollectionError, FileChange};
use std::collections::BTreeMap;

/// Result of changed-file collection.
#[derive(Debug, Clone)]
pub(crate) struct Collected {
    pub files: Vec<ChangedFile>,
    /// `sha256:<hex>` of `status --porcelain=v1 -z`, or `None` if that
    /// read failed.
    pub fingerprint: Option<String>,
    /// True when a required diff source failed (the changed set is
    /// incomplete → scope untrustworthy for conditional checks).
    pub diff_unavailable: bool,
    /// True when any git output was clipped by the byte cap.
    pub truncated: bool,
    pub errors: Vec<CollectionError>,
}

fn err(code: &str, detail: impl Into<String>) -> CollectionError {
    CollectionError {
        code: code.to_string(),
        detail: detail.into(),
    }
}

/// Map a git `--name-status` status letter to our `FileChange`.
fn map_status(letter: u8) -> FileChange {
    match letter {
        b'A' => FileChange::Added,
        b'D' => FileChange::Deleted,
        b'R' => FileChange::Renamed,
        b'C' => FileChange::Added, // copy: treat the new path as added
        _ => FileChange::Modified, // M, T (typechange), U, etc.
    }
}

/// Accumulator entry: latest status (precedence-ordered) + summed counts.
#[derive(Default)]
struct Entry {
    change: Option<FileChange>,
    renamed_from: Option<String>,
    lines_added: u32,
    lines_deleted: u32,
}

/// Parse a `diff --name-status -z` byte stream, applying status + rename
/// origin into `acc`. The `-z` format is NUL-separated; rename/copy
/// entries carry two extra NUL-separated paths (old, new).
fn parse_name_status(bytes: &[u8], acc: &mut BTreeMap<String, Entry>) {
    let mut fields = bytes.split(|&b| b == 0).filter(|f| !f.is_empty());
    while let Some(status) = fields.next() {
        let letter = status[0];
        let change = map_status(letter);
        if letter == b'R' || letter == b'C' {
            // status, old-path, new-path
            let Some(old) = fields.next() else { break };
            let Some(new) = fields.next() else { break };
            let path = String::from_utf8_lossy(new).to_string();
            let e = acc.entry(path).or_default();
            e.change = Some(change);
            e.renamed_from = Some(String::from_utf8_lossy(old).to_string());
        } else {
            let Some(path) = fields.next() else { break };
            let path = String::from_utf8_lossy(path).to_string();
            let e = acc.entry(path).or_default();
            e.change = Some(change);
        }
    }
}

/// Parse `diff --numstat -z` to sum added/deleted line counts into `acc`.
/// Binary files report `-` for both and are left at 0 here (their `binary`
/// flag is set elsewhere by the secret/large-file pass).
fn parse_numstat(bytes: &[u8], acc: &mut BTreeMap<String, Entry>) {
    // numstat -z layout: "<added>\t<deleted>\t" then NUL-terminated path;
    // for renames the path is two NUL-separated fields (old, new).
    let mut fields = bytes.split(|&b| b == 0).filter(|f| !f.is_empty());
    while let Some(first) = fields.next() {
        // `first` = "<added>\t<deleted>\t<path-or-empty>"
        let text = String::from_utf8_lossy(first);
        let mut parts = text.splitn(3, '\t');
        let added = parts.next().unwrap_or("-");
        let deleted = parts.next().unwrap_or("-");
        let inline_path = parts.next().unwrap_or("");
        let path = if inline_path.is_empty() {
            // rename: old then new follow as separate fields.
            let _old = fields.next();
            match fields.next() {
                Some(p) => String::from_utf8_lossy(p).to_string(),
                None => break,
            }
        } else {
            inline_path.to_string()
        };
        let e = acc.entry(path).or_default();
        e.lines_added = e
            .lines_added
            .saturating_add(added.parse::<u32>().unwrap_or(0));
        e.lines_deleted = e
            .lines_deleted
            .saturating_add(deleted.parse::<u32>().unwrap_or(0));
    }
}

/// Parse `status --porcelain=v1 -z` for untracked (`??`) entries, adding
/// them as `Added`/untracked with the lowest line counts (0). Untracked
/// wins status precedence (applied last).
fn parse_untracked(bytes: &[u8], acc: &mut BTreeMap<String, Entry>) {
    // porcelain v1 -z: each record is "XY <path>\0"; renames add an extra
    // "\0<orig>" but untracked (`??`) never renames.
    for record in bytes.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        if record.len() < 3 {
            continue;
        }
        let xy = &record[..2];
        if xy == b"??" {
            let path = String::from_utf8_lossy(&record[3..]).to_string();
            let e = acc.entry(path).or_default();
            e.change = Some(FileChange::Added);
        }
    }
}

/// Collect the changed-file set across all four sources (PLAN §C.11.b).
pub(crate) async fn collect_changes(git: &GitCtx, base: &BaseResolution) -> Collected {
    let mut acc: BTreeMap<String, Entry> = BTreeMap::new();
    let mut errors: Vec<CollectionError> = Vec::new();
    let mut diff_unavailable = false;
    let mut truncated = false;

    // 1. committed base..head (skip when base unresolved).
    if base.committed_available() {
        if let (Some(b), Some(h)) = (&base.base_sha, &base.head_sha) {
            let range = format!("{b}..{h}");
            match git.run_diff(&["diff", "--name-status", "-z", &range]).await {
                Ok(o) => {
                    truncated |= o.truncated;
                    parse_name_status(&o.stdout, &mut acc);
                }
                Err(e) => {
                    diff_unavailable = true;
                    errors.push(err(e.code, e.detail));
                }
            }
            match git.run_diff(&["diff", "--numstat", "-z", &range]).await {
                Ok(o) => {
                    truncated |= o.truncated;
                    parse_numstat(&o.stdout, &mut acc);
                }
                Err(e) => errors.push(err(e.code, e.detail)),
            }
        }
    }

    // 2. staged (index vs HEAD).
    match git
        .run_diff(&["diff", "--cached", "--name-status", "-z"])
        .await
    {
        Ok(o) => {
            truncated |= o.truncated;
            parse_name_status(&o.stdout, &mut acc);
        }
        Err(e) => {
            diff_unavailable = true;
            errors.push(err(e.code, e.detail));
        }
    }
    if let Ok(o) = git.run_diff(&["diff", "--cached", "--numstat", "-z"]).await {
        truncated |= o.truncated;
        parse_numstat(&o.stdout, &mut acc);
    }

    // 3. unstaged (worktree vs index).
    match git.run_diff(&["diff", "--name-status", "-z"]).await {
        Ok(o) => {
            truncated |= o.truncated;
            parse_name_status(&o.stdout, &mut acc);
        }
        Err(e) => {
            diff_unavailable = true;
            errors.push(err(e.code, e.detail));
        }
    }
    if let Ok(o) = git.run_diff(&["diff", "--numstat", "-z"]).await {
        truncated |= o.truncated;
        parse_numstat(&o.stdout, &mut acc);
    }

    // 4. untracked + fingerprint, both from one porcelain read.
    let fingerprint = match git.run(&["status", "--porcelain=v1", "-z"]).await {
        Ok(o) => {
            truncated |= o.truncated;
            parse_untracked(&o.stdout, &mut acc);
            Some(fingerprint_of(&o.stdout))
        }
        Err(e) => {
            diff_unavailable = true;
            errors.push(err(e.code, e.detail));
            None
        }
    };

    let files = acc
        .into_iter()
        .map(|(path, e)| ChangedFile {
            path,
            change: e.change.unwrap_or(FileChange::Modified),
            renamed_from: e.renamed_from,
            lines_added: e.lines_added,
            lines_deleted: e.lines_deleted,
            binary: false,
        })
        .collect();

    Collected {
        files,
        fingerprint,
        diff_unavailable,
        truncated,
        errors,
    }
}

/// `sha256:<hex>` of arbitrary bytes — the worktree/index fingerprint.
pub(crate) fn fingerprint_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut s = String::from("sha256:");
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::base::resolve_base;
    use std::path::Path;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    fn run(dir: &Path, args: &[&str]) {
        let status = StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        run(dir.path(), &["init"]);
        run(dir.path(), &["config", "user.email", "t@e.com"]);
        run(dir.path(), &["config", "user.name", "T"]);
        run(dir.path(), &["commit", "--allow-empty", "-m", "init"]);
        run(dir.path(), &["branch", "-M", "main"]);
        dir
    }

    #[test]
    fn name_status_parses_rename() {
        let mut acc = BTreeMap::new();
        let bytes = b"R100\0old.rs\0new.rs\0";
        parse_name_status(bytes, &mut acc);
        let e = acc.get("new.rs").unwrap();
        assert_eq!(e.change, Some(FileChange::Renamed));
        assert_eq!(e.renamed_from.as_deref(), Some("old.rs"));
    }

    #[test]
    fn untracked_parsed_from_porcelain() {
        let mut acc = BTreeMap::new();
        let bytes = b"?? new.txt\0 M tracked.rs\0";
        parse_untracked(bytes, &mut acc);
        assert!(acc.contains_key("new.txt"));
        assert!(!acc.contains_key("tracked.rs"));
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive() {
        let a = fingerprint_of(b"?? x\0");
        let b = fingerprint_of(b"?? x\0");
        let c = fingerprint_of(b"?? y\0");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("sha256:"));
    }

    #[tokio::test]
    async fn collects_staged_unstaged_untracked() {
        let repo = init_repo();
        // tracked file committed, then modified (unstaged) + a staged new
        // file + an untracked file.
        std::fs::write(repo.path().join("a.rs"), "one\n").unwrap();
        run(repo.path(), &["add", "a.rs"]);
        run(repo.path(), &["commit", "-m", "add a"]);
        std::fs::write(repo.path().join("a.rs"), "one\ntwo\n").unwrap(); // unstaged
        std::fs::write(repo.path().join("b.rs"), "new\n").unwrap();
        run(repo.path(), &["add", "b.rs"]); // staged
        std::fs::write(repo.path().join("c.txt"), "untracked\n").unwrap(); // untracked

        let git = GitCtx::new(repo.path());
        let base = resolve_base(&git, Some("main"), None).await;
        let col = collect_changes(&git, &base).await;
        let paths: Vec<&str> = col.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"a.rs"));
        assert!(paths.contains(&"b.rs"));
        assert!(paths.contains(&"c.txt"));
        assert!(col.fingerprint.is_some());
        assert!(!col.diff_unavailable);
    }
}
