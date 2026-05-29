//! Per-agent model discovery via PTY spawn + `/model` TUI capture.
//!
//! Both Claude Code and Codex hide their model list inside an interactive
//! REPL menu — there is no `--list-models` flag (`agente --help` confirmed
//! 2026-05-28). To list them at runtime we spawn the agent under a PTY,
//! reply to terminal capability queries the binary blocks on (DSR /
//! cursor-position), inject `/model<Enter>`, capture the rendered bytes,
//! reconstruct the final frame with `vte`, and regex the rows.
//!
//! Runtime entry point is `discover_models`; results are cached per
//! `AgenteKind` on `AppState` so the 10-15 s PTY warm-up only happens
//! once per session per agent. Fixtures
//! (`src-tauri/testdata/models_{claude,codex}.bin`) lock the parser
//! shape and are the unit-test inputs.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};

use crate::config::AgenteKind;

/// One row of the `/model` menu, normalized for UI consumption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Value to pass to the agent's `--model` arg.
    pub id: String,
    /// Human-readable label as shown in the TUI (after the `N.` number).
    pub label: String,
    /// Whether the TUI marked this entry as currently selected.
    pub current: bool,
}

/// A discovered model list persisted to `config.json` so it survives
/// restarts. Keyed by `(kind, command)` — the same shape as the in-memory
/// `AppState.agent_models` cache — so a stored list is only reused when the
/// agent kind and resolved command still match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedModels {
    pub kind: AgenteKind,
    pub command: String,
    pub models: Vec<ModelEntry>,
}

/// Re-render `bytes` (raw PTY output) into a text frame, then extract
/// model rows using the per-kind parser.
pub fn parse_models(bytes: &[u8], kind: AgenteKind) -> Vec<ModelEntry> {
    let frame = render_frame(bytes, 40, 140);
    match kind {
        AgenteKind::ClaudeCode => parse_claude(&frame),
        AgenteKind::Codex => parse_codex(&frame),
    }
}

/// Spawn `binary` (typically `claude` or `codex`) under a PTY, drive it
/// to the `/model` menu, and return the parsed entries.
///
/// `predismiss_enters` is the number of pre-`/model` Enter presses
/// (claude: 1 to dismiss the trust dialog when the cwd is unknown;
/// codex: 1 for its onboarding screen on first run, 0 once trusted).
pub fn discover_models(
    binary: &str,
    kind: AgenteKind,
    warmup_secs: u64,
    tail_secs: u64,
    predismiss_enters: u32,
) -> Result<Vec<ModelEntry>> {
    let bytes = capture_model_menu(binary, warmup_secs, tail_secs, predismiss_enters)?;
    Ok(parse_models(&bytes, kind))
}

/// Drive the PTY exactly like the T-29 probe does. Kept in this module
/// instead of `terminal.rs` because the timing (warmup, dismiss, split
/// `/model` + `\r` send) is /model-specific and we don't want to
/// generalize before we know other discovery flows.
fn capture_model_menu(
    binary: &str,
    warmup_secs: u64,
    tail_secs: u64,
    predismiss_enters: u32,
) -> Result<Vec<u8>> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 40,
        cols: 140,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    // Resolve `binary` the same way the agent-spawn path does, so npm's
    // Windows shims (`<name>.cmd`) are found and wrapped with `cmd.exe /C`
    // instead of handing the extensionless POSIX script to CreateProcessW
    // (which fails with os error 2 / ERROR_BAD_EXE_FORMAT). No-op on Unix.
    let (resolved, prefix_args) = crate::spawn::resolve_command(binary);
    let mut cmd = CommandBuilder::new(&resolved);
    for a in &prefix_args {
        cmd.arg(a);
    }
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| anyhow!("spawn {binary}: {e}"))?;
    drop(pair.slave);

    let master = pair.master;
    let mut reader = master.try_clone_reader()?;
    let writer = Arc::new(Mutex::new(master.take_writer()?));

    let collected = Arc::new(Mutex::new(Vec::<u8>::new()));
    let collected_c = collected.clone();
    let writer_c = writer.clone();
    let _reader_handle = thread::spawn(move || {
        let mut dsr_state: u8 = 0;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    {
                        let mut g = collected_c.lock().unwrap();
                        g.extend_from_slice(&buf[..n]);
                    }
                    // Reply to the ConPTY DSR-CPR query so the agent
                    // unblocks its boot (see spawn::answer_dsr_cpr).
                    crate::spawn::answer_dsr_cpr(&mut dsr_state, &buf[..n], &writer_c);
                }
                Err(_) => break,
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(warmup_secs);
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(200));
    }

    for _ in 0..predismiss_enters {
        if let Ok(mut w) = writer.lock() {
            let _ = w.write_all(b"\r");
            let _ = w.flush();
        }
        thread::sleep(Duration::from_millis(1500));
    }

    if let Ok(mut w) = writer.lock() {
        let _ = w.write_all(b"/model");
        let _ = w.flush();
    }
    thread::sleep(Duration::from_millis(800));
    if let Ok(mut w) = writer.lock() {
        let _ = w.write_all(b"\r");
        let _ = w.flush();
    }

    thread::sleep(Duration::from_secs(tail_secs));

    let bytes = collected.lock().unwrap().clone();

    if let Ok(mut w) = writer.lock() {
        let _ = w.write_all(b"\x1b\x1b\x03");
        let _ = w.flush();
    }
    thread::sleep(Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();

    // Reader thread may block on ConPTY teardown — don't join, the OS
    // reclaims its stack when the process exits.
    Ok(bytes)
}

