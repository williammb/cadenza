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
//!   - GitHub Copilot CLI: accepts `--session-id <uuid>` so we control the
//!     conversation id from the start. Resume with `--session-id <uuid>`.
//!   - OpenCode: generates a `ses_*` id on first start. We capture it by
//!     comparing `opencode session list --format json` before/after spawn.
//!     Resume with `opencode --session <id>`.
//!
//! Verified empirically on 2026-05-27 against `claude --help` and
//! `codex --help`. If either CLI's argument surface changes, this
//! module is the single seam to update.
//!
//! Initial-prompt delivery (verified 2026-05-30 against `--help` for all
//! supported agents): the prompt is passed via argv so the backend never types into
//! the live PTY. Claude and Codex take it as a trailing positional
//! (`claude/codex [OPTIONS] [PROMPT]`, interactive by default); agy needs
//! `--prompt-interactive <prompt>` (it rejects a bare positional);
//! Copilot uses `-i <prompt>`; OpenCode uses `--prompt <prompt>`. See
//! `PromptDelivery`.
//!
//! Auto Mode args were verified on 2026-06-01. Claude and Antigravity
//! were checked against local `--help`; Codex and Copilot were checked
//! against their official CLI docs. OpenCode stays unsupported here until
//! the installed CLI help exposes a stable auto-approval flag.

use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
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
    /// Whether the caller should kick off the OpenCode async session-id
    /// capture task to find the generated `ses_*` id.
    pub pending_opencode_capture: Option<OpenCodeCapture>,
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

#[derive(Clone, Debug)]
pub struct OpenCodeCapture {
    pub command: String,
    pub cwd: PathBuf,
    pub before_ids: BTreeSet<String>,
    pub started_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct OpenCodeSessionInfo {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_i64")]
    pub updated: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_optional_i64")]
    pub created: Option<i64>,
    #[serde(default, rename = "projectId")]
    pub project_id: Option<String>,
    #[serde(default)]
    pub directory: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LaunchOptions {
    pub auto_mode: bool,
}

/// Choose the binary name for `kind` when the user hasn't overridden
/// the path in `Config.agente.command` / `Project.agente.command`.
pub fn default_command(kind: AgenteKind) -> &'static str {
    match kind {
        AgenteKind::ClaudeCode => "claude",
        AgenteKind::Codex => "codex",
        AgenteKind::Copilot => "copilot",
        AgenteKind::Antigravity => "agy",
        AgenteKind::OpenCode => "opencode",
    }
}

pub fn supports_auto_mode(kind: AgenteKind) -> bool {
    auto_mode_args(kind).is_some()
}

fn auto_mode_args(kind: AgenteKind) -> Option<&'static [&'static str]> {
    match kind {
        // `claude --help` exposes `--permission-mode <mode>` with `auto`
        // plus the `auto-mode` subcommand for classifier configuration.
        AgenteKind::ClaudeCode => Some(&["--permission-mode", "auto"]),
        // OpenAI Codex CLI docs list `--full-auto` as the full automatic
        // approval/sandbox mode.
        AgenteKind::Codex => Some(&["--full-auto"]),
        // GitHub docs recommend autopilot with all permissions for
        // unattended Copilot CLI work; `--no-ask-user` suppresses
        // clarifying-question stops.
        AgenteKind::Copilot => Some(&["--mode", "autopilot", "--yolo", "--no-ask-user"]),
        // `agy --help` documents this as auto-approving tool permissions.
        AgenteKind::Antigravity => Some(&["--dangerously-skip-permissions"]),
        AgenteKind::OpenCode => None,
    }
}

fn initial_args(kind: AgenteKind, options: LaunchOptions) -> Result<Vec<String>, String> {
    if !options.auto_mode {
        return Ok(Vec::new());
    }
    let args = auto_mode_args(kind).ok_or_else(|| {
        format!(
            "Auto Mode is not supported for agent '{}'",
            default_command(kind)
        )
    })?;
    Ok(args.iter().map(|arg| (*arg).to_string()).collect())
}

