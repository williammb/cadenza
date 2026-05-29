//! PTY spawning via `portable-pty` (ConPTY on Windows, fork/exec on
//! Unix).
//!
//! Per DESIGN-desktop-v2.md § "spawn.rs". Env vars injected on every
//! spawn: `TASKAI_PROJECT_ID`, `TASKAI_TASK_ID`, `CLAUDE_SESSION_ID`.
//!
//! Wired into Tauri commands in Phase 3.
#![allow(dead_code)]

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, ExitStatus, MasterPty, PtyPair, PtySize};
use std::io::{Read, Write};
use std::path::PathBuf;

/// Inputs to `PtyHandle::spawn`.
pub struct SpawnConfig {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

impl SpawnConfig {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
        }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, p: impl Into<PathBuf>) -> Self {
        self.cwd = Some(p.into());
        self
    }

    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }

    pub fn size(mut self, cols: u16, rows: u16) -> Self {
        self.cols = cols;
        self.rows = rows;
        self
    }

    /// Inject the standard Cadenza env vars. `project_id` / `task_id`
    /// are required by the existing Node skill; `claude_session_id` is
    /// the per-run identifier the agent uses.
    ///
    /// Also prepends the directory containing the running Cadenza
    /// executable to PATH so the agent can find `cadenza-cli` without
    /// the user having to install it separately — both binaries ship
    /// from the same install root.
    pub fn cadenza_env(mut self, project_id: &str, task_id: &str, claude_session_id: &str) -> Self {
        self.env
            .push(("TASKAI_PROJECT_ID".into(), project_id.into()));
        self.env.push(("TASKAI_TASK_ID".into(), task_id.into()));
        self.env
            .push(("CLAUDE_SESSION_ID".into(), claude_session_id.into()));
        if let Some(path) = cli_augmented_path() {
            self.env.push(("PATH".into(), path));
        }
        self
    }

    /// Vars adicionais para fluxo "destrinchar ideia": além das vars
    /// padrão (`cadenza_env`), seta `CADENZA_IDEIA_ID` e
    /// `CADENZA_IDEIA_BODY` para o agente saber qual ideia decompor.
    /// O `task_id` passado para `cadenza_env` deve ser um placeholder
    /// estável (ex. `IDEIA-<id>`) — usado só para logs/tracing.
    pub fn ideia_env(mut self, ideia_id: &str, ideia_body: &str) -> Self {
        self.env.push(("CADENZA_IDEIA_ID".into(), ideia_id.into()));
        self.env
            .push(("CADENZA_IDEIA_BODY".into(), ideia_body.into()));
        self
    }
}

/// Parent-process env vars safe to inherit into spawned agents. Names
/// not on this list — notably anything ending in `_KEY` / `_TOKEN` /
/// `_SECRET`, plus `AWS_*` / `GOOGLE_*` / API-key-shaped vars — are
/// dropped via `CommandBuilder::env_clear` so the agent never sees
/// them by accident. Cadenza adds its own vars (TASKAI_*,
/// CLAUDE_SESSION_ID, augmented PATH) on top through `cadenza_env`.
pub(crate) const FORWARD_ENV_ALLOWLIST: &[&str] = &[
    // Path resolution.
    "PATH",
    "PATHEXT",
    // Home + temp.
    "HOME",
    "USERPROFILE",
    "TEMP",
    "TMP",
    "TMPDIR",
    // Locale + tty.
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_TIME",
    "LC_COLLATE",
    "LC_NUMERIC",
    "LC_MONETARY",
    "TZ",
    "TERM",
    // User identity.
    "USER",
    "USERNAME",
    "LOGNAME",
    "SHELL",
    // Windows essentials.
    "SystemRoot",
    "SystemDrive",
    "COMSPEC",
    "WINDIR",
    // Per-user / system app data dirs (npm, Codex, Claude Code use these).
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "PROCESSOR_ARCHITECTURE",
    // Node / npm.
    "NODE_PATH",
    "NPM_CONFIG_PREFIX",
];

