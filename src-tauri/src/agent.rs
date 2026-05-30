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
//!
//! Initial-prompt delivery (verified 2026-05-30 against `--help` for all
//! three): the prompt is passed via argv so the backend never types into
//! the live PTY. Claude and Codex take it as a trailing positional
//! (`claude/codex [OPTIONS] [PROMPT]`, interactive by default); agy needs
//! `--prompt-interactive <prompt>` (it rejects a bare positional). See
//! `PromptDelivery`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;
use uuid::Uuid;

use crate::config::AgenteKind;
use crate::spawn::SpawnConfig;

const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 30;

/// How the freshly-rendered initial prompt reaches the agent.
///
/// The preferred path is `Argv`: the prompt is baked into the spawn
/// command line, so the backend never types into the live PTY and there
/// is no race with the agent's UI bootstrap. All currently supported
/// agents (Claude, Codex, agy) have a verified initial-prompt flag and
/// use `Argv`. `TypeIn` is the retained fallback for an agent whose CLI
/// surface changes or a future agent without such a flag — keep it wired
/// so the seam stays extensible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptDelivery {
    /// Baked into the spawn argv — the caller must NOT type into the PTY.
    Argv,
    /// No verified initial-prompt flag for this agent; the caller types
    /// the prompt into the TUI after boot (`send_initial_prompt`).
    // No planner constructs this today — it's a deliberate fallback for
    // CLI drift / future agents, not dead code to delete.
    #[allow(dead_code)]
    TypeIn,
}

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
    /// How the caller should deliver the initial prompt (if any). `Argv`
    /// means it is already in `spawn`; `TypeIn` means the caller must
    /// type it into the PTY after boot.
    pub prompt_delivery: PromptDelivery,
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
        AgenteKind::Antigravity => "agy",
    }
}

/// Per-agent install detection: whether the agent's CLI binary lives
/// on `PATH` and/or its dotfile directory exists under `$HOME`.
///
/// `installed` is `on_path || has_config_dir` — the UI considers the
/// agent usable if either signal is true. We expose the underlying
/// signals so the UI can show a tooltip explaining what's missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct AgentPresence {
    pub kind: AgenteKind,
    pub installed: bool,
    pub on_path: bool,
    pub has_config_dir: bool,
}

/// Return install presence for every supported agent. Stable order:
/// Claude Code, Codex, Antigravity — matches the UI's option order.
pub fn list_installed_agents() -> Vec<AgentPresence> {
    [
        AgenteKind::ClaudeCode,
        AgenteKind::Codex,
        AgenteKind::Antigravity,
    ]
    .into_iter()
    .map(detect_presence)
    .collect()
}

fn detect_presence(kind: AgenteKind) -> AgentPresence {
    let on_path = binary_on_path(default_command(kind));
    let has_config_dir = config_dir_for(kind).map(|p| p.is_dir()).unwrap_or(false);
    // `on_path` stays faithful to PATH; the off-PATH locator (e.g. the
    // OpenAI Codex store) only contributes to `installed` so the agent is
    // still considered usable when we can auto-detect its binary.
    let located = crate::spawn::locate_agent_binary(default_command(kind)).is_some();
    AgentPresence {
        kind,
        installed: on_path || has_config_dir || located,
        on_path,
        has_config_dir,
    }
}

fn config_dir_for(kind: AgenteKind) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(match kind {
        AgenteKind::ClaudeCode => home.join(".claude"),
        AgenteKind::Codex => home.join(".codex"),
        // TODO(agy-verify): confirm the dir `agy` actually creates.
        // Docs point at `~/.gemini/antigravity-cli` (skills/MCP config)
        // and `~/.config/antigravity` (config.toml). `.gemini` is the
        // one tied to agent state, so prefer it for presence detection.
        AgenteKind::Antigravity => home.join(".gemini").join("antigravity-cli"),
    })
}

/// Look up `name` on `PATH`. On Windows we append each `PATHEXT` entry
/// (defaulting to the standard list when the env var is missing); on
/// Unix we match the bare name. Returns the first hit; we only care
/// about presence, not the resolved path.
fn binary_on_path(name: &str) -> bool {
    // Use the augmented search PATH (Homebrew, ~/.local/bin, npm-global,
    // …) so presence detection matches how the agent is actually resolved
    // on a GUI launch with a stripped PATH. See `spawn::search_path`.
    binary_in_path_var(name, &crate::spawn::search_path())
}

