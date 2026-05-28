//! Cadenza skill installer — install/remove/status of the agent skill
//! snippet for Claude Code and Codex, in project or global scope.
//!
//! Targets:
//!   Claude global  → ~/.claude/skills/cadenza/SKILL.md
//!   Claude project → <cwd>/.claude/skills/cadenza/SKILL.md
//!   Codex global   → ~/.codex/AGENTS.md       (managed section)
//!   Codex project  → <cwd>/AGENTS.md          (managed section)
//!
//! For Codex, the skill is wrapped in HTML comment markers so install /
//! remove can edit a shared file without clobbering unrelated content:
//!
//! ```text
//! <!-- cadenza:start v=1 locale=pt-BR -->
//! ...skill body...
//! <!-- cadenza:end -->
//! ```
//!
//! For Claude, a YAML frontmatter is prepended so the file is a valid
//! Claude Code Skill that the agent can discover by name.
//!
//! The skill body is embedded at compile time from `skills/cadenza.*.md`
//! so the binary is self-contained — no need for the skill files to be
//! present on the user's machine.
//!
//! This crate holds only the *pure* logic. The cadenza-cli wraps it
//! with clap, TTY prompts and stdout printing; the Tauri backend wraps
//! it with `#[tauri::command]` handlers.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const SKILL_PT_BR: &str = include_str!("../../skills/cadenza.pt-BR.md");
const SKILL_EN: &str = include_str!("../../skills/cadenza.en.md");

const CODEX_MARKER_START_PREFIX: &str = "<!-- cadenza:start";
const CODEX_MARKER_END: &str = "<!-- cadenza:end -->";

const CLAUDE_SKILL_NAME: &str = "cadenza";
const CLAUDE_SKILL_DESCRIPTION_PT: &str =
    "Como gerenciar tarefas via o CLI `cadenza` (current, log, propose, done).";
const CLAUDE_SKILL_DESCRIPTION_EN: &str =
    "How to manage tasks via the `cadenza` CLI (current, log, propose, done).";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Project,
    Global,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Global => "global",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Installed,
    Removed,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub agent: Agent,
    pub scope: Scope,
    #[serde(serialize_with = "serialize_path")]
    pub path: PathBuf,
    pub action: Action,
    pub detail: Option<String>,
    pub locale: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusRow {
    pub agent: Agent,
    pub scope: Scope,
    #[serde(serialize_with = "serialize_path")]
    pub path: PathBuf,
    pub installed: bool,
    pub locale: Option<String>,
}

fn serialize_path<S: serde::Serializer>(p: &Path, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&p.display().to_string())
}

// --- install ---------------------------------------------------------------

pub fn install(
    agents: &[Agent],
    scope: Scope,
    locale: &str,
    force: bool,
    project_root: Option<&Path>,
) -> Result<Vec<Outcome>> {
    let body = skill_body(locale);
    let mut report = Vec::with_capacity(agents.len());
    for agent in dedup_sorted(agents) {
        let outcome = match agent {
            Agent::Claude => install_claude(scope, locale, body, force, project_root)?,
            Agent::Codex => install_codex(scope, locale, body, force, project_root)?,
        };
        report.push(outcome);
    }
    Ok(report)
}