/// Directories where CLI tools are commonly installed but which a GUI
/// app does NOT inherit when launched from Finder/Dock (macOS) or a
/// desktop launcher (Linux) — there the PATH is the bare
/// `/usr/bin:/bin:/usr/sbin:/sbin`, so `claude` / `codex` (Homebrew,
/// npm-global, native installers) can't be found at all. We merge these
/// into the PATH used for resolution and into the PATH handed to spawned
/// agents, so resolution matches the user's terminal. Only existing
/// directories are returned. Cross-platform; the Windows set is small
/// since GUI processes there normally inherit the full user PATH.
fn extra_path_dirs() -> Vec<PathBuf> {
    let mut dirs_list: Vec<PathBuf> = Vec::new();
    let home = dirs::home_dir();

    #[cfg(windows)]
    {
        if let Some(data) = dirs::data_dir() {
            // npm global prefix (%APPDATA%\npm).
            dirs_list.push(data.join("npm"));
        }
        if let Some(h) = home.as_ref() {
            dirs_list.push(h.join(".cargo").join("bin"));
        }
    }

    #[cfg(not(windows))]
    {
        // Homebrew (Apple Silicon + Intel) and system-local.
        for p in [
            "/opt/homebrew/bin",
            "/opt/homebrew/sbin",
            "/usr/local/bin",
            "/usr/local/sbin",
        ] {
            dirs_list.push(PathBuf::from(p));
        }
        if let Some(h) = home.as_ref() {
            // Per-user tool dirs + native-installer / migrate targets.
            dirs_list.push(h.join(".local").join("bin"));
            dirs_list.push(h.join(".cargo").join("bin"));
            dirs_list.push(h.join(".bun").join("bin"));
            dirs_list.push(h.join(".deno").join("bin"));
            dirs_list.push(h.join(".volta").join("bin"));
            dirs_list.push(h.join(".npm-global").join("bin"));
            dirs_list.push(h.join(".claude").join("local"));
            dirs_list.push(h.join(".codex").join("bin"));
        }
    }

    dirs_list.into_iter().filter(|p| p.is_dir()).collect()
}

/// PATH used to resolve agent binaries: the inherited process PATH plus
/// `extra_path_dirs()`, de-duplicated and order-preserving (inherited
/// entries first). On a GUI launch with a stripped PATH this is what lets
/// `claude` / `codex` be found at all.
pub(crate) fn search_path() -> std::ffi::OsString {
    let mut entries: Vec<PathBuf> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let from_env = std::env::var_os("PATH");
    if let Some(existing) = &from_env {
        for p in std::env::split_paths(existing) {
            if seen.insert(p.to_string_lossy().into_owned()) {
                entries.push(p);
            }
        }
    }
    for p in extra_path_dirs() {
        if seen.insert(p.to_string_lossy().into_owned()) {
            entries.push(p);
        }
    }
    std::env::join_paths(&entries).unwrap_or_else(|_| from_env.unwrap_or_default())
}

/// Build a PATH value that puts the directory holding the current
/// Cadenza executable at the front (so `cadenza-cli` resolves without any
/// user setup), followed by the augmented `search_path()` (so the spawned
/// agent finds its own sub-tools the same way the user's terminal does).
/// Returns `None` if `current_exe` isn't reachable or has no parent, in
/// which case the caller leaves PATH inherited.
fn cli_augmented_path() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.to_path_buf();
    let dir_str = dir.to_string_lossy().into_owned();
    let mut entries: Vec<PathBuf> = vec![dir];
    for p in std::env::split_paths(&search_path()) {
        // Skip duplicates so re-spawns don't grow PATH unboundedly.
        if p.to_string_lossy() != dir_str {
            entries.push(p);
        }
    }
    std::env::join_paths(entries)
        .ok()
        .map(|os| os.to_string_lossy().into_owned())
}