/// Search `path_var` (a `PATH`-style list) for an executable named `name`.
/// Split out from `binary_on_path` so tests can pass a synthetic `PATH`
/// directly instead of mutating the process-wide env var — that mutation
/// races other tests in the same binary that resolve binaries from `PATH`
/// (e.g. the `git` spawns in `git::tests`).
fn binary_in_path_var(name: &str, path_var: &std::ffi::OsStr) -> bool {
    let exts = path_extensions();
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in &exts {
            let mut candidate = dir.join(name);
            if !ext.is_empty() {
                let mut full = candidate.into_os_string();
                full.push(ext);
                candidate = PathBuf::from(full);
            }
            if candidate.is_file() {
                return true;
            }
        }
    }
    false
}

fn path_extensions() -> Vec<std::ffi::OsString> {
    if cfg!(windows) {
        let raw = std::env::var_os("PATHEXT")
            .unwrap_or_else(|| std::ffi::OsString::from(".COM;.EXE;.BAT;.CMD"));
        let lossy = raw.to_string_lossy().into_owned();
        let mut out: Vec<std::ffi::OsString> = lossy
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(std::ffi::OsString::from)
            .collect();
        // Also try the bare name — npm-style shims sometimes ship a
        // PowerShell file named `claude.ps1` while a `claude` shim with
        // no extension also exists (Git Bash setups).
        out.push(std::ffi::OsString::new());
        out
    } else {
        vec![std::ffi::OsString::new()]
    }
}

/// Resolve the launch plan. `command_override` (if `Some`) wins over
/// `default_command(kind)` — typically comes from `config.agente.command`
/// or `project.agente.command`.
///
/// `existing_conversation_id` is the value from `task-runs.json` (if
/// any). Its presence flips us into resume mode.
// One positional per launch dimension reads clearly at the two call sites;
// a params struct is the eventual cleanup if this grows further.
#[allow(clippy::too_many_arguments)]
pub fn plan_launch(
    kind: AgenteKind,
    model: &str,
    command_override: Option<&Path>,
    cwd: &Path,
    task_id: &str,
    project_id: &str,
    existing_conversation_id: Option<&str>,
    initial_prompt: Option<&str>,
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
            initial_prompt,
        ),
        AgenteKind::Codex => plan_codex(
            command,
            model,
            cwd,
            task_id,
            project_id,
            existing_conversation_id,
            initial_prompt,
        ),
        AgenteKind::Antigravity => plan_antigravity(
            command,
            model,
            cwd,
            task_id,
            project_id,
            existing_conversation_id,
            initial_prompt,
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
    initial_prompt: Option<&str>,
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

    // Claude takes the initial prompt as a trailing positional argument and
    // stays interactive (it only goes non-interactive with `-p/--print`).
    // Verified against `claude --help`: "Usage: claude [options] [command]
    // [prompt]". Baking it here means the backend never types into the PTY.
    if let Some(prompt) = initial_prompt {
        args.push(prompt.to_string());
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
        prompt_delivery: PromptDelivery::Argv,
    }
}

fn plan_codex(
    command: String,
    model: &str,
    cwd: &Path,
    task_id: &str,
    project_id: &str,
    existing_conversation_id: Option<&str>,
    initial_prompt: Option<&str>,
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

    // Codex takes the initial prompt as a trailing positional argument and
    // starts the interactive TUI by default (the `exec` subcommand is the
    // non-interactive one). Verified against `codex --help`: "Usage: codex
    // [OPTIONS] [PROMPT]". Only on a fresh start — a resume uses the
    // `resume <id>` subcommand and carries its own context, so the caller
    // passes no prompt there.
    if let Some(prompt) = initial_prompt {
        args.push(prompt.to_string());
    }

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
        prompt_delivery: PromptDelivery::Argv,
    }
}

fn codex_sessions_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".codex")
        .join("sessions")
}

