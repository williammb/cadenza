//! `cadenza skill` — install/remove the Cadenza usage instructions into
//! AI agents (Claude Code, Codex) at project or global scope.
//!
//! The actual filesystem logic lives in the `skills-core` crate so the
//! Tauri app and the CLI share one implementation. This file is just
//! the clap layer + interactive TTY prompt + stdout/JSON reporting.

use anyhow::Result;
use cadenza_i18n::locale::{self, LocaleSources};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::json;
use skills_core::{Action, Agent, Outcome, Scope, StatusRow};
use std::io::{self, IsTerminal, Write};

#[derive(Debug, Args)]
pub struct SkillCmd {
    #[command(subcommand)]
    pub action: SkillAction,
}

#[derive(Debug, Subcommand)]
pub enum SkillAction {
    /// Install the Cadenza skill into the target agent(s).
    Install(InstallOpts),
    /// Remove the Cadenza skill from the target agent(s).
    Remove(RemoveOpts),
    /// Show what is installed where.
    Status,
}

#[derive(Debug, Args)]
pub struct InstallOpts {
    /// Agents to target. Repeat or comma-separate. Empty = prompt.
    #[arg(long, value_enum, num_args = 0.., value_delimiter = ',')]
    pub agent: Vec<CliAgent>,
    /// Installation scope.
    #[arg(long, value_enum, default_value_t = CliScope::Project)]
    pub scope: CliScope,
    /// Override locale (auto = use resolution chain).
    #[arg(long, default_value = "auto")]
    pub locale: String,
    /// Overwrite without prompting.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct RemoveOpts {
    #[arg(long, value_enum, num_args = 0.., value_delimiter = ',')]
    pub agent: Vec<CliAgent>,
    #[arg(long, value_enum, default_value_t = CliScope::Project)]
    pub scope: CliScope,
}

/// Clap wrapper for `skills_core::Agent` — adds `ValueEnum`. Kept
/// separate so the shared crate doesn't need a clap dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliAgent {
    Claude,
    Codex,
}

impl From<CliAgent> for Agent {
    fn from(a: CliAgent) -> Self {
        match a {
            CliAgent::Claude => Agent::Claude,
            CliAgent::Codex => Agent::Codex,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliScope {
    Project,
    Global,
}

impl From<CliScope> for Scope {
    fn from(s: CliScope) -> Self {
        match s {
            CliScope::Project => Scope::Project,
            CliScope::Global => Scope::Global,
        }
    }
}

pub fn run(cmd: SkillCmd, lang_flag: Option<&str>, json: bool) -> Result<()> {
    match cmd.action {
        SkillAction::Install(opts) => {
            let agents = resolve_agents(opts.agent, "install")?;
            let locale = resolve_locale(&opts.locale, lang_flag);
            let report =
                skills_core::install(&agents, opts.scope.into(), &locale, opts.force, None)?;
            emit_report(&report, json);
            Ok(())
        }
        SkillAction::Remove(opts) => {
            let agents = resolve_agents(opts.agent, "remove")?;
            let report = skills_core::remove(&agents, opts.scope.into(), None)?;
            emit_report(&report, json);
            Ok(())
        }
        SkillAction::Status => {
            let rows = skills_core::status(None);
            emit_status(&rows, json);
            Ok(())
        }
    }
}

// --- agent selection -------------------------------------------------------

fn resolve_agents(given: Vec<CliAgent>, action: &str) -> Result<Vec<Agent>> {
    if !given.is_empty() {
        let mut out: Vec<Agent> = given.into_iter().map(Agent::from).collect();
        out.sort_by_key(|a| a.as_str());
        out.dedup();
        return Ok(out);
    }
    prompt_agents(action)
}

fn prompt_agents(action: &str) -> Result<Vec<Agent>> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        anyhow::bail!(
            "no --agent given and stdin is not a TTY; pass --agent claude, --agent codex, or --agent claude,codex"
        );
    }
    eprintln!("Which agent(s) to {action}?");
    eprintln!("  [1] claude");
    eprintln!("  [2] codex");
    eprintln!("  [3] both");
    eprint!("> ");
    io::stderr().flush().ok();
    let mut line = String::new();
    stdin.read_line(&mut line)?;
    let trimmed = line.trim();
    match trimmed {
        "1" | "claude" => Ok(vec![Agent::Claude]),
        "2" | "codex" => Ok(vec![Agent::Codex]),
        "3" | "both" | "all" | "" => Ok(vec![Agent::Claude, Agent::Codex]),
        other => anyhow::bail!("invalid choice: {other:?}"),
    }
}

fn resolve_locale(flag: &str, lang_flag: Option<&str>) -> String {
    if flag != "auto" {
        return locale::normalize(flag);
    }
    let env = locale::read_env();
    locale::resolve(LocaleSources {
        flag: lang_flag,
        env: env.as_deref(),
        config: None,
    })
}

// --- reporting -------------------------------------------------------------

fn emit_report(report: &[Outcome], json: bool) {
    if json {
        let arr: Vec<serde_json::Value> = report
            .iter()
            .map(|o| {
                json!({
                    "agent": o.agent.as_str(),
                    "scope": o.scope.as_str(),
                    "action": action_str(o.action),
                    "path": o.path.display().to_string(),
                    "locale": o.locale,
                    "detail": o.detail,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string(&arr).unwrap_or_else(|_| "[]".into())
        );
    } else {
        for o in report {
            let mut line = format!(
                "{} {} {} {}",
                action_str(o.action),
                o.agent.as_str(),
                o.scope.as_str(),
                o.path.display(),
            );
            if let Some(l) = &o.locale {
                line.push_str(&format!(" [{l}]"));
            }
            if let Some(d) = &o.detail {
                line.push_str(&format!(" — {d}"));
            }
            println!("{line}");
        }
    }
}

fn emit_status(rows: &[StatusRow], json: bool) {
    if json {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "agent": r.agent.as_str(),
                    "scope": r.scope.as_str(),
                    "path": r.path.display().to_string(),
                    "installed": r.installed,
                    "locale": r.locale,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string(&arr).unwrap_or_else(|_| "[]".into())
        );
    } else {
        println!("{:<7} {:<8} {:<10} path", "agent", "scope", "status");
        for r in rows {
            let status = if r.installed {
                match r.locale.as_deref() {
                    Some(l) => format!("yes [{l}]"),
                    None => "yes".to_string(),
                }
            } else {
                "no".to_string()
            };
            println!(
                "{:<7} {:<8} {:<10} {}",
                r.agent.as_str(),
                r.scope.as_str(),
                status,
                r.path.display()
            );
        }
    }
}

fn action_str(a: Action) -> &'static str {
    match a {
        Action::Installed => "installed",
        Action::Removed => "removed",
        Action::Skipped => "skipped",
    }
}