/// Locate an agent binary that isn't on `PATH` by searching the agent's
/// standard install locations. Returns the newest matching executable (by
/// mtime) or `None`. Keyed on the bare agent name, so it never alters how
/// arbitrary commands resolve.
///
/// Currently handles the OpenAI Codex standalone installer, whose binary
/// lives in a content-addressed store
/// (`.../OpenAI/Codex/bin/<hash>/codex[.exe]`) that is not added to PATH
/// and whose hash changes on every update — so a user-set fixed path would
/// break on the next upgrade. Cross-platform: roots come from the
/// platform data-local dir and `~/.codex`.
pub(crate) fn locate_agent_binary(command: &str) -> Option<PathBuf> {
    let stem = std::path::Path::new(command)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(command)
        .to_ascii_lowercase();

    let mut roots: Vec<PathBuf> = Vec::new();
    let exe_name: &str = match stem.as_str() {
        "codex" => {
            if let Some(home) = dirs::home_dir() {
                roots.push(home.join(".codex"));
            }
            if let Some(data) = dirs::data_local_dir() {
                // Windows: %LOCALAPPDATA%\OpenAI\Codex
                // macOS:   ~/Library/Application Support/OpenAI/Codex
                // Linux:   $XDG_DATA_HOME (or ~/.local/share)/OpenAI/Codex
                roots.push(data.join("OpenAI").join("Codex"));
            }
            if cfg!(windows) {
                "codex.exe"
            } else {
                "codex"
            }
        }
        "agy" => {
            // TODO(agy-verify): confirm the `agy` install layout. The
            // installer is documented to drop the binary under
            // `~/.local/bin` (Unix) or `%LOCALAPPDATA%\Antigravity\`
            // (Windows). locate_in_roots also probes `<root>/bin`, so the
            // home root covers the `~/.local/bin` case.
            if let Some(home) = dirs::home_dir() {
                roots.push(home.join(".local"));
                roots.push(home.join(".gemini").join("antigravity-cli"));
            }
            if let Some(data) = dirs::data_local_dir() {
                roots.push(data.join("Antigravity"));
            }
            if cfg!(windows) {
                "agy.exe"
            } else {
                "agy"
            }
        }
        _ => return None,
    };

    locate_in_roots(&roots, exe_name)
}

/// Search each root — the root itself, `root/bin`, and every immediate
/// subdir of `root/bin` (the content-addressed store) — for `exe_name`,
/// returning the candidate with the newest modification time. Split out
/// from `locate_agent_binary` so tests can supply synthetic roots.
fn locate_in_roots(roots: &[PathBuf], exe_name: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for root in roots {
        candidates.push(root.join(exe_name));
        let bin = root.join("bin");
        candidates.push(bin.join(exe_name));
        if let Ok(entries) = std::fs::read_dir(&bin) {
            for entry in entries.flatten() {
                let sub = entry.path();
                if sub.is_dir() {
                    candidates.push(sub.join(exe_name));
                }
            }
        }
    }
    candidates
        .into_iter()
        .filter(|p| p.is_file())
        .max_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        })
}