/// In-memory framebuffer driven by `vte::Parser` + a minimal `Perform`
/// impl. Handles only what the `/model` TUI actually emits: print,
/// CR/LF/BS/TAB, CSI cursor moves, EL/ED erasure. SGR (colors) and most
/// mode toggles are dropped — they don't affect cell content.
struct Frame {
    rows: usize,
    cols: usize,
    grid: Vec<Vec<char>>,
    row: usize,
    col: usize,
}

impl Frame {
    fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            grid: vec![vec![' '; cols]; rows],
            row: 0,
            col: 0,
        }
    }

    fn to_lines(&self) -> Vec<String> {
        self.grid
            .iter()
            .map(|row| {
                let s: String = row.iter().collect();
                s.trim_end().to_string()
            })
            .collect()
    }

    fn put_char(&mut self, c: char) {
        if self.row >= self.rows {
            return;
        }
        if self.col >= self.cols {
            // Don't auto-wrap — the menus we care about render within
            // the configured 140 cols. Wrapping would only happen for
            // pathologically narrow probes.
            self.col = self.cols.saturating_sub(1);
        }
        self.grid[self.row][self.col] = c;
        self.col = (self.col + 1).min(self.cols);
    }

    fn move_to(&mut self, row: usize, col: usize) {
        self.row = row.min(self.rows.saturating_sub(1));
        self.col = col.min(self.cols.saturating_sub(1));
    }
}