fn install_claude(
    scope: Scope,
    locale: &str,
    body: &str,
    force: bool,
    project_root: Option<&Path>,
) -> Result<Outcome> {
    let path = claude_path(scope, project_root)?;
    if path.exists() && !force {
        return Ok(Outcome::skipped(
            Agent::Claude,
            scope,
            &path,
            "already exists (use force)",
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let description = match locale {
        "pt-BR" => CLAUDE_SKILL_DESCRIPTION_PT,
        _ => CLAUDE_SKILL_DESCRIPTION_EN,
    };
    let content = format!(
        "---\nname: {name}\ndescription: {desc}\n---\n\n{body}",
        name = CLAUDE_SKILL_NAME,
        desc = description,
        body = body,
    );
    write_atomic(&path, content.as_bytes())?;
    Ok(Outcome::installed(Agent::Claude, scope, &path, locale))
}

fn install_codex(
    scope: Scope,
    locale: &str,
    body: &str,
    force: bool,
    project_root: Option<&Path>,
) -> Result<Outcome> {
    let path = codex_agents_path(scope, project_root)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let block = format!(
        "{start} v=1 locale={locale} -->\n{body}\n{end}\n",
        start = CODEX_MARKER_START_PREFIX,
        locale = locale,
        body = body,
        end = CODEX_MARKER_END,
    );

    let new_content = if let Some((before, after)) = split_codex_block(&existing) {
        if !force {
            return Ok(Outcome::skipped(
                Agent::Codex,
                scope,
                &path,
                "managed block already present (use force to update)",
            ));
        }
        format!("{before}{block}{after}")
    } else if existing.is_empty() {
        block
    } else {
        let sep = if existing.ends_with("\n\n") {
            ""
        } else if existing.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        format!("{existing}{sep}{block}")
    };

    write_atomic(&path, new_content.as_bytes())?;
    Ok(Outcome::installed(Agent::Codex, scope, &path, locale))
}

// --- remove ----------------------------------------------------------------

pub fn remove(agents: &[Agent], scope: Scope, project_root: Option<&Path>) -> Result<Vec<Outcome>> {
    let mut report = Vec::with_capacity(agents.len());
    for agent in dedup_sorted(agents) {
        let outcome = match agent {
            Agent::Claude => remove_claude(scope, project_root)?,
            Agent::Codex => remove_codex(scope, project_root)?,
        };
        report.push(outcome);
    }
    Ok(report)
}

fn remove_claude(scope: Scope, project_root: Option<&Path>) -> Result<Outcome> {
    let path = claude_path(scope, project_root)?;
    if !path.exists() {
        return Ok(Outcome::skipped(
            Agent::Claude,
            scope,
            &path,
            "not installed",
        ));
    }
    fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    // Best-effort cleanup of the empty `cadenza/` skill folder.
    if let Some(parent) = path.parent() {
        let _ = fs::remove_dir(parent);
    }
    Ok(Outcome::removed(Agent::Claude, scope, &path))
}

fn remove_codex(scope: Scope, project_root: Option<&Path>) -> Result<Outcome> {
    let path = codex_agents_path(scope, project_root)?;
    if !path.exists() {
        return Ok(Outcome::skipped(
            Agent::Codex,
            scope,
            &path,
            "AGENTS.md not present",
        ));
    }
    let existing = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let Some((before, after)) = split_codex_block(&existing) else {
        return Ok(Outcome::skipped(
            Agent::Codex,
            scope,
            &path,
            "no managed block",
        ));
    };
    // Collapse whitespace at the boundary so we don't leave a blank gap.
    let joined = format!("{}{}", before.trim_end_matches('\n'), after);
    let cleaned = if joined.is_empty() {
        // The file had ONLY our block — delete it so we leave the FS tidy.
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        return Ok(Outcome::removed(Agent::Codex, scope, &path));
    } else if joined.ends_with('\n') {
        joined
    } else {
        format!("{joined}\n")
    };
    write_atomic(&path, cleaned.as_bytes())?;
    Ok(Outcome::removed(Agent::Codex, scope, &path))
}

// --- status ----------------------------------------------------------------

pub fn status(project_root: Option<&Path>) -> Vec<StatusRow> {
    let mut rows = Vec::with_capacity(4);
    for scope in [Scope::Project, Scope::Global] {
        rows.push(probe_claude(scope, project_root));
        rows.push(probe_codex(scope, project_root));
    }
    rows
}

fn probe_claude(scope: Scope, project_root: Option<&Path>) -> StatusRow {
    let path = claude_path(scope, project_root).unwrap_or_else(|_| PathBuf::from("<no home>"));
    let installed = path.exists();
    let locale = if installed {
        fs::read_to_string(&path).ok().and_then(parse_claude_locale)
    } else {
        None
    };
    StatusRow {
        agent: Agent::Claude,
        scope,
        path,
        installed,
        locale,
    }
}

fn probe_codex(scope: Scope, project_root: Option<&Path>) -> StatusRow {
    let path =
        codex_agents_path(scope, project_root).unwrap_or_else(|_| PathBuf::from("<no home>"));
    let (installed, locale) = if path.exists() {
        let content = fs::read_to_string(&path).unwrap_or_default();
        let loc = parse_codex_locale(&content);
        (loc.is_some(), loc)
    } else {
        (false, None)
    };
    StatusRow {
        agent: Agent::Codex,
        scope,
        path,
        installed,
        locale,
    }
}

fn parse_claude_locale(content: String) -> Option<String> {
    // Skill body is verbatim from skills/cadenza.{locale}.md after the
    // YAML frontmatter. Locale is not encoded in frontmatter; detect by
    // body content — the EN file's first heading is "How to use Cadenza".
    if content.contains("# Cadenza — Como usar") {
        Some("pt-BR".into())
    } else if content.contains("# Cadenza — How to use")
        || content.contains("# Cadenza - How to use")
    {
        Some("en".into())
    } else {
        None
    }
}

fn parse_codex_locale(content: &str) -> Option<String> {
    let start_line = content
        .lines()
        .find(|l| l.starts_with(CODEX_MARKER_START_PREFIX))?;
    // Marker format: <!-- cadenza:start v=1 locale=pt-BR -->
    start_line
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("locale="))
        .map(|s| s.trim_end_matches("-->").trim().to_string())
}

// --- shared helpers --------------------------------------------------------

fn dedup_sorted(agents: &[Agent]) -> Vec<Agent> {
    let mut out: Vec<Agent> = agents.to_vec();
    out.sort_by_key(|a| a.as_str());
    out.dedup();
    out
}

fn skill_body(locale: &str) -> &'static str {
    match locale {
        "pt-BR" => SKILL_PT_BR,
        _ => SKILL_EN,
    }
}

fn project_root_or_cwd(project_root: Option<&Path>) -> Result<PathBuf> {
    match project_root {
        Some(p) => Ok(p.to_path_buf()),
        None => std::env::current_dir().context("read current directory"),
    }
}

fn claude_path(scope: Scope, project_root: Option<&Path>) -> Result<PathBuf> {
    let base = match scope {
        Scope::Project => project_root_or_cwd(project_root)?,
        Scope::Global => home_dir()?,
    };
    Ok(base
        .join(".claude")
        .join("skills")
        .join(CLAUDE_SKILL_NAME)
        .join("SKILL.md"))
}

fn codex_agents_path(scope: Scope, project_root: Option<&Path>) -> Result<PathBuf> {
    Ok(match scope {
        Scope::Project => project_root_or_cwd(project_root)?.join("AGENTS.md"),
        Scope::Global => home_dir()?.join(".codex").join("AGENTS.md"),
    })
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("could not determine the user's home directory")
}

fn split_codex_block(content: &str) -> Option<(String, String)> {
    let start_idx = content.find(CODEX_MARKER_START_PREFIX)?;
    let after_start = &content[start_idx..];
    let end_rel = after_start.find(CODEX_MARKER_END)?;
    let end_idx = start_idx + end_rel + CODEX_MARKER_END.len();
    // Swallow a single trailing newline so successive install/remove
    // doesn't accumulate blank lines.
    let mut tail_start = end_idx;
    if content.as_bytes().get(tail_start) == Some(&b'\n') {
        tail_start += 1;
    }
    Some((
        content[..start_idx].to_string(),
        content[tail_start..].to_string(),
    ))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("cadenza-tmp");
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

// --- constructors ----------------------------------------------------------

impl Outcome {
    fn installed(agent: Agent, scope: Scope, path: &Path, locale: &str) -> Self {
        Self {
            agent,
            scope,
            path: path.to_path_buf(),
            action: Action::Installed,
            detail: None,
            locale: Some(locale.to_string()),
        }
    }
    fn removed(agent: Agent, scope: Scope, path: &Path) -> Self {
        Self {
            agent,
            scope,
            path: path.to_path_buf(),
            action: Action::Removed,
            detail: None,
            locale: None,
        }
    }
    fn skipped(agent: Agent, scope: Scope, path: &Path, reason: &str) -> Self {
        Self {
            agent,
            scope,
            path: path.to_path_buf(),
            action: Action::Skipped,
            detail: Some(reason.to_string()),
            locale: None,
        }
    }
}

// --- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_finds_existing_block() {
        let content =
            "intro\n\n<!-- cadenza:start v=1 locale=en -->\nbody\n<!-- cadenza:end -->\ntail\n";
        let (before, after) = split_codex_block(content).expect("block present");
        assert_eq!(before, "intro\n\n");
        assert_eq!(after, "tail\n");
    }

    #[test]
    fn split_returns_none_when_absent() {
        assert!(split_codex_block("nothing here").is_none());
    }

    #[test]
    fn parse_codex_locale_reads_marker() {
        let content = "<!-- cadenza:start v=1 locale=pt-BR -->\nx\n<!-- cadenza:end -->";
        assert_eq!(parse_codex_locale(content).as_deref(), Some("pt-BR"));
    }

    #[test]
    fn parse_claude_locale_detects_pt() {
        let body = "---\nname: cadenza\n---\n\n# Cadenza — Como usar\nfoo";
        assert_eq!(parse_claude_locale(body.into()).as_deref(), Some("pt-BR"));
    }
}