/// Returns `(executable, prefix_args)`. On Windows, batch files (.cmd,
/// .bat) are not valid Win32 executables — `CreateProcessW` fails with
/// 193 unless we invoke them through `cmd.exe /C <path>`. This helper
/// resolves bare command names against PATH (preferring real binaries
/// over batch shims), and only wraps with cmd.exe when the resolved
/// target is a batch file. A bare name not found on PATH falls back to
/// `locate_agent_binary` (e.g. the off-PATH OpenAI Codex store).
#[cfg(windows)]
pub(crate) fn resolve_command(command: &str) -> (String, Vec<String>) {
    use std::env;
    use std::path::Path;

    fn cmd_wrap(path: String) -> (String, Vec<String>) {
        ("cmd.exe".to_string(), vec!["/C".to_string(), path])
    }

    let lower = command.to_ascii_lowercase();
    let has_separator = command.contains('\\') || command.contains('/');
    let p = Path::new(command);

    // Already a specific file (absolute or has separator). Respect the
    // user's intent — only insert the cmd.exe wrapper if needed because
    // they pointed us at a batch file.
    if p.is_absolute() || has_separator {
        if lower.ends_with(".cmd") || lower.ends_with(".bat") {
            return cmd_wrap(command.to_string());
        }
        return (command.to_string(), Vec::new());
    }

    // Has an extension but no separator (e.g. "codex.cmd"). Same logic.
    if lower.ends_with(".exe") || lower.ends_with(".com") {
        return (command.to_string(), Vec::new());
    }
    if lower.ends_with(".cmd") || lower.ends_with(".bat") {
        return cmd_wrap(command.to_string());
    }

    // Bare command — search the augmented PATH. Prefer real binaries
    // (.exe / .com) so we don't drag cmd.exe into the process tree
    // unnecessarily; fall back to .cmd / .bat (wrapped) which is the
    // common npm shim case.
    let path_var = search_path();
    for dir in env::split_paths(&path_var) {
        for ext in ["exe", "com"] {
            let candidate = dir.join(format!("{command}.{ext}"));
            if candidate.is_file() {
                return (candidate.to_string_lossy().into_owned(), Vec::new());
            }
        }
        for ext in ["cmd", "bat"] {
            let candidate = dir.join(format!("{command}.{ext}"));
            if candidate.is_file() {
                return cmd_wrap(candidate.to_string_lossy().into_owned());
            }
        }
    }
    // Not on PATH — try the agent's off-PATH install location.
    if let Some(found) = locate_agent_binary(command) {
        return (found.to_string_lossy().into_owned(), Vec::new());
    }
    (command.to_string(), Vec::new())
}

#[cfg(not(windows))]
pub(crate) fn resolve_command(command: &str) -> (String, Vec<String>) {
    // Absolute paths and names with a separator are handed straight to exec.
    if command.contains('/') {
        return (command.to_string(), Vec::new());
    }
    // Resolve the bare name against the augmented search PATH and return
    // an absolute path, so resolution doesn't depend on whatever PATH the
    // spawner inherits. This matters most for model discovery, which sets
    // no env at all on the spawned process — on a macOS GUI launch its
    // PATH is just `/usr/bin:/bin:/usr/sbin:/sbin`.
    for dir in std::env::split_paths(&search_path()) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(command);
        // Require the execute bit, mirroring what the shell's bare-name
        // PATH lookup does — otherwise a non-executable regular file named
        // like the agent would shadow a real binary later on the path and
        // turn a clean "not found" into a permission-denied at exec.
        if is_executable_file(&candidate) {
            return (candidate.to_string_lossy().into_owned(), Vec::new());
        }
    }
    // Off-PATH install (e.g. the OpenAI Codex content-addressed store).
    if let Some(found) = locate_agent_binary(command) {
        return (found.to_string_lossy().into_owned(), Vec::new());
    }
    (command.to_string(), Vec::new())
}

/// True if `path` is a regular file with at least one execute bit set.
/// Follows symlinks (via `metadata`), matching shell PATH resolution.
#[cfg(not(windows))]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

/// Scan `chunk` for the ConPTY Device-Status-Report query (`ESC[6n`),
/// tracking partial matches across calls via `state`, and reply with a
/// cursor-position report (`ESC[1;1R`) through `writer` when the full
/// sequence is seen. Windows ConPTY withholds the child's output until the
/// terminal answers this query at boot; xterm.js is meant to answer from the
/// webview but the reply isn't reliably delivered, so we answer here too (a
/// duplicate reply is harmless). Unix PTYs never send the query, so this is a
/// no-op there. `state` must persist across reads — any ESC (re)starts the
/// match so a stray/aborted escape can't swallow the real query.
pub(crate) fn answer_dsr_cpr(
    state: &mut u8,
    chunk: &[u8],
    writer: &std::sync::Mutex<Box<dyn Write + Send>>,
) {
    for &b in chunk {
        match (*state, b) {
            (_, 0x1B) => *state = 1,
            (1, b'[') => *state = 2,
            (2, b'6') => *state = 3,
            (3, b'n') => {
                if let Ok(mut w) = writer.lock() {
                    let _ = w.write_all(b"\x1b[1;1R");
                    let _ = w.flush();
                }
                *state = 0;
            }
            _ => *state = 0,
        }
    }
}