/// Per-agent install detection: whether the agent's CLI binary lives
/// on `PATH` and/or its dotfile directory exists under `$HOME`.
///
/// `installed` is `on_path || has_config_dir || located` — the UI considers
/// the agent usable if any signal is true. We expose the underlying
/// cheap signals so the UI can show a tooltip explaining what's missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct AgentPresence {
    pub kind: AgenteKind,
    pub installed: bool,
    pub on_path: bool,
    pub has_config_dir: bool,
    pub supports_auto_mode: bool,
}

/// Return install presence for every supported agent. Stable order:
/// Claude Code, Codex, Antigravity, OpenCode, Copilot.
pub fn list_installed_agents() -> Vec<AgentPresence> {
    [
        AgenteKind::ClaudeCode,
        AgenteKind::Codex,
        AgenteKind::Antigravity,
        AgenteKind::OpenCode,
        AgenteKind::Copilot,
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
        supports_auto_mode: supports_auto_mode(kind),
    }
}

fn config_dir_for(kind: AgenteKind) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(match kind {
        AgenteKind::ClaudeCode => home.join(".claude"),
        AgenteKind::Codex => home.join(".codex"),
        AgenteKind::Copilot => home.join(".copilot"),
        // TODO(agy-verify): confirm the dir `agy` actually creates.
        // Docs point at `~/.gemini/antigravity-cli` (skills/MCP config)
        // and `~/.config/antigravity` (config.toml). `.gemini` is the
        // one tied to agent state, so prefer it for presence detection.
        AgenteKind::Antigravity => home.join(".gemini").join("antigravity-cli"),
        AgenteKind::OpenCode => home.join(".config").join("opencode"),
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

struct AgentPlanContext<'a> {
    model: &'a str,
    cwd: &'a Path,
    task_id: &'a str,
    project_id: &'a str,
    existing_conversation_id: Option<&'a str>,
    initial_prompt: Option<&'a str>,
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
    plan_launch_with_options(
        kind,
        model,
        command_override,
        cwd,
        task_id,
        project_id,
        existing_conversation_id,
        initial_prompt,
        LaunchOptions::default(),
    )
    .expect("default launch options must always be valid")
}

#[allow(clippy::too_many_arguments)]
pub fn plan_launch_with_options(
    kind: AgenteKind,
    model: &str,
    command_override: Option<&Path>,
    cwd: &Path,
    task_id: &str,
    project_id: &str,
    existing_conversation_id: Option<&str>,
    initial_prompt: Option<&str>,
    options: LaunchOptions,
) -> Result<LaunchPlan, String> {
    let command = command_override
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| default_command(kind).to_string());

    let model = model.trim();
    let args = initial_args(kind, options)?;
    let ctx = AgentPlanContext {
        model,
        cwd,
        task_id,
        project_id,
        existing_conversation_id,
        initial_prompt,
    };

    let plan = match kind {
        AgenteKind::ClaudeCode => plan_claude(command, args, &ctx),
        AgenteKind::Codex => plan_codex(command, args, &ctx),
        AgenteKind::Copilot => plan_copilot(command, args, &ctx),
        AgenteKind::Antigravity => plan_antigravity(command, args, &ctx),
        AgenteKind::OpenCode => plan_opencode(command, args, &ctx),
    };
    Ok(plan)
}

fn plan_copilot(command: String, mut args: Vec<String>, ctx: &AgentPlanContext<'_>) -> LaunchPlan {
    let conversation_id = match ctx.existing_conversation_id {
        Some(id) => id.to_string(),
        None => Uuid::new_v4().to_string(),
    };

    args.push("--session-id".to_string());
    args.push(conversation_id.clone());

    if ctx.existing_conversation_id.is_none() {
        if !ctx.model.is_empty() {
            args.push("--model".to_string());
            args.push(ctx.model.to_string());
        }
        if let Some(prompt) = ctx.initial_prompt {
            args.push("-i".to_string());
            args.push(prompt.to_string());
        }
    }

    let cfg = SpawnConfig::new(command)
        .args(args)
        .cwd(ctx.cwd)
        .size(DEFAULT_COLS, DEFAULT_ROWS)
        .cadenza_env(ctx.project_id, ctx.task_id, &conversation_id);

    LaunchPlan {
        spawn: cfg,
        conversation_id_known: Some(conversation_id),
        pending_codex_capture: None,
        pending_opencode_capture: None,
        prompt_delivery: PromptDelivery::Argv,
    }
}

