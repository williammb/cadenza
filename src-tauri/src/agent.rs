//! Per-agent launcher: turns "user wants to run agent X on task T with
//! model M, optionally resuming conversation C" into a concrete
//! `SpawnConfig` for `spawn.rs`.
//!
//! Each `AgenteKind` has a different CLI shape:
//!   - Claude Code: accepts `--session-id <uuid>` so we control the
//!     conversation id from the start. Resume with `--resume <uuid>`.
//!   - Codex: generates its own session UUID on every start. We capture
//!     it asynchronously from `~/.codex/sessions/<y>/<m>/<d>/rollout-…-<uuid>.jsonl`
//!     after spawning. Resume with `codex resume <uuid>` (UUIDs take
//!     precedence over thread-name args per `codex resume --help`).
//!
//! Verified empirically on 2026-05-27 against `claude --help` and
//! `codex --help`. If either CLI's argument surface changes, this
//! module is the single seam to update.

use std::path::{Path, PathBuf};
use std::time::SystemTime;
use uuid::Uuid;

use crate::config::AgenteKind;
use crate::spawn::SpawnConfig;

const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 30;

/// What `start_task_agent` needs to do after spawning the PTY.
pub struct LaunchPlan {
    pub spawn: SpawnConfig,
    /// Conversation id known *before* spawning. For Claude this is the
    /// UUID we passed via `--session-id`; for Codex it's `None` on
    /// first run (will be captured asynchronously) and `Some` on resume.
    pub conversation_id_known: Option<String>,
    /// Whether the caller should kick off the Codex-specific async
    /// capture task to find the generated session UUID on disk.
    pub pending_codex_capture: Option<CodexCapture>,
}

#[derive(Clone, Debug)]
pub struct CodexCapture {
    /// `~/.codex/sessions/` root. Walked recursively for the newest
    /// `*.jsonl` file with mtime after `started_at`.
    pub sessions_root: PathBuf,
    pub started_at: SystemTime,
}

/// Choose the binary name for `kind` when the user hasn't overridden
/// the path in `Config.agente.command` / `Project.agente.command`.
pub fn default_command(kind: AgenteKind) -> &'static str {
    match kind {
        AgenteKind::ClaudeCode => "claude",
        AgenteKind::Codex => "codex",
    }
}

/// Resolve the launch plan. `command_override` (if `Some`) wins over
/// `default_command(kind)` — typically comes from `config.agente.command`
/// or `project.agente.command`.
///
/// `existing_conversation_id` is the value from `task-runs.json` (if
/// any). Its presence flips us into resume mode.
pub fn plan_launch(
    kind: AgenteKind,
    model: &str,
    command_override: Option<&Path>,
    cwd: &Path,
    task_id: &str,
    project_id: &str,
    existing_conversation_id: Option<&str>,
) -> LaunchPlan {
    let command = command_override
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| default_command(kind).to_string());

    let model = model.trim();

    match kind {
        AgenteKind::ClaudeCode => plan_claude(
            command,
            model,
            cwd,
            task_id,
            project_id,
            existing_conversation_id,
        ),
        AgenteKind::Codex => plan_codex(
            command,
            model,
            cwd,
            task_id,
            project_id,
            existing_conversation_id,
        ),
    }
}

fn plan_claude(
    command: String,
    model: &str,
    cwd: &Path,
    task_id: &str,
    project_id: &str,
    existing_conversation_id: Option<&str>,
) -> LaunchPlan {
    let mut args: Vec<String> = Vec::new();

    let conversation_id = match existing_conversation_id {
        Some(id) => {
            args.push("--resume".to_string());
            args.push(id.to_string());
            id.to_string()
        }
        None => {
            // We generate the UUID and tell Claude to use it. This way
            // the conversation id is known *before* the process starts,
            // so we can persist it immediately and not race the PTY.
            let uuid = Uuid::new_v4();
            let s = uuid.to_string();
            args.push("--session-id".to_string());
            args.push(s.clone());
            s
        }
    };

    if !model.is_empty() {
        args.push("--model".to_string());
        args.push(model.to_string());
    }

    let cfg = SpawnConfig::new(command)
        .args(args)
        .cwd(cwd)
        .size(DEFAULT_COLS, DEFAULT_ROWS)
        .cadenza_env(project_id, task_id, &conversation_id);

    LaunchPlan {
        spawn: cfg,
        conversation_id_known: Some(conversation_id),
        pending_codex_capture: None,
    }
}