/// Owned PTY + child handle. The reader is cloned out via
/// `try_clone_reader` and the writer via `take_writer`; both are
/// passed to `terminal::TerminalSession`.
pub struct PtyHandle {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl PtyHandle {
    pub fn spawn(config: SpawnConfig) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair: PtyPair = pty_system
            .openpty(PtySize {
                rows: config.rows,
                cols: config.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        // On Windows, npm installs CLI tools (claude, codex…) as a triplet:
        // `<name>` (POSIX shell script), `<name>.cmd` (Windows shim), and
        // `<name>.ps1`. portable_pty's PATH search hits the extensionless
        // shell script first and hands it to CreateProcessW, which fails
        // with ERROR_BAD_EXE_FORMAT (193). And even if we found the .cmd,
        // CreateProcessW can't launch batch files directly — they need
        // `cmd.exe /C`. Resolve both problems here. Unix is untouched.
        let (resolved_command, prefix_args) = resolve_command(&config.command);
        let mut cmd = CommandBuilder::new(&resolved_command);
        for a in &prefix_args {
            cmd.arg(a);
        }
        for a in &config.args {
            cmd.arg(a);
        }
        if let Some(d) = config.cwd.as_deref() {
            cmd.cwd(d);
        }
        // Start from an empty env so API keys, OAuth tokens, AWS creds,
        // etc. from the user's shell don't leak into the spawned agent
        // (Claude Code, Codex). Only the FORWARD_ENV_ALLOWLIST vars
        // — the ones the agent genuinely needs to find binaries, home,
        // and locale — get inherited. `config.env` is applied last so
        // Cadenza-specific overrides (TASKAI_*, CLAUDE_SESSION_ID, the
        // augmented PATH from cli_augmented_path) always win.
        cmd.env_clear();
        for name in FORWARD_ENV_ALLOWLIST {
            if let Some(val) = std::env::var_os(name) {
                cmd.env(name, val);
            }
        }
        for (k, v) in &config.env {
            cmd.env(k, v);
        }

        let PtyPair { slave, master } = pair;
        let child = slave.spawn_command(cmd).context("spawn_command failed")?;
        // Drop the slave so the child's stdio is the only end with the
        // slave side; when the child closes, reads on the master see EOF.
        drop(slave);

        Ok(PtyHandle { master, child })
    }

    pub fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>> {
        self.master.try_clone_reader().context("try_clone_reader")
    }

    pub fn take_writer(&self) -> Result<Box<dyn Write + Send>> {
        self.master.take_writer().context("take_writer")
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize")
    }

    pub fn kill(&mut self) -> Result<()> {
        self.child.kill().context("kill")
    }

    pub fn wait(&mut self) -> Result<ExitStatus> {
        self.child.wait().context("wait")
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.child.try_wait().context("try_wait")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// `cmd /C echo hi` on Windows, `/bin/sh -c "echo hi"` on Unix.
    /// Both produce "hi" on stdout via the PTY.
    fn echo_hi() -> SpawnConfig {
        if cfg!(windows) {
            SpawnConfig::new("cmd").arg("/C").arg("echo hi")
        } else {
            SpawnConfig::new("/bin/sh").arg("-c").arg("echo hi")
        }
    }

    #[test]
    fn spawn_echo_and_read_output() {
        let handle = PtyHandle::spawn(echo_hi()).expect("spawn echo");
        let mut reader = handle.try_clone_reader().expect("clone reader");
        let mut writer = handle.take_writer().expect("take writer");
        // Read with a deadline — child writes immediately and exits.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        let mut answered_dsr = false;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if std::str::from_utf8(&buf).is_ok_and(|s| s.contains("hi")) {
                        break;
                    }
                    // Windows ConPTY emits a Device Status Report query
                    // (ESC[6n) on startup and withholds the program's
                    // output until the terminal answers with a cursor
                    // position report. A real terminal (xterm.js in the
                    // webview) answers automatically; the test must too,
                    // or `echo hi` never flushes. No-op on Unix PTYs,
                    // which don't send the query.
                    if !answered_dsr && buf.windows(4).any(|w| w == b"\x1b[6n") {
                        let _ = writer.write_all(b"\x1b[1;1R");
                        let _ = writer.flush();
                        answered_dsr = true;
                    }
                }
                Err(_) => break,
            }
            if Instant::now() > deadline {
                break;
            }
        }
        let out = String::from_utf8_lossy(&buf);
        assert!(
            out.contains("hi"),
            "expected 'hi' in PTY output, got: {out:?}"
        );
    }

    #[test]
    fn resize_after_spawn_doesnt_panic() {
        let handle = PtyHandle::spawn(echo_hi()).expect("spawn");
        handle.resize(120, 30).expect("resize");
    }

    #[test]
    fn cadenza_env_sets_three_vars() {
        let cfg = SpawnConfig::new("nope").cadenza_env("proj", "t-1", "sess-X");
        let mut by_key: std::collections::HashMap<&str, &str> = Default::default();
        for (k, v) in &cfg.env {
            by_key.insert(k, v);
        }
        assert_eq!(by_key.get("TASKAI_PROJECT_ID"), Some(&"proj"));
        assert_eq!(by_key.get("TASKAI_TASK_ID"), Some(&"t-1"));
        assert_eq!(by_key.get("CLAUDE_SESSION_ID"), Some(&"sess-X"));
    }

    fn codex_exe() -> &'static str {
        if cfg!(windows) {
            "codex.exe"
        } else {
            "codex"
        }
    }

    #[test]
    fn locate_in_roots_finds_nested_store_binary() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("OpenAI").join("Codex");
        let store = root.join("bin").join("deadbeefcafe");
        std::fs::create_dir_all(&store).unwrap();
        let exe = store.join(codex_exe());
        std::fs::write(&exe, b"x").unwrap();
        assert_eq!(
            locate_in_roots(&[root], codex_exe()).as_deref(),
            Some(exe.as_path()),
        );
    }

    #[test]
    fn locate_in_roots_picks_newest_by_mtime() {
        use std::fs::{File, FileTimes};
        use std::time::{Duration, UNIX_EPOCH};

        let tmp = tempfile::tempdir().expect("tempdir");
        let bin = tmp.path().join("bin");
        let old_dir = bin.join("oldhash");
        let new_dir = bin.join("newhash");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        let old_exe = old_dir.join(codex_exe());
        let new_exe = new_dir.join(codex_exe());
        std::fs::write(&old_exe, b"x").unwrap();
        std::fs::write(&new_exe, b"x").unwrap();

        // Stamp deterministic mtimes so order doesn't depend on disk timing.
        File::options()
            .write(true)
            .open(&old_exe)
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_000)))
            .unwrap();
        File::options()
            .write(true)
            .open(&new_exe)
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(2_000_000)))
            .unwrap();

        assert_eq!(
            locate_in_roots(&[tmp.path().to_path_buf()], codex_exe()).as_deref(),
            Some(new_exe.as_path()),
        );
    }

    #[test]
    fn locate_in_roots_returns_none_when_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(locate_in_roots(&[tmp.path().to_path_buf()], codex_exe()).is_none());
    }

    #[test]
    fn resolve_command_passthrough_for_unknown_bare_name() {
        // A bare name that isn't an agent and isn't installed resolves to
        // itself (no cmd.exe wrap, no locator hit) on every platform.
        let (resolved, prefix) = resolve_command("definitely-not-a-real-binary-xyz");
        assert_eq!(resolved, "definitely-not-a-real-binary-xyz");
        assert!(prefix.is_empty());
    }
}