fn plan_claude(command: String, mut args: Vec<String>, ctx: &AgentPlanContext<'_>) -> LaunchPlan {
    let conversation_id = match ctx.existing_conversation_id {
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

    if !ctx.model.is_empty() {
        args.push("--model".to_string());
        args.push(ctx.model.to_string());
    }

    // Claude takes the initial prompt as a trailing positional argument and
    // stays interactive (it only goes non-interactive with `-p/--print`).
    // Verified against `claude --help`: "Usage: claude [options] [command]
    // [prompt]". Baking it here means the backend never types into the PTY.
    if let Some(prompt) = ctx.initial_prompt {
        args.push(prompt.to_string());
    }

    let cfg = SpawnConfig::new(command)
        .args(args)
        .cwd(ctx.cwd)
        .size(DEFAULT_COLS, DEFAULT_ROWS)
        .cadenza_env(ctx.project_id, ctx.task_id, &conversation_id);

    LaunchPlan {
        spawn: cfg,
        conversation_id_known: Some(conversation_id),
        pending_codex_capture: None,
        pending_opencode_capture: None,
        prompt_delivery: PromptDelivery::Argv,
    }
}

fn plan_codex(command: String, mut args: Vec<String>, ctx: &AgentPlanContext<'_>) -> LaunchPlan {
    match ctx.existing_conversation_id {
        Some(id) => {
            args.push("resume".to_string());
            args.push(id.to_string());
            // Don't pass --model on resume: codex pins the model to the
            // saved session. Passing it would either be ignored or
            // (worse) silently break the resume on some versions.
        }
        None => {
            if !ctx.model.is_empty() {
                args.push("--model".to_string());
                args.push(ctx.model.to_string());
            }
        }
    };

    // Codex takes the initial prompt as a trailing positional argument and
    // starts the interactive TUI by default (the `exec` subcommand is the
    // non-interactive one). Verified against `codex --help`: "Usage: codex
    // [OPTIONS] [PROMPT]". Only on a fresh start — a resume uses the
    // `resume <id>` subcommand and carries its own context, so the caller
    // passes no prompt there.
    if let Some(prompt) = ctx.initial_prompt {
        args.push(prompt.to_string());
    }

    // CLAUDE_SESSION_ID env var still helps the cadenza-cli skill: the
    // user can re-run `cadenza-cli current` from inside codex and the
    // mapping still works. We seed it with whatever id we know — empty
    // string when we don't (Codex first run).
    let conv_seed = ctx.existing_conversation_id.unwrap_or("");

    let cfg = SpawnConfig::new(command)
        .args(args)
        .cwd(ctx.cwd)
        .size(DEFAULT_COLS, DEFAULT_ROWS)
        .cadenza_env(ctx.project_id, ctx.task_id, conv_seed);

    let pending_capture = if ctx.existing_conversation_id.is_none() {
        Some(CodexCapture {
            sessions_root: codex_sessions_root(),
            started_at: SystemTime::now(),
        })
    } else {
        None
    };

    LaunchPlan {
        spawn: cfg,
        conversation_id_known: ctx.existing_conversation_id.map(String::from),
        pending_codex_capture: pending_capture,
        pending_opencode_capture: None,
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
    mut args: Vec<String>,
    ctx: &AgentPlanContext<'_>,
) -> LaunchPlan {
    match ctx.existing_conversation_id {
        Some(id) => {
            // Resume by id. Per `agy --help`, `--conversation <id>`
            // resumes a previous conversation. Skip --model on resume for
            // the same reason as Codex: the model is pinned to the saved
            // session.
            args.push("--conversation".to_string());
            args.push(id.to_string());
        }
        None => {
            if !ctx.model.is_empty() {
                args.push("--model".to_string());
                args.push(ctx.model.to_string());
            }
        }
    };

    // agy takes the initial prompt via `--prompt-interactive <prompt>`,
    // which "runs an initial prompt interactively and continue[s] the
    // session" (verified against `agy --help`). It does NOT accept a bare
    // positional prompt, so the flag is required. Only on a fresh start —
    // a resume carries its own context.
    if let Some(prompt) = ctx.initial_prompt {
        args.push("--prompt-interactive".to_string());
        args.push(prompt.to_string());
    }

    // Seed CLAUDE_SESSION_ID for the cadenza-cli skill the same way as
    // the other agents — empty on a fresh `agy` run (id not yet known).
    let conv_seed = ctx.existing_conversation_id.unwrap_or("");

    let cfg = SpawnConfig::new(command)
        .args(args)
        .cwd(ctx.cwd)
        .size(DEFAULT_COLS, DEFAULT_ROWS)
        .cadenza_env(ctx.project_id, ctx.task_id, conv_seed);

    let pending_capture = if ctx.existing_conversation_id.is_none() {
        Some(CodexCapture {
            sessions_root: antigravity_sessions_root(),
            started_at: SystemTime::now(),
        })
    } else {
        None
    };

    LaunchPlan {
        spawn: cfg,
        conversation_id_known: ctx.existing_conversation_id.map(String::from),
        pending_codex_capture: pending_capture,
        pending_opencode_capture: None,
        prompt_delivery: PromptDelivery::Argv,
    }
}

fn plan_opencode(command: String, mut args: Vec<String>, ctx: &AgentPlanContext<'_>) -> LaunchPlan {
    match ctx.existing_conversation_id {
        Some(id) => {
            args.push("--session".to_string());
            args.push(id.to_string());
            // Resume keeps the saved session's model/context. Do not pass
            // --model or --prompt on resume.
        }
        None => {
            if !ctx.model.is_empty() {
                args.push("--model".to_string());
                args.push(ctx.model.to_string());
            }
            if let Some(prompt) = ctx.initial_prompt {
                args.push("--prompt".to_string());
                args.push(prompt.to_string());
            }
        }
    };

    let conv_seed = ctx.existing_conversation_id.unwrap_or("");

    let cfg = SpawnConfig::new(command.clone())
        .args(args)
        .cwd(ctx.cwd)
        .size(DEFAULT_COLS, DEFAULT_ROWS)
        .cadenza_env(ctx.project_id, ctx.task_id, conv_seed);

    // The "before" session snapshot is filled in by the caller via
    // `snapshot_opencode_session_ids` on the blocking pool — collecting it
    // here would run a synchronous `opencode session list` subprocess
    // inside `plan_launch`, which executes on the async runtime thread and
    // would stall it. Keep the planner pure (no I/O).
    let pending_capture = if ctx.existing_conversation_id.is_none() {
        Some(OpenCodeCapture {
            command,
            cwd: ctx.cwd.to_path_buf(),
            before_ids: BTreeSet::new(),
            started_at_ms: chrono::Utc::now().timestamp_millis(),
        })
    } else {
        None
    };

    LaunchPlan {
        spawn: cfg,
        conversation_id_known: ctx.existing_conversation_id.map(String::from),
        pending_codex_capture: None,
        pending_opencode_capture: pending_capture,
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

pub fn find_opencode_session_id(capture: &OpenCodeCapture) -> Option<String> {
    let sessions = collect_opencode_sessions(&capture.command, &capture.cwd, 50).ok()?;
    pick_new_opencode_session(capture, &sessions)
}

/// Snapshot the set of existing OpenCode session ids before a fresh
/// launch, so the post-spawn poll can tell the new session apart from
/// pre-existing ones. Runs `opencode session list` (a blocking
/// subprocess), so callers must invoke it off the async runtime (e.g.
/// `spawn_blocking`). Failures degrade to an empty set — the poll then
/// falls back to timestamp + directory matching.
pub fn snapshot_opencode_session_ids(command: &str, cwd: &Path) -> BTreeSet<String> {
    collect_opencode_sessions(command, cwd, 50)
        .map(|sessions| sessions.into_iter().map(|s| s.id).collect())
        .unwrap_or_default()
}

pub fn pick_new_opencode_session(
    capture: &OpenCodeCapture,
    sessions: &[OpenCodeSessionInfo],
) -> Option<String> {
    let slack_ms = 5_000;
    let threshold = capture.started_at_ms.saturating_sub(slack_ms);
    let mut candidates: Vec<&OpenCodeSessionInfo> = sessions
        .iter()
        .filter(|s| !capture.before_ids.contains(&s.id))
        .filter(|s| {
            s.created
                .or(s.updated)
                .map(|ts| ts >= threshold)
                .unwrap_or(true)
        })
        .collect();

    candidates.sort_by_key(|s| std::cmp::Reverse(s.updated.or(s.created).unwrap_or(0)));

    if let Some(session) = candidates
        .iter()
        .copied()
        .find(|s| session_directory_matches(s.directory.as_deref(), &capture.cwd))
    {
        return Some(session.id.clone());
    }

    // No session matched our cwd (every candidate reports a *different*
    // known directory). If exactly one new session appeared it is almost
    // certainly ours, so bind it; but if several appeared and none match,
    // refuse to guess — picking the wrong `ses_*` would silently resume an
    // unrelated conversation. Better to leave conversation_id unset.
    match candidates.as_slice() {
        [only] => Some(only.id.clone()),
        _ => None,
    }
}

fn session_directory_matches(directory: Option<&str>, cwd: &Path) -> bool {
    let Some(directory) = directory else {
        return true;
    };
    normalize_pathish(directory) == normalize_pathish(&cwd.to_string_lossy())
}

fn normalize_pathish(path: &str) -> String {
    let normalized = path.replace('\\', "/").trim_end_matches('/').to_string();
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

pub fn parse_opencode_sessions_json(text: &str) -> anyhow::Result<Vec<OpenCodeSessionInfo>> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    let array = if let Some(arr) = value.as_array() {
        arr.clone()
    } else if let Some(arr) = value.get("sessions").and_then(|v| v.as_array()) {
        arr.clone()
    } else {
        return Err(anyhow::anyhow!("expected OpenCode session JSON array"));
    };
    Ok(serde_json::from_value(serde_json::Value::Array(array))?)
}

fn collect_opencode_sessions(
    command: &str,
    cwd: &Path,
    max_count: u32,
) -> anyhow::Result<Vec<OpenCodeSessionInfo>> {
    let (resolved, prefix_args) = crate::spawn::resolve_command(command);
    let mut cmd = Command::new(&resolved);
    cmd.args(prefix_args);
    cmd.args(["session", "list", "--format", "json", "--max-count"]);
    cmd.arg(max_count.to_string());
    cmd.current_dir(cwd);
    cmd.env_clear();
    for name in crate::spawn::FORWARD_ENV_ALLOWLIST {
        if let Some(val) = std::env::var_os(name) {
            cmd.env(name, val);
        }
    }
    cmd.env("PATH", crate::spawn::search_path());

    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "opencode session list failed ({}): {}",
            output.status,
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok(Vec::new());
    }
    parse_opencode_sessions_json(&stdout)
}

fn deserialize_optional_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(value) = value else {
        return Ok(None);
    };
    match value {
        serde_json::Value::Number(n) => Ok(n.as_i64()),
        serde_json::Value::String(s) => {
            s.parse::<i64>().map(Some).map_err(serde::de::Error::custom)
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
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
    fn auto_mode_disabled_omits_codex_flag() {
        let plan = plan_launch_with_options(
            AgenteKind::Codex,
            "gpt-5.4",
            None,
            Path::new("/tmp/proj"),
            "T-2",
            "proj-b",
            None,
            Some("do the task"),
            LaunchOptions { auto_mode: false },
        )
        .unwrap();
        assert!(!plan.spawn.args.iter().any(|a| a == "--full-auto"));
        assert_eq!(
            plan.spawn.args.last().map(String::as_str),
            Some("do the task")
        );
    }

    #[test]
    fn auto_mode_enabled_prefixes_codex_resume_args() {
        let plan = plan_launch_with_options(
            AgenteKind::Codex,
            "gpt-5.4",
            None,
            Path::new("/tmp/proj"),
            "T-2",
            "proj-b",
            Some("019d4891-0feb-78a2-8f90-841686dc0175"),
            None,
            LaunchOptions { auto_mode: true },
        )
        .unwrap();
        assert_eq!(plan.spawn.args[0], "--full-auto");
        assert_eq!(plan.spawn.args[1], "resume");
        assert_eq!(plan.spawn.args[2], "019d4891-0feb-78a2-8f90-841686dc0175");
    }

    #[test]
    fn auto_mode_rejects_unsupported_agent() {
        let result = plan_launch_with_options(
            AgenteKind::OpenCode,
            "anthropic/claude-sonnet-4-6",
            None,
            Path::new("/tmp/proj"),
            "T-4",
            "proj-d",
            None,
            Some("do the task"),
            LaunchOptions { auto_mode: true },
        );
        let err = match result {
            Ok(_) => panic!("opencode has no declared Auto Mode support"),
            Err(err) => err,
        };
        assert!(err.contains("Auto Mode is not supported"));
    }

    #[test]
    fn copilot_new_session_passes_session_id_model_and_prompt() {
        let plan = plan_launch(
            AgenteKind::Copilot,
            "gpt-5.2",
            None,
            Path::new("/tmp/proj"),
            "T-21",
            "proj-copilot",
            None,
            Some("do the task"),
        );
        assert_eq!(plan.spawn.command, "copilot");
        let args = &plan.spawn.args;
        let s = args
            .iter()
            .position(|a| a == "--session-id")
            .expect("--session-id");
        let uuid = &args[s + 1];
        assert_eq!(uuid.len(), 36, "uuid arg should be 36 chars, got {uuid}");
        let m = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(args[m + 1], "gpt-5.2");
        let p = args.iter().position(|a| a == "-i").expect("-i");
        assert_eq!(args[p + 1], "do the task");
        assert_eq!(plan.conversation_id_known.as_deref(), Some(uuid.as_str()));
        assert!(plan.pending_codex_capture.is_none());
        assert!(plan.pending_opencode_capture.is_none());
        assert_eq!(plan.prompt_delivery, PromptDelivery::Argv);
    }

    #[test]
    fn copilot_resume_uses_session_id_and_skips_model_prompt() {
        let plan = plan_launch(
            AgenteKind::Copilot,
            "gpt-5.2",
            None,
            Path::new("/tmp/proj"),
            "T-21",
            "proj-copilot",
            Some("019d4891-0feb-78a2-8f90-841686dc0175"),
            Some("ignored"),
        );
        let args = &plan.spawn.args;
        let s = args
            .iter()
            .position(|a| a == "--session-id")
            .expect("--session-id");
        assert_eq!(args[s + 1], "019d4891-0feb-78a2-8f90-841686dc0175");
        assert!(!args.iter().any(|a| a == "--model"));
        assert!(!args.iter().any(|a| a == "-i"));
        assert_eq!(
            plan.conversation_id_known.as_deref(),
            Some("019d4891-0feb-78a2-8f90-841686dc0175")
        );
        assert!(plan.pending_codex_capture.is_none());
        assert!(plan.pending_opencode_capture.is_none());
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
    fn opencode_new_session_passes_model_and_prompt() {
        let plan = plan_launch(
            AgenteKind::OpenCode,
            "anthropic/claude-sonnet-4-6",
            Some(Path::new("definitely-not-opencode")),
            Path::new("/tmp/proj"),
            "T-4",
            "proj-d",
            None,
            Some("do the task"),
        );
        assert_eq!(plan.spawn.command, "definitely-not-opencode");
        let args = &plan.spawn.args;
        let m = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(args[m + 1], "anthropic/claude-sonnet-4-6");
        let p = args.iter().position(|a| a == "--prompt").expect("--prompt");
        assert_eq!(args[p + 1], "do the task");
        assert!(plan.conversation_id_known.is_none());
        assert!(plan.pending_codex_capture.is_none());
        assert!(plan.pending_opencode_capture.is_some());
    }

    #[test]
    fn opencode_new_session_without_model_omits_model() {
        let plan = plan_launch(
            AgenteKind::OpenCode,
            "",
            Some(Path::new("definitely-not-opencode")),
            Path::new("/tmp/proj"),
            "T-4",
            "proj-d",
            None,
            None,
        );
        assert!(!plan.spawn.args.iter().any(|a| a == "--model"));
        assert!(plan.pending_opencode_capture.is_some());
    }

    #[test]
    fn opencode_resume_uses_session_flag_and_skips_model_prompt() {
        let plan = plan_launch(
            AgenteKind::OpenCode,
            "anthropic/claude-sonnet-4-6",
            None,
            Path::new("/tmp/proj"),
            "T-4",
            "proj-d",
            Some("ses_2132323b6ffeuRlYHhPcU8DaZ6"),
            Some("ignored"),
        );
        let args = &plan.spawn.args;
        let s = args
            .iter()
            .position(|a| a == "--session")
            .expect("--session");
        assert_eq!(args[s + 1], "ses_2132323b6ffeuRlYHhPcU8DaZ6");
        assert!(!args.iter().any(|a| a == "--model"));
        assert!(!args.iter().any(|a| a == "--prompt"));
        assert!(plan.pending_opencode_capture.is_none());
        assert_eq!(
            plan.conversation_id_known.as_deref(),
            Some("ses_2132323b6ffeuRlYHhPcU8DaZ6")
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
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].kind, AgenteKind::ClaudeCode);
        assert_eq!(rows[1].kind, AgenteKind::Codex);
        assert_eq!(rows[2].kind, AgenteKind::Antigravity);
        assert_eq!(rows[3].kind, AgenteKind::OpenCode);
        assert_eq!(rows[4].kind, AgenteKind::Copilot);
        for row in &rows {
            // `installed` may also be set by the off-PATH binary locator,
            // so it's a superset of the on_path/config-dir signals.
            if row.on_path || row.has_config_dir {
                assert!(row.installed);
            }
        }
        assert!(rows[0].supports_auto_mode);
        assert!(rows[1].supports_auto_mode);
        assert!(rows[2].supports_auto_mode);
        assert!(!rows[3].supports_auto_mode);
        assert!(rows[4].supports_auto_mode);
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

    #[test]
    fn parse_opencode_session_list_json_fixture() {
        let json = r#"[
          {
            "id": "ses_old",
            "title": "Old session",
            "updated": 1780000000000,
            "created": 1780000000000,
            "projectId": "proj-old",
            "directory": "/tmp/proj"
          },
          {
            "id": "ses_new",
            "title": "New session",
            "updated": "1780000100000",
            "created": "1780000100000",
            "projectId": "proj-new",
            "directory": "/tmp/proj"
          }
        ]"#;
        let sessions = parse_opencode_sessions_json(json).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[1].id, "ses_new");
        assert_eq!(sessions[1].created, Some(1_780_000_100_000));
        assert_eq!(sessions[1].project_id.as_deref(), Some("proj-new"));
    }

    #[test]
    fn pick_new_opencode_session_prefers_new_matching_directory() {
        let mut before_ids = BTreeSet::new();
        before_ids.insert("ses_old".to_string());
        let capture = OpenCodeCapture {
            command: "opencode".to_string(),
            cwd: PathBuf::from("/tmp/proj"),
            before_ids,
            started_at_ms: 1_780_000_000_000,
        };
        let sessions = vec![
            OpenCodeSessionInfo {
                id: "ses_elsewhere".to_string(),
                title: None,
                updated: Some(1_780_000_100_000),
                created: Some(1_780_000_100_000),
                project_id: None,
                directory: Some("/tmp/other".to_string()),
            },
            OpenCodeSessionInfo {
                id: "ses_new".to_string(),
                title: None,
                updated: Some(1_780_000_090_000),
                created: Some(1_780_000_090_000),
                project_id: None,
                directory: Some("/tmp/proj".to_string()),
            },
        ];
        assert_eq!(
            pick_new_opencode_session(&capture, &sessions).as_deref(),
            Some("ses_new")
        );
    }
}