/// Plan for the Antigravity CLI (`agy`). Structurally like Codex: `agy`
/// generates its own conversation id, so the id is unknown until we
/// capture it from disk after the process boots. The differences from
/// Codex are the resume flag (`--conversation <id>` rather than a
/// `resume <id>` subcommand) and the session store location.
///
/// Capture degrades gracefully: the reused `find_codex_session_uuid`
/// walker looks for the newest `*.jsonl` with a trailing UUID under
/// `antigravity_sessions_root()`. If `agy`'s on-disk format differs (the
/// exact store is unverified — see the TODO below), the walker simply
/// finds nothing, `conversation_id` stays `None`, and every start is a
/// fresh conversation. No broken `--conversation` call, no error.
fn plan_antigravity(
    command: String,
    model: &str,
    cwd: &Path,
    task_id: &str,
    project_id: &str,
    existing_conversation_id: Option<&str>,
    initial_prompt: Option<&str>,
) -> LaunchPlan {
    let mut args: Vec<String> = Vec::new();

    match existing_conversation_id {
        Some(id) => {
            // Resume by id. Per `agy --help`, `--conversation <id>`
            // resumes a previous conversation. Skip --model on resume for
            // the same reason as Codex: the model is pinned to the saved
            // session.
            args.push("--conversation".to_string());
            args.push(id.to_string());
        }
        None => {
            if !model.is_empty() {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
        }
    };

    // agy takes the initial prompt via `--prompt-interactive <prompt>`,
    // which "runs an initial prompt interactively and continue[s] the
    // session" (verified against `agy --help`). It does NOT accept a bare
    // positional prompt, so the flag is required. Only on a fresh start —
    // a resume carries its own context.
    if let Some(prompt) = initial_prompt {
        args.push("--prompt-interactive".to_string());
        args.push(prompt.to_string());
    }

    // Seed CLAUDE_SESSION_ID for the cadenza-cli skill the same way as
    // the other agents — empty on a fresh `agy` run (id not yet known).
    let conv_seed = existing_conversation_id.unwrap_or("");

    let cfg = SpawnConfig::new(command)
        .args(args)
        .cwd(cwd)
        .size(DEFAULT_COLS, DEFAULT_ROWS)
        .cadenza_env(project_id, task_id, conv_seed);

    let pending_capture = if existing_conversation_id.is_none() {
        Some(CodexCapture {
            sessions_root: antigravity_sessions_root(),
            started_at: SystemTime::now(),
        })
    } else {
        None
    };

    LaunchPlan {
        spawn: cfg,
        conversation_id_known: existing_conversation_id.map(String::from),
        pending_codex_capture: pending_capture,
        prompt_delivery: PromptDelivery::Argv,
    }
}

fn antigravity_sessions_root() -> PathBuf {
    // TODO(agy-verify): `agy` is not installed on the dev machine, so the
    // session-store path and filename/id format are unconfirmed. The docs
    // point at `~/.gemini/antigravity-cli/` for agent state; the capture
    // reuses the codex jsonl+UUID-suffix walker. If `agy` stores sessions
    // elsewhere or with a different id format, capture returns None and
    // resume is disabled (graceful) until this is verified empirically.
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".gemini")
        .join("antigravity-cli")
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
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0));
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
            None,
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
            None,
        );
        let args = &plan.spawn.args;
        assert_eq!(args[0], "resume");
        assert_eq!(args[1], "019d4891-0feb-78a2-8f90-841686dc0175");
        // No --model on resume — see comment in plan_codex.
        assert!(!args.iter().any(|a| a == "--model"));
        assert!(plan.pending_codex_capture.is_none());
    }

    #[test]
    fn antigravity_new_session_marks_pending_capture() {
        let plan = plan_launch(
            AgenteKind::Antigravity,
            "gemini-3.1-pro",
            None,
            Path::new("/tmp/proj"),
            "T-3",
            "proj-c",
            None,
            None,
        );
        assert_eq!(plan.spawn.command, "agy");
        let args = &plan.spawn.args;
        let m = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(args[m + 1], "gemini-3.1-pro");
        assert!(!args.iter().any(|a| a == "--conversation"));
        assert!(plan.conversation_id_known.is_none());
        // agy generates its own id → capture pending, like Codex.
        assert!(plan.pending_codex_capture.is_some());
    }

    #[test]
    fn antigravity_resume_uses_conversation_flag_and_skips_model() {
        let plan = plan_launch(
            AgenteKind::Antigravity,
            "gemini-3.1-pro",
            None,
            Path::new("/tmp/proj"),
            "T-3",
            "proj-c",
            Some("019d4891-0feb-78a2-8f90-841686dc0175"),
            None,
        );
        let args = &plan.spawn.args;
        let i = args
            .iter()
            .position(|a| a == "--conversation")
            .expect("--conversation");
        assert_eq!(args[i + 1], "019d4891-0feb-78a2-8f90-841686dc0175");
        // No --model on resume — the session pins the model.
        assert!(!args.iter().any(|a| a == "--model"));
        assert!(plan.pending_codex_capture.is_none());
        assert_eq!(
            plan.conversation_id_known.as_deref(),
            Some("019d4891-0feb-78a2-8f90-841686dc0175")
        );
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
            None,
        );
        assert!(!plan.spawn.args.iter().any(|a| a == "--model"));
    }

    #[test]
    fn claude_fresh_bakes_prompt_as_trailing_arg() {
        // A fresh Claude start delivers the prompt via argv (positional,
        // last arg) and reports Argv so the caller types nothing.
        let plan = plan_launch(
            AgenteKind::ClaudeCode,
            "opus",
            None,
            Path::new("/tmp/proj"),
            "T-1",
            "proj-a",
            None,
            Some("do the task"),
        );
        assert_eq!(
            plan.spawn.args.last().map(String::as_str),
            Some("do the task")
        );
        assert_eq!(plan.prompt_delivery, PromptDelivery::Argv);
    }

    #[test]
    fn antigravity_fresh_uses_prompt_interactive_flag() {
        // agy takes the prompt via `--prompt-interactive <prompt>`, not a
        // bare positional, and reports Argv.
        let plan = plan_launch(
            AgenteKind::Antigravity,
            "gemini-3.1-pro",
            None,
            Path::new("/tmp/proj"),
            "T-3",
            "proj-c",
            None,
            Some("decompose this"),
        );
        let args = &plan.spawn.args;
        let i = args
            .iter()
            .position(|a| a == "--prompt-interactive")
            .expect("--prompt-interactive");
        assert_eq!(args[i + 1], "decompose this");
        assert_eq!(plan.prompt_delivery, PromptDelivery::Argv);
    }

    #[test]
    fn codex_fresh_bakes_prompt_as_trailing_arg() {
        // A fresh Codex start delivers the prompt via argv (positional, last
        // arg — `codex [OPTIONS] [PROMPT]`) and reports Argv.
        let plan = plan_launch(
            AgenteKind::Codex,
            "gpt-5.4",
            None,
            Path::new("/tmp/proj"),
            "T-2",
            "proj-b",
            None,
            Some("do the task"),
        );
        assert_eq!(
            plan.spawn.args.last().map(String::as_str),
            Some("do the task")
        );
        assert_eq!(plan.prompt_delivery, PromptDelivery::Argv);
    }

    #[test]
    fn resume_never_bakes_a_prompt() {
        // On resume the caller passes no prompt; nothing extra lands in argv
        // beyond the resume flags.
        let plan = plan_launch(
            AgenteKind::ClaudeCode,
            "opus",
            None,
            Path::new("/tmp/proj"),
            "T-1",
            "proj-a",
            Some("AAAA-BBBB"),
            None,
        );
        // Only --resume <id> --model <m> — last arg is the model, not a prompt.
        assert_eq!(plan.spawn.args.last().map(String::as_str), Some("opus"));
        assert_eq!(plan.prompt_delivery, PromptDelivery::Argv);
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
    fn binary_on_path_finds_file_in_isolated_path() {
        // Build a synthetic PATH that contains a single dir holding a
        // file named `mybin` (or `mybin.exe` on Windows). This avoids
        // dependence on whatever the real PATH happens to carry.
        let dir = TempDir::new().unwrap();
        let ext = if cfg!(windows) { ".exe" } else { "" };
        let name = "cadenza-probe-marker";
        let file = dir.path().join(format!("{name}{ext}"));
        fs::write(&file, b"").unwrap();

        // Pass the synthetic PATH directly rather than mutating the
        // process-wide env var, which would race other tests that resolve
        // binaries from PATH (e.g. the `git` spawns in `git::tests`).
        assert!(
            binary_in_path_var(name, dir.path().as_os_str()),
            "expected to find {name}{ext} in synthetic PATH"
        );
    }

    #[test]
    fn binary_on_path_returns_false_when_missing() {
        let dir = TempDir::new().unwrap();
        assert!(!binary_in_path_var(
            "cadenza-definitely-not-installed",
            dir.path().as_os_str()
        ));
    }

    #[test]
    fn list_installed_agents_returns_all_kinds_in_stable_order() {
        let rows = list_installed_agents();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].kind, AgenteKind::ClaudeCode);
        assert_eq!(rows[1].kind, AgenteKind::Codex);
        assert_eq!(rows[2].kind, AgenteKind::Antigravity);
        for row in &rows {
            // `installed` may also be set by the off-PATH binary locator,
            // so it's a superset of the on_path/config-dir signals.
            if row.on_path || row.has_config_dir {
                assert!(row.installed);
            }
        }
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