impl vte::Perform for Frame {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.row = (self.row + 1).min(self.rows.saturating_sub(1));
            }
            b'\r' => {
                self.col = 0;
            }
            b'\t' => {
                self.col = ((self.col / 8) + 1) * 8;
                if self.col >= self.cols {
                    self.col = self.cols.saturating_sub(1);
                }
            }
            0x08 => {
                self.col = self.col.saturating_sub(1);
            }
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        // Collapse the param iterator into a Vec<u16> using the first
        // subparam of each group. None of the sequences we care about
        // use sub-params, so this is lossless for /model menus.
        let p: Vec<u16> = params
            .iter()
            .map(|g| g.first().copied().unwrap_or(0))
            .collect();
        match action {
            'H' | 'f' => {
                let r = p.first().copied().unwrap_or(1).max(1) as usize - 1;
                let c = p.get(1).copied().unwrap_or(1).max(1) as usize - 1;
                self.move_to(r, c);
            }
            'A' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.row = self.row.saturating_sub(n);
            }
            'B' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.row = (self.row + n).min(self.rows.saturating_sub(1));
            }
            'C' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.col = (self.col + n).min(self.cols.saturating_sub(1));
            }
            'D' => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                self.col = self.col.saturating_sub(n);
            }
            'G' => {
                let c = p.first().copied().unwrap_or(1).max(1) as usize - 1;
                self.col = c.min(self.cols.saturating_sub(1));
            }
            'K' => {
                // EL — erase in line. 0=cursor→EOL (default), 1=BOL→cursor, 2=line.
                let mode = p.first().copied().unwrap_or(0);
                if self.row < self.rows {
                    let row = &mut self.grid[self.row];
                    match mode {
                        0 => {
                            for cell in row.iter_mut().skip(self.col) {
                                *cell = ' ';
                            }
                        }
                        1 => {
                            for cell in row.iter_mut().take(self.col + 1) {
                                *cell = ' ';
                            }
                        }
                        2 => {
                            for cell in row.iter_mut() {
                                *cell = ' ';
                            }
                        }
                        _ => {}
                    }
                }
            }
            'J' => {
                // ED — erase in display. 0=cursor→end, 1=start→cursor, 2=screen.
                let mode = p.first().copied().unwrap_or(0);
                match mode {
                    2 | 3 => {
                        for row in self.grid.iter_mut() {
                            for cell in row.iter_mut() {
                                *cell = ' ';
                            }
                        }
                    }
                    0 => {
                        if self.row < self.rows {
                            for cell in self.grid[self.row].iter_mut().skip(self.col) {
                                *cell = ' ';
                            }
                            for row in self.grid.iter_mut().skip(self.row + 1) {
                                for cell in row.iter_mut() {
                                    *cell = ' ';
                                }
                            }
                        }
                    }
                    1 => {
                        for row in self.grid.iter_mut().take(self.row) {
                            for cell in row.iter_mut() {
                                *cell = ' ';
                            }
                        }
                        if self.row < self.rows {
                            for cell in self.grid[self.row].iter_mut().take(self.col + 1) {
                                *cell = ' ';
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn render_frame(bytes: &[u8], rows: usize, cols: usize) -> Vec<String> {
    let mut frame = Frame::new(rows, cols);
    let mut parser = vte::Parser::new();
    parser.advance(&mut frame, bytes);
    frame.to_lines()
}

// ---- Per-agent parsers --------------------------------------------------

/// Strip a leading list marker (`›`, `>`, `●`, whitespace) so a row that
/// starts with the "current item" indicator still matches the number regex.
fn lstrip_marker(s: &str) -> &str {
    s.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, '>' | '›' | '●' | '◦' | '*' | '·' | '-')
    })
}

/// Match `N. <rest>` at the start of a line, returning `(N, rest)`.
fn match_numbered_row(line: &str) -> Option<(u32, &str)> {
    let s = lstrip_marker(line);
    let (num_str, rest) = s.split_once('.')?;
    let n: u32 = num_str.trim().parse().ok()?;
    Some((n, rest.trim_start()))
}

fn parse_claude(frame: &[String]) -> Vec<ModelEntry> {
    // Find the "Select model" header so we don't pick up stray digits
    // from the welcome panel.
    let start = frame
        .iter()
        .position(|l| l.contains("Select model"))
        .unwrap_or(0);
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in frame.iter().skip(start) {
        let Some((_, rest)) = match_numbered_row(line) else {
            continue;
        };
        // First whitespace-separated token is the display name.
        let mut name = rest.split_whitespace().next().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        // Trim any trailing punctuation the TUI appended (rare, defensive).
        while name
            .chars()
            .last()
            .map(|c| !c.is_alphanumeric())
            .unwrap_or(false)
        {
            name.pop();
        }
        if name.is_empty() || !seen.insert(name.clone()) {
            continue;
        }
        // Claude marks the current selection with `√`. The same row also
        // contains the model identifier (e.g. "Opus 4.8") which we keep
        // as the label.
        let current = line.contains('√');
        let id = name.to_lowercase();
        entries.push(ModelEntry {
            id,
            label: rest.trim().to_string(),
            current,
        });
        if entries.len() >= 8 {
            break;
        }
    }
    entries
}

fn parse_codex(frame: &[String]) -> Vec<ModelEntry> {
    let start = frame
        .iter()
        .position(|l| l.contains("Select Model") || l.contains("/model"))
        .unwrap_or(0);
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in frame.iter().skip(start) {
        let Some((_, rest)) = match_numbered_row(line) else {
            continue;
        };
        // Codex prints the model id as the first token (e.g. "gpt-5.5").
        let id = rest.split_whitespace().next().unwrap_or("").to_string();
        if id.is_empty() || !id.starts_with("gpt-") {
            continue;
        }
        if !seen.insert(id.clone()) {
            continue;
        }
        let current = rest.contains("(current)");
        entries.push(ModelEntry {
            id: id.clone(),
            label: rest.trim().to_string(),
            current,
        });
        if entries.len() >= 12 {
            break;
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_FIXTURE: &[u8] = include_bytes!("../testdata/models_claude.bin");
    const CODEX_FIXTURE: &[u8] = include_bytes!("../testdata/models_codex.bin");

    #[test]
    fn parse_claude_fixture_lists_three_models_with_default_current() {
        let entries = parse_models(CLAUDE_FIXTURE, AgenteKind::ClaudeCode);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["default", "sonnet", "haiku"]);
        let current_count = entries.iter().filter(|e| e.current).count();
        assert_eq!(
            current_count, 1,
            "exactly one row should be marked current (√), got {entries:#?}",
        );
        let default = entries.iter().find(|e| e.id == "default").unwrap();
        assert!(
            default.current,
            "the '√'-marked row should be 'default', got {default:?}",
        );
        assert!(
            default.label.to_lowercase().contains("opus"),
            "default label should mention opus, got {:?}",
            default.label,
        );
    }

    #[test]
    fn parse_codex_fixture_lists_six_models_with_gpt55_current() {
        let entries = parse_models(CODEX_FIXTURE, AgenteKind::Codex);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "gpt-5.5",
                "gpt-5.4",
                "gpt-5.4-mini",
                "gpt-5.3-codex",
                "gpt-5.3-codex-spark",
                "gpt-5.2",
            ],
        );
        let current = entries.iter().find(|e| e.current).expect("a current row");
        assert_eq!(current.id, "gpt-5.5");
    }

    #[test]
    fn frame_renders_overstrike_to_final_glyph() {
        // ESC[H = home; print "AB"; ESC[H = home; print "Cz".
        // Final frame row 0 should read "Cz".
        let bytes = b"\x1b[HAB\x1b[HCz";
        let frame = render_frame(bytes, 4, 10);
        assert_eq!(frame[0].trim_end(), "Cz");
    }

    #[test]
    fn match_numbered_row_strips_list_marker() {
        assert_eq!(
            match_numbered_row("› 1. gpt-5.5 (current)"),
            Some((1, "gpt-5.5 (current)")),
        );
        assert_eq!(match_numbered_row("  2. Sonnet"), Some((2, "Sonnet")),);
        assert_eq!(match_numbered_row("no number here"), None);
    }
}