fn plan_codex(
    command: String,
    model: &str,
    cwd: &Path,
    task_id: &str,
    project_id: &str,
    existing_conversation_id: Option<&str>,
) -> LaunchPlan {
    let mut args: Vec<String> = Vec::new();

    match existing_conversation_id {
        Some(id) => {
            args.push("resume".to_string());
            args.push(id.to_string());
            // Don't pass --model on resume: codex pins the model to the
            // saved session. Passing it would either be ignored or
            // (worse) silently break the resume on some versions.
        }
        None => {
            if !model.is_empty() {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
        }
    };

    // CLAUDE_SESSION_ID env var still helps the cadenza-cli skill: the
    // user can re-run `cadenza-cli current` from inside codex and the
    // mapping still works. We seed it with whatever id we know — empty
    // string when we don't (Codex first run).
    let conv_seed = existing_conversation_id.unwrap_or("");

    let cfg = SpawnConfig::new(command)
        .args(args)
        .cwd(cwd)
        .size(DEFAULT_COLS, DEFAULT_ROWS)
        .cadenza_env(project_id, task_id, conv_seed);

    let pending_capture = if existing_conversation_id.is_none() {
        Some(CodexCapture {
            sessions_root: codex_sessions_root(),
            started_at: SystemTime::now(),
        })
    } else {
        None
    };

    LaunchPlan {
        spawn: cfg,
        conversation_id_known: existing_conversation_id.map(String::from),
        pending_codex_capture: pending_capture,
    }
}

fn codex_sessions_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".codex")
        .join("sessions")
}

/// Walk `sessions_root` looking for a `*.jsonl` rollout file created
/// after `started_at`, and parse the trailing UUID from its filename.
/// Returns the newest match (by mtime) or `None`.
///
/// Codex filename pattern (verified 2026-05-27):
///   `rollout-2026-04-01T07-22-39-019d4891-0feb-78a2-8f90-841686dc0175.jsonl`
/// The UUID is the last 36 chars of the stem.
pub fn find_codex_session_uuid(capture: &CodexCapture) -> Option<String> {
    let mut candidates: Vec<(SystemTime, String)> = Vec::new();
    collect_jsonl_newer_than(&capture.sessions_root, capture.started_at, &mut candidates);
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    for (_mtime, name) in candidates {
        if let Some(uuid) = extract_uuid_suffix(&name) {
            return Some(uuid);
        }
    }
    None
}

fn collect_jsonl_newer_than(
    dir: &Path,
    threshold: SystemTime,
    out: &mut Vec<(SystemTime, String)>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            collect_jsonl_newer_than(&path, threshold, out);
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        // Allow a small slack — Codex creates the file slightly after
        // we record SystemTime::now() (clock granularity on Windows is
        // ~16 ms and the spawn takes longer than that anyway, but be
        // defensive).
        if mtime
            .duration_since(threshold)
            .ok()
            .map(|d| d.as_secs_f64() > -1.0)
            .unwrap_or(false)
            || mtime >= threshold
        {
            if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                out.push((mtime, name.to_string()));
            }
        }
    }
}

fn extract_uuid_suffix(stem: &str) -> Option<String> {
    // UUID is 36 chars (8-4-4-4-12 + 4 hyphens). The codex rollout
    // filenames end with the UUID.
    if stem.len() < 36 {
        return None;
    }
    let candidate = &stem[stem.len() - 36..];
    let valid = candidate.chars().enumerate().all(|(i, c)| match i {
        8 | 13 | 18 | 23 => c == '-',
        _ => c.is_ascii_hexdigit(),
    });
    if valid {
        Some(candidate.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn claude_new_session_passes_session_id() {
        let plan = plan_launch(
            AgenteKind::ClaudeCode,
            "claude-opus-4-7",
            None,
            Path::new("/tmp/proj"),
            "T-1",
            "proj-a",
            None,
        );
        let cmd = &plan.spawn.command;
        assert_eq!(cmd, "claude");
        let args = &plan.spawn.args;
        // Should contain --session-id <uuid> --model <m>
        let i = args
            .iter()
            .position(|a| a == "--session-id")
            .expect("--session-id");
        let uuid = &args[i + 1];
        assert_eq!(uuid.len(), 36, "uuid arg should be 36 chars, got {uuid}");
        let m = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(args[m + 1], "claude-opus-4-7");
        assert_eq!(plan.conversation_id_known.as_deref(), Some(uuid.as_str()));
        assert!(plan.pending_codex_capture.is_none());
    }

    #[test]
    fn claude_resume_uses_existing_id() {
        let plan = plan_launch(
            AgenteKind::ClaudeCode,
            "sonnet",
            None,
            Path::new("/tmp/proj"),
            "T-1",
            "proj-a",
            Some("AAAA-BBBB"),
        );
        let args = &plan.spawn.args;
        let i = args.iter().position(|a| a == "--resume").expect("--resume");
        assert_eq!(args[i + 1], "AAAA-BBBB");
        assert!(!args.iter().any(|a| a == "--session-id"));
        assert_eq!(plan.conversation_id_known.as_deref(), Some("AAAA-BBBB"));
    }

    #[test]
    fn codex_new_session_marks_pending_capture() {
        let plan = plan_launch(
            AgenteKind::Codex,
            "gpt-5.4",
            None,
            Path::new("/tmp/proj"),
            "T-2",
            "proj-b",
            None,
        );
        assert_eq!(plan.spawn.command, "codex");
        let args = &plan.spawn.args;
        let m = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(args[m + 1], "gpt-5.4");
        assert!(!args.iter().any(|a| a == "resume"));
        assert!(plan.conversation_id_known.is_none());
        assert!(plan.pending_codex_capture.is_some());
    }

    #[test]
    fn codex_resume_uses_subcommand_and_skips_model() {
        let plan = plan_launch(
            AgenteKind::Codex,
            "gpt-5.4",
            None,
            Path::new("/tmp/proj"),
            "T-2",
            "proj-b",
            Some("019d4891-0feb-78a2-8f90-841686dc0175"),
        );
        let args = &plan.spawn.args;
        assert_eq!(args[0], "resume");
        assert_eq!(args[1], "019d4891-0feb-78a2-8f90-841686dc0175");
        // No --model on resume — see comment in plan_codex.
        assert!(!args.iter().any(|a| a == "--model"));
        assert!(plan.pending_codex_capture.is_none());
    }

    #[test]
    fn command_override_wins() {
        let plan = plan_launch(
            AgenteKind::ClaudeCode,
            "opus",
            Some(Path::new("/opt/anthropic/bin/claude-beta")),
            Path::new("/tmp"),
            "T-1",
            "p",
            None,
        );
        assert_eq!(plan.spawn.command, "/opt/anthropic/bin/claude-beta");
    }

    #[test]
    fn empty_model_is_not_passed() {
        let plan = plan_launch(
            AgenteKind::ClaudeCode,
            "",
            None,
            Path::new("/tmp"),
            "T-1",
            "p",
            None,
        );
        assert!(!plan.spawn.args.iter().any(|a| a == "--model"));
    }

    #[test]
    fn extract_uuid_suffix_matches_codex_rollout() {
        let name = "rollout-2026-04-01T07-22-39-019d4891-0feb-78a2-8f90-841686dc0175";
        assert_eq!(
            extract_uuid_suffix(name).as_deref(),
            Some("019d4891-0feb-78a2-8f90-841686dc0175")
        );
    }

    #[test]
    fn extract_uuid_suffix_rejects_non_uuid_tail() {
        assert!(extract_uuid_suffix("hello").is_none());
        assert!(extract_uuid_suffix("rollout-not-a-uuid-here-but-long-enough-tail").is_none());
    }

    #[test]
    fn find_codex_session_uuid_picks_newest_jsonl() {
        let dir = TempDir::new().unwrap();
        let day = dir.path().join("2026").join("05").join("27");
        fs::create_dir_all(&day).unwrap();

        let old_name = "rollout-2026-05-26T10-00-00-019d4891-0feb-78a2-8f90-aaaaaaaaaaaa.jsonl";
        let new_name = "rollout-2026-05-27T11-22-33-019d4891-0feb-78a2-8f90-bbbbbbbbbbbb.jsonl";

        // Write the "old" file first so its mtime naturally predates
        // `started_at` — avoids pulling in a filetime dep just for the test.
        fs::write(day.join(old_name), "").unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let started_at = SystemTime::now();
        std::thread::sleep(Duration::from_millis(30));
        fs::write(day.join(new_name), "").unwrap();

        let capture = CodexCapture {
            sessions_root: dir.path().to_path_buf(),
            started_at,
        };
        let got = find_codex_session_uuid(&capture);
        assert_eq!(got.as_deref(), Some("019d4891-0feb-78a2-8f90-bbbbbbbbbbbb"));
    }
}
