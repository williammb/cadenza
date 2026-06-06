use anyhow::{Context, Result};
use cadenza_proto::ops;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use uuid::Uuid;

mod aliases;
mod client;
mod skill;

use client::{AppNotRunning, Client, WireError};
use skill::SkillCmd;

/// Cadenza CLI — drive tasks from an AI agent.
#[derive(Parser, Debug)]
#[command(name = "cadenza-cli", version, about, long_about = None)]
struct Cli {
    /// Locale override (overrides CADENZA_LANG and config.json).
    #[arg(long, global = true, value_name = "LOCALE")]
    lang: Option<String>,

    /// Emit JSON output (PT canonical values, stable for parsing).
    #[arg(long, global = true)]
    json: bool,

    /// Verbose tracing (CADENZA_LOG=debug equivalent).
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List tasks, optionally filtered by state.
    List {
        /// a_fazer|fazendo|aguardando_revisao|feito (or EN aliases: todo|doing|review|done)
        #[arg(long, value_name = "ESTADO")]
        estado: Option<String>,
    },
    /// Show the current task (the single task in `fazendo`).
    ///
    /// Ambiguous when several tasks are in `fazendo` (one per running
    /// agent): it returns only the topmost card. Prefer `get
    /// "$TASKAI_TASK_ID"` to fetch the task this agent was started for.
    Current,
    /// Show a single task by id — returns only that task, or exits 30
    /// (`task_not_found`) if it doesn't exist.
    Get { task_id: String },
    /// List the configured projects (id, name, path) so you can discover
    /// the `project_id` to pass to `new-task` / `create-ideia`.
    Projects,
    /// Append a progress log line to a task.
    Log { task_id: String, text: String },
    /// Propose a derived task and block until the human decides.
    Propose {
        #[arg(long)]
        parent: Option<String>,
        #[arg(long)]
        title: String,
        #[arg(long)]
        repro: String,
        #[arg(long)]
        file: String,
        #[arg(long = "what-failed")]
        what_failed: String,
        #[arg(long)]
        action: String,
        #[arg(long = "timeout-min", default_value_t = 5)]
        timeout_min: u32,
        /// Idempotency key for this proposal (uuid v4 recommended).
        /// If absent, falls back to $CADENZA_IDEMPOTENCY_KEY and then
        /// to a freshly minted v4 — but a retried `propose` only hits
        /// the server-side dedup path when the SAME key is passed, so
        /// agents that may crash mid-flight should generate one
        /// up-front and pass it explicitly. The resolved key is
        /// echoed to stderr on success.
        #[arg(long = "idempotency-key")]
        idempotency_key: Option<String>,
    },
    /// Materialize an analysis run into proposals (server-stamped Jira
    /// identity). The capability secret is read from $CADENZA_RUN_SECRET
    /// (or STDIN with --secret-stdin) — never from argv. Subtasks are read
    /// from a JSON file (`[{"title","body"}, ...]`), or "-" for STDIN.
    JiraMaterialize {
        #[arg(long)]
        analysis_run_id: String,
        /// Read the capability secret from STDIN (first line) instead of
        /// $CADENZA_RUN_SECRET.
        #[arg(long)]
        secret_stdin: bool,
        /// Path to a JSON file with `[{"title","body"}, ...]`; or "-" for
        /// STDIN. Cannot be "-" together with --secret-stdin.
        #[arg(long)]
        subtasks_file: String,
    },
    /// Verify the configured Jira credentials by calling `/myself`. Prints
    /// the resolved account id + display name. Exit 2 if Jira is not
    /// configured, 11 on auth failure.
    JiraTestConnection {
        #[arg(long)]
        json: bool,
    },
    /// Fetch one Jira issue by key (e.g. PROJ-123). Prints the summary +
    /// the description converted to Markdown. Exit 30 if not found.
    JiraFetchIssue {
        key: String,
        #[arg(long)]
        json: bool,
    },
    /// List the caller's open (not-Done) assigned Jira issues. The result
    /// may be marked partial if the page cap was hit.
    JiraListAssigned {
        #[arg(long)]
        json: bool,
    },
    /// Build + persist the aggregate (issue-owned) review: the committed
    /// branch diff of the shared per-issue worktree. STATE-NEUTRAL — does not
    /// move any subtask. Exit 1 if the worktree is not yet Ready, 30 if the
    /// issue record is absent. Prints the package JSON.
    JiraReview {
        #[arg(long)]
        site: String,
        #[arg(long)]
        issue: String,
        #[arg(long)]
        json: bool,
    },
    /// Import a Jira issue into a project and spawn the analyst agent. The
    /// analyst decomposes the issue into subtasks and submits them via
    /// `jira-materialize`. Re-importing an issue that already has active work
    /// is idempotent (no re-fetch/spawn).
    JiraImport {
        /// Jira issue key, e.g. PROJ-123.
        issue_ref: String,
        /// Target project id.
        #[arg(long)]
        project: String,
        /// Analyst agent kind (claude-code|codex|copilot|antigravity|opencode).
        #[arg(long)]
        analyst: String,
        #[arg(long)]
        json: bool,
    },
    /// Discard an imported Jira issue: remove its worktree, revoke its run
    /// secret, and delete its record. Refuses a dirty worktree unless
    /// `--force` is given. RETAINS the produced subtasks, branch, and review
    /// packages.
    JiraDiscard {
        #[arg(long)]
        site: String,
        #[arg(long)]
        issue: String,
        /// Override the dirty-worktree refusal (loses uncommitted work).
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Print this project's quality contract — the checks the agent must
    /// run before `done`. Resolution order app-side: `--project` → env
    /// (`TASKAI_PROJECT_ID`/`CADENZA_PROJECT_ID`) → the app's active project.
    /// `--task` is accepted for symmetry/future use; the app may resolve the
    /// project from it. Unknown project ⇒ exit 30; resolved-but-no-profile ⇒
    /// an empty check list (that is `no_validation`, not an error).
    Quality {
        /// Task id (optional; default $TASKAI_TASK_ID). Lets the app resolve
        /// the project from the task when no project is given.
        #[arg(long)]
        task: Option<String>,
        /// Project id (default $TASKAI_PROJECT_ID, fallback $CADENZA_PROJECT_ID).
        #[arg(long)]
        project: Option<String>,
    },
    /// Request completion — the human still has the final word.
    ///
    /// `done <task_id> <summary>` stays backward compatible. Pass
    /// `--evidence <file.json>` to attach validation evidence (parsed and
    /// capped locally; malformed ⇒ exit 2). `--evidence` requires a v3 app:
    /// against an older app it exits 12 (protocol_mismatch) unless
    /// `--legacy-done` is given, which sends the plain positional `done`.
    /// `--idempotency-key` (or $CADENZA_IDEMPOTENCY_KEY) makes the request
    /// resumable; the resolved key is echoed to stderr in all modes.
    Done {
        task_id: String,
        /// Human summary (positional). May also be given via `--summary`.
        #[arg(default_value = "")]
        summary: String,
        /// Validation evidence JSON (PLAN §B.6 schema). Requires a v3 app.
        #[arg(long, value_name = "FILE")]
        evidence: Option<PathBuf>,
        /// Alias for the positional `summary` (takes precedence if both set).
        #[arg(long = "summary", value_name = "TEXT")]
        summary_flag: Option<String>,
        /// Idempotency key for this done attempt (uuid v4 recommended).
        /// Falls back to $CADENZA_IDEMPOTENCY_KEY then a fresh v4. Echoed to
        /// stderr; included in `--json` output. Validated (exit 2 on bad key).
        #[arg(long = "idempotency-key")]
        idempotency_key: Option<String>,
        /// When `--evidence` is set but the app is pre-v3, send the plain
        /// positional `done` (drop evidence) instead of failing with exit 12.
        #[arg(long = "legacy-done")]
        legacy_done: bool,
    },
    /// Save a task's refined plan. Used in plan mode: the agent interviews
    /// the human and persists the agreed plan. By default the plan is
    /// appended as a `## Plano` section, preserving the original
    /// description; pass `--replace` to overwrite the whole body. Omit
    /// `--body` to read the plan from stdin.
    Plan {
        task_id: String,
        /// Plan markdown. If omitted, read from stdin.
        #[arg(long)]
        body: Option<String>,
        /// Replace the whole body instead of appending a `## Plano` section.
        #[arg(long)]
        replace: bool,
    },
    /// Create a new task in `a_fazer`, bound to a project. Used by the
    /// "destrinchar ideia" flow: o agente chama isso N vezes para
    /// transformar uma ideia em tasks concretas. Defaults pegam do
    /// ambiente do PTY do agente (`$TASKAI_PROJECT_ID`,
    /// `$CADENZA_IDEIA_ID`).
    NewTask {
        #[arg(long)]
        titulo: String,
        #[arg(long, default_value = "")]
        body: String,
        /// Project ID (default: $TASKAI_PROJECT_ID, fallback $CADENZA_PROJECT_ID).
        #[arg(long)]
        project: Option<String>,
        /// Marca a ideia de origem como `destrinchada` ao final.
        /// (default: $CADENZA_IDEIA_ID).
        #[arg(long = "from-ideia")]
        from_ideia: Option<String>,
    },
    /// List pending ideias in the Inbox.
    ListIdeias,
    /// Read a single ideia's full body.
    ReadIdeia { ideia_id: String },
    /// Create a new ideia (Inbox entry).
    CreateIdeia {
        #[arg(long)]
        titulo: String,
        #[arg(long, default_value = "")]
        body: String,
        /// Project ID (default: $TASKAI_PROJECT_ID, fallback $CADENZA_PROJECT_ID).
        #[arg(long)]
        project: Option<String>,
    },
    /// Delete an ideia.
    DeleteIdeia { ideia_id: String },
    /// Project shared memory (read it, suggest learnings, propose reevals).
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Associate a task with a git worktree path and/or branch.
    /// Calling with no options clears the association.
    SetWorktree {
        task_id: String,
        /// Absolute path to the git worktree directory.
        #[arg(long, value_name = "PATH")]
        path: Option<String>,
        /// Git branch name for this task.
        #[arg(long, value_name = "BRANCH")]
        branch: Option<String>,
    },
    /// Print runtime diagnostics.
    Diag,
    /// Install / remove the Cadenza skill in supported agents.
    Skill(SkillCmd),
}

/// `cadenza-cli memory ...` — interagir com a memória compartilhada do
/// projeto. `list` relê a memória oficial; `suggest` propõe um
/// aprendizado (pendente até o usuário promovê-lo no review da task);
/// `revise` propõe uma op de reavaliação (modo memory-reeval). Nenhum
/// dos dois altera a memória oficial — só enfileira uma sugestão.
#[derive(Subcommand, Debug)]
enum MemoryCmd {
    /// List the official project memory items.
    List {
        /// Project ID (default: $TASKAI_PROJECT_ID, fallback $CADENZA_PROJECT_ID).
        #[arg(long)]
        project: Option<String>,
    },
    /// Suggest a reusable learning. Pending until the human promotes it
    /// in the task review — nothing enters memory automatically.
    Suggest {
        /// The learning text (one durable fact / decision / convention).
        texto: String,
        /// Project ID (default: $TASKAI_PROJECT_ID, fallback $CADENZA_PROJECT_ID).
        #[arg(long)]
        project: Option<String>,
        /// Originating task (default: $TASKAI_TASK_ID) so the right task
        /// review surfaces this learning.
        #[arg(long)]
        task: Option<String>,
    },
    /// Propose a reevaluation op (used in memory-reeval mode). Pending
    /// until the human approves it in the Memory tab.
    Revise {
        /// remover | reescrever | mesclar | nova | contradicao
        #[arg(long)]
        op: String,
        /// Project ID (default: $TASKAI_PROJECT_ID, fallback $CADENZA_PROJECT_ID).
        #[arg(long)]
        project: Option<String>,
        /// Target item id(s). Repeatable (`--target M-1 --target M-2`).
        /// Required by remover/reescrever (one) and mesclar/contradicao.
        #[arg(long = "target")]
        targets: Vec<String>,
        /// New / merged / new-item text. Required by reescrever, mesclar, nova.
        #[arg(long)]
        texto: Option<String>,
        /// Explanatory note. Required by contradicao.
        #[arg(long)]
        nota: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    if matches!(cli.cmd, Cmd::Diag) {
        // Diag is local-only, no server required.
        return match run_diag() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::from(1)
            }
        };
    }

    if let Cmd::Skill(_) = cli.cmd {
        // Skill management is local-only: edits files under ~/.claude,
        // ~/.codex, or the current project. No app required.
        let Cli {
            cmd, lang, json, ..
        } = cli;
        let Cmd::Skill(skill_cmd) = cmd else {
            unreachable!()
        };
        return match skill::run(skill_cmd, lang.as_deref(), json) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::from(1)
            }
        };
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to build tokio runtime: {e:#}");
            return ExitCode::from(1);
        }
    };

    let outcome = runtime.block_on(async { run(cli).await });
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Map known error types to specific exit codes.
            if e.downcast_ref::<AppNotRunning>().is_some() {
                eprintln!("error: {e:#}");
                return ExitCode::from(10);
            }
            if let Some(wire) = e.downcast_ref::<WireError>() {
                eprintln!("error: {e:#}");
                return ExitCode::from(wire.exit_code() as u8);
            }
            if let Some(bt) = e.downcast_ref::<TokenError>() {
                eprintln!("error: {bt}");
                return ExitCode::from(11);
            }
            if let Some(ue) = e.downcast_ref::<UsageError>() {
                // Bad usage (malformed evidence / invalid idempotency-key)
                // mirrors clap's usage exit code.
                eprintln!("error: {ue}");
                return ExitCode::from(2);
            }
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn init_tracing(verbose: bool) {
    let level = if verbose { "debug" } else { "warn" };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("CADENZA_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .with_writer(std::io::stderr)
        .try_init();
}

async fn run(cli: Cli) -> Result<()> {
    let token = read_token().map_err(|e| anyhow::Error::new(TokenError(e.to_string())))?;
    let mut client = Client::connect().await?;
    // Capture the negotiated protocol version: `done --evidence` gates on it
    // (PLAN §B.6 — evidence requires v3). Other commands ignore it.
    let negotiated_protocol = client.hello(&token).await?.protocol;

    match cli.cmd {
        Cmd::List { estado } => cmd_list(&mut client, cli.json, estado).await?,
        Cmd::Current => cmd_current(&mut client, cli.json).await?,
        Cmd::Get { task_id } => cmd_get(&mut client, cli.json, task_id).await?,
        Cmd::Projects => cmd_projects(&mut client, cli.json).await?,
        Cmd::Log { task_id, text } => cmd_log(&mut client, cli.json, task_id, text).await?,
        Cmd::Propose {
            parent,
            title,
            repro,
            file,
            what_failed,
            action,
            timeout_min,
            idempotency_key,
        } => {
            cmd_propose(
                &mut client,
                cli.json,
                parent,
                title,
                repro,
                file,
                what_failed,
                action,
                timeout_min,
                idempotency_key,
            )
            .await?
        }
        Cmd::JiraMaterialize {
            analysis_run_id,
            secret_stdin,
            subtasks_file,
        } => {
            cmd_jira_materialize(
                &mut client,
                cli.json,
                analysis_run_id,
                secret_stdin,
                subtasks_file,
            )
            .await?
        }
        Cmd::JiraTestConnection { json } => {
            cmd_jira_test_connection(&mut client, cli.json || json).await?
        }
        Cmd::JiraFetchIssue { key, json } => {
            cmd_jira_fetch_issue(&mut client, cli.json || json, key).await?
        }
        Cmd::JiraListAssigned { json } => {
            cmd_jira_list_assigned(&mut client, cli.json || json).await?
        }
        Cmd::JiraReview { site, issue, json } => {
            cmd_jira_review(&mut client, cli.json || json, site, issue).await?
        }
        Cmd::JiraImport {
            issue_ref,
            project,
            analyst,
            json,
        } => cmd_jira_import(&mut client, cli.json || json, issue_ref, project, analyst).await?,
        Cmd::JiraDiscard {
            site,
            issue,
            force,
            json,
        } => cmd_jira_discard(&mut client, cli.json || json, site, issue, force).await?,
        Cmd::Quality { task, project } => cmd_quality(&mut client, cli.json, task, project).await?,
        Cmd::Done {
            task_id,
            summary,
            evidence,
            summary_flag,
            idempotency_key,
            legacy_done,
        } => {
            cmd_done(
                &mut client,
                cli.json,
                negotiated_protocol,
                task_id,
                summary,
                summary_flag,
                evidence,
                idempotency_key,
                legacy_done,
            )
            .await?
        }
        Cmd::Plan {
            task_id,
            body,
            replace,
        } => cmd_plan(&mut client, cli.json, task_id, body, replace).await?,
        Cmd::NewTask {
            titulo,
            body,
            project,
            from_ideia,
        } => cmd_new_task(&mut client, cli.json, titulo, body, project, from_ideia).await?,
        Cmd::ListIdeias => cmd_list_ideias(&mut client, cli.json).await?,
        Cmd::ReadIdeia { ideia_id } => cmd_read_ideia(&mut client, cli.json, ideia_id).await?,
        Cmd::CreateIdeia {
            titulo,
            body,
            project,
        } => cmd_create_ideia(&mut client, cli.json, titulo, body, project).await?,
        Cmd::DeleteIdeia { ideia_id } => cmd_delete_ideia(&mut client, cli.json, ideia_id).await?,
        Cmd::Memory { cmd } => cmd_memory(&mut client, cli.json, cmd).await?,
        Cmd::SetWorktree {
            task_id,
            path,
            branch,
        } => cmd_set_worktree(&mut client, cli.json, task_id, path, branch).await?,
        Cmd::Diag => unreachable!(),
        Cmd::Skill(_) => unreachable!(),
    }

    // Best-effort bye; don't fail the whole command if it errors.
    let _: Result<ops::bye::Result> = client.request(ops::OP_BYE, ops::bye::Args::default()).await;
    Ok(())
}

async fn cmd_list(client: &mut Client, json: bool, estado: Option<String>) -> Result<()> {
    let canonical = if let Some(e) = estado.as_deref() {
        Some(
            aliases::canonicalize(e)
                .ok_or_else(|| anyhow::anyhow!("invalid --estado '{e}'"))?
                .to_string(),
        )
    } else {
        None
    };
    let tasks: ops::list_tasks::Result = client
        .request(
            ops::OP_LIST_TASKS,
            ops::list_tasks::Args { estado: canonical },
        )
        .await?;
    if json {
        println!("{}", serde_json::to_string(&tasks)?);
    } else if tasks.is_empty() {
        println!("(no tasks)");
    } else {
        for t in &tasks {
            println!("{}\t[{}]\t{}", t.id, t.estado.as_str(), t.titulo);
        }
    }
    Ok(())
}

async fn cmd_current(client: &mut Client, json: bool) -> Result<()> {
    let current: ops::current_task::Result = client
        .request(ops::OP_CURRENT_TASK, ops::current_task::Args::default())
        .await?;
    if json {
        println!("{}", serde_json::to_string(&current)?);
    } else {
        match current {
            None => println!("(no current task)"),
            Some(t) => println!("{}\t[{}]\t{}", t.id, t.estado.as_str(), t.titulo),
        }
    }
    Ok(())
}

async fn cmd_get(client: &mut Client, json: bool, task_id: String) -> Result<()> {
    let task: ops::read_task::Result = client
        .request(ops::OP_READ_TASK, ops::read_task::Args { task_id })
        .await?;
    if json {
        println!("{}", serde_json::to_string(&task)?);
    } else {
        println!("{}\t[{}]\t{}", task.id, task.estado.as_str(), task.titulo);
        if !task.body.trim().is_empty() {
            println!("\n{}", task.body);
        }
    }
    Ok(())
}

async fn cmd_projects(client: &mut Client, json: bool) -> Result<()> {
    let projects: ops::list_projects::Result = client
        .request(ops::OP_LIST_PROJECTS, ops::list_projects::Args::default())
        .await?;
    if json {
        println!("{}", serde_json::to_string(&projects)?);
    } else if projects.is_empty() {
        println!("(no projects)");
    } else {
        for p in &projects {
            println!("{}\t{}\t{}", p.id, p.name, p.path);
        }
    }
    Ok(())
}

async fn cmd_log(client: &mut Client, json: bool, task_id: String, text: String) -> Result<()> {
    let _: ops::append_log::Result = client
        .request(ops::OP_APPEND_LOG, ops::append_log::Args { task_id, text })
        .await?;
    if json {
        println!("{{\"ok\":true}}");
    } else {
        println!("ok");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_propose(
    client: &mut Client,
    json: bool,
    parent: Option<String>,
    title: String,
    repro: String,
    file: String,
    what_failed: String,
    action: String,
    timeout_min: u32,
    idempotency_key: Option<String>,
) -> Result<()> {
    // Resolve key: --idempotency-key → $CADENZA_IDEMPOTENCY_KEY → fresh.
    // Echo the resolved value to stderr so the human / agent can capture
    // it and pass `--idempotency-key <value>` on retry to hit the
    // server-side dedup path (CLAUDE.md "propose must be idempotent
    // and resumable"). Without that, a crashed-and-retried CLI minted
    // a fresh uuid v4 every time and the dedup never matched.
    let idempotency_key = idempotency_key
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("CADENZA_IDEMPOTENCY_KEY").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    eprintln!("idempotency-key: {idempotency_key}");
    let propose_args = cadenza_proto::NewProposta {
        idempotency_key,
        parent,
        title,
        repro,
        file,
        what_failed,
        action,
        jira_site: None,
        jira_issue_id: None,
    };
    let started: ops::propose::Result = client.request(ops::OP_PROPOSE, propose_args).await?;
    eprintln!("propose enviado — id={}", started.proposta_id);

    let timeout_ms = (timeout_min as u64).saturating_mul(60_000).min(30 * 60_000);
    let decision = client
        .await_decision(ops::await_decision::Args {
            proposta_id: started.proposta_id.clone(),
            timeout_ms,
        })
        .await?;

    if json {
        println!("{}", serde_json::to_string(&decision)?);
    } else {
        match decision.decisao {
            cadenza_proto::Decisao::Aceita => {
                println!(
                    "aceita {}",
                    decision.task_id.as_deref().unwrap_or("(sem task_id)")
                );
            }
            cadenza_proto::Decisao::Rejeitada => println!("rejeitada"),
            cadenza_proto::Decisao::Mesclada => {
                println!(
                    "mesclada em {}",
                    decision.task_id.as_deref().unwrap_or("(sem task_id)")
                );
            }
        }
    }

    if matches!(decision.decisao, cadenza_proto::Decisao::Rejeitada) {
        return Err(anyhow::Error::new(WireError(
            cadenza_proto::ErrorBody::new("proposal_rejected", "human rejected the proposal"),
        )));
    }
    Ok(())
}

/// Resolve the capability secret. Pure helper so it is unit-testable:
/// `--secret-stdin` reads `stdin_line`; otherwise the `env` lookup is used.
/// Returns `UsageError` (exit 2) if neither path yields a non-empty secret.
fn resolve_run_secret(
    secret_stdin: bool,
    env_secret: Option<String>,
    stdin_line: Option<String>,
) -> Result<String> {
    let raw = if secret_stdin { stdin_line } else { env_secret };
    let secret = raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    secret.ok_or_else(|| {
        anyhow::Error::new(UsageError(
            "no run secret: set $CADENZA_RUN_SECRET or pass --secret-stdin".to_string(),
        ))
    })
}

/// Parse the subtasks JSON (`[{"title","body"}, ...]`). Pure helper so it is
/// unit-testable. Malformed input ⇒ `UsageError` (exit 2).
fn parse_subtasks(text: &str) -> Result<Vec<ops::jira_materialize::Subtask>> {
    serde_json::from_str(text)
        .map_err(|e| anyhow::Error::new(UsageError(format!("invalid subtasks JSON: {e}"))))
}

async fn cmd_jira_materialize(
    client: &mut Client,
    json: bool,
    analysis_run_id: String,
    secret_stdin: bool,
    subtasks_file: String,
) -> Result<()> {
    use std::io::Read as _;

    let subtasks_from_stdin = subtasks_file == "-";
    if secret_stdin && subtasks_from_stdin {
        return Err(anyhow::Error::new(UsageError(
            "cannot read both --secret-stdin and subtasks_file=- from STDIN".to_string(),
        )));
    }

    // Read STDIN once if either source needs it.
    let stdin_line = if secret_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_line(&mut buf)
            .context("read secret from stdin")?;
        Some(buf)
    } else {
        None
    };

    let run_secret = resolve_run_secret(
        secret_stdin,
        std::env::var("CADENZA_RUN_SECRET").ok(),
        stdin_line,
    )?;

    let subtasks_text = if subtasks_from_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("read subtasks from stdin")?;
        buf
    } else {
        std::fs::read_to_string(&subtasks_file)
            .with_context(|| format!("read subtasks file {subtasks_file}"))?
    };
    let subtasks = parse_subtasks(&subtasks_text)?;

    let args = ops::jira_materialize::Args {
        analysis_run_id,
        run_secret,
        subtasks,
    };
    let result: ops::jira_materialize::Result =
        client.request(ops::OP_JIRA_MATERIALIZE, args).await?;

    if json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        for t in &result.created {
            println!(
                "{}\t{}\t{}",
                t.subtask_index, t.proposta_id, t.idempotency_key
            );
        }
    }
    Ok(())
}

async fn cmd_jira_test_connection(client: &mut Client, json: bool) -> Result<()> {
    let args = ops::jira_test_connection::Args {};
    let result: ops::jira_test_connection::Result =
        client.request(ops::OP_JIRA_TEST_CONNECTION, args).await?;
    if json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        println!("{}\t{}", result.account_id, result.display_name);
    }
    Ok(())
}

async fn cmd_jira_fetch_issue(client: &mut Client, json: bool, key: String) -> Result<()> {
    let args = ops::jira_fetch_issue::Args { key };
    let result: ops::jira_fetch_issue::Result =
        client.request(ops::OP_JIRA_FETCH_ISSUE, args).await?;
    if json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        println!("{}\t{}", result.jira_key, result.summary);
        if !result.description_markdown.is_empty() {
            println!("{}", result.description_markdown);
        }
    }
    Ok(())
}

async fn cmd_jira_list_assigned(client: &mut Client, json: bool) -> Result<()> {
    let args = ops::jira_list_assigned::Args {};
    let result: ops::jira_list_assigned::Result =
        client.request(ops::OP_JIRA_LIST_ASSIGNED, args).await?;
    if json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        for issue in &result.issues {
            println!("{}\t{}", issue.key, issue.summary);
        }
        if result.partial {
            eprintln!("warning: result is partial (page cap reached)");
        }
    }
    Ok(())
}

/// Build + persist the aggregate (issue-owned) review and print it. The
/// returned package is a JSON passthrough (its concrete type lives in the app
/// crate), so output is always JSON; `--json` just selects compact vs pretty.
async fn cmd_jira_review(
    client: &mut Client,
    json: bool,
    site: String,
    issue: String,
) -> Result<()> {
    let args = ops::jira_review::Args {
        jira_site: site,
        jira_issue_id: issue,
    };
    let result: ops::jira_review::Result = client.request(ops::OP_JIRA_REVIEW, args).await?;
    if json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(())
}

/// Import a Jira issue + spawn the analyst. Prints the outcome
/// (`imported`/`existing_active`) and the key identity fields; never prints any
/// secret (the wire `Result` carries none — it reaches the analyst via ENV).
async fn cmd_jira_import(
    client: &mut Client,
    json: bool,
    issue_ref: String,
    project: String,
    analyst: String,
) -> Result<()> {
    let args = ops::jira_import::Args {
        issue_ref,
        project_id: project,
        analyst_kind: analyst,
    };
    let result: ops::jira_import::Result = client.request(ops::OP_JIRA_IMPORT, args).await?;
    if json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        match &result {
            ops::jira_import::Result::Imported {
                jira_key,
                analysis_run_id,
                session_id,
                ..
            } => {
                println!("imported\t{jira_key}\t{analysis_run_id}\t{session_id}");
            }
            ops::jira_import::Result::ExistingActive {
                jira_key,
                analysis_run_id,
                ..
            } => {
                let run = analysis_run_id.as_deref().unwrap_or("-");
                println!("existing_active\t{jira_key}\t{run}");
            }
        }
    }
    Ok(())
}

/// Discard an imported Jira issue. Prints whether a worktree was removed and
/// how many subtask sidecars were forgotten.
async fn cmd_jira_discard(
    client: &mut Client,
    json: bool,
    site: String,
    issue: String,
    force: bool,
) -> Result<()> {
    let args = ops::jira_discard::Args {
        jira_site: site,
        jira_issue_id: issue,
        force,
    };
    let result: ops::jira_discard::Result = client.request(ops::OP_JIRA_DISCARD, args).await?;
    if json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        println!(
            "discarded\tworktree_removed={}\tforgotten_task_worktrees={}",
            result.worktree_removed, result.forgotten_task_worktrees
        );
    }
    Ok(())
}

async fn cmd_quality(
    client: &mut Client,
    json: bool,
    task: Option<String>,
    project: Option<String>,
) -> Result<()> {
    // Resolve project locally when possible (explicit flag → env); fall back
    // to None so the app resolves its active project (PLAN §B.5). `--task`
    // defaults to $TASKAI_TASK_ID. Unlike `new-task`, no project here is NOT
    // a hard CLI error — the app can still resolve via task or active project;
    // an unknown project surfaces as the server's `unknown_project` diagnostic
    // (exit 30), preserving the right exit code on resolution failure.
    let project = project
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("TASKAI_PROJECT_ID").ok())
        .or_else(|| std::env::var("CADENZA_PROJECT_ID").ok())
        .filter(|s| !s.trim().is_empty());
    let task = task
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("TASKAI_TASK_ID").ok())
        .filter(|s| !s.trim().is_empty());

    let result: ops::quality::Result = client
        .request(ops::OP_QUALITY, ops::quality::Args { task, project })
        .await?;

    if json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        println!("contract: {}", result.contract_version);
        if result.checks.is_empty() {
            println!("(no quality checks configured)");
        } else {
            for c in &result.checks {
                let req = if c.required { "required" } else { "optional" };
                println!("{}\t[{}]\t{}\t{}", c.id, req, c.name, c.cmd);
                if !c.required_if_changed.is_empty() {
                    println!(
                        "\trequired_if_changed: {}",
                        c.required_if_changed.join(", ")
                    );
                }
            }
        }
    }
    Ok(())
}

/// Idempotency-key validation failed → exit 2 (bad usage). Maps to the clap
/// usage exit code in `main` via this dedicated error type.
#[derive(Debug)]
struct UsageError(String);
impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for UsageError {}

#[allow(clippy::too_many_arguments)]
async fn cmd_done(
    client: &mut Client,
    json: bool,
    negotiated_protocol: u32,
    task_id: String,
    summary: String,
    summary_flag: Option<String>,
    evidence_path: Option<PathBuf>,
    idempotency_key: Option<String>,
    legacy_done: bool,
) -> Result<()> {
    // `--summary` flag wins over the positional when both are given.
    let summary = summary_flag.unwrap_or(summary);

    // Resolve key: --idempotency-key → $CADENZA_IDEMPOTENCY_KEY → fresh v4
    // (mirror cmd_propose). A retried `done` only hits the server dedup path
    // when the SAME key is passed, so crash-prone agents should mint one
    // up-front. Validate with the rule shared (by duplication) with the app
    // (PLAN §B.6); a bad key is a usage error (exit 2).
    let idempotency_key = idempotency_key
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("CADENZA_IDEMPOTENCY_KEY").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    if let Err(e) = aliases::validate_idempotency_key(&idempotency_key) {
        return Err(anyhow::Error::new(UsageError(format!(
            "invalid idempotency-key: {e}"
        ))));
    }
    // Echo the resolved key to stderr in ALL modes so the agent can capture
    // and reuse it on retry.
    eprintln!("idempotency-key: {idempotency_key}");

    // Parse + validate evidence locally if provided (PLAN §B.7). Malformed or
    // over-cap ⇒ exit 2 (usage). Done before any wire send.
    let evidence = match evidence_path {
        None => None,
        Some(path) => Some(load_evidence(&path)?),
    };

    // Protocol downgrade gate (PLAN §B.6): evidence needs a v3 app. Against an
    // older app, exit 12 unless --legacy-done downgrades to the plain
    // positional done (dropping evidence). No evidence ⇒ works on any app.
    let send_evidence = match evidence_gate(evidence.is_some(), negotiated_protocol, legacy_done) {
        EvidenceGate::Send => evidence,
        EvidenceGate::Downgrade => {
            eprintln!(
                "warning: app protocol {negotiated_protocol} < 3; sending legacy done without evidence (--legacy-done)"
            );
            None
        }
        EvidenceGate::Reject => {
            return Err(anyhow::Error::new(WireError(
                cadenza_proto::ErrorBody::new(
                    "protocol_too_old",
                    format!(
                        "--evidence requires app protocol 3 (negotiated {negotiated_protocol}); pass --legacy-done to send without evidence"
                    ),
                ),
            )));
        }
    };

    let _: ops::done::Result = client
        .request(
            ops::OP_DONE,
            ops::done::Args {
                task_id,
                summary,
                evidence: send_evidence,
                idempotency_key: Some(idempotency_key.clone()),
            },
        )
        .await?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "ok": true, "idempotency_key": idempotency_key })
        );
    } else {
        println!("ok");
    }
    Ok(())
}

/// Outcome of the `done --evidence` protocol gate (PLAN §B.6).
#[derive(Debug, PartialEq, Eq)]
enum EvidenceGate {
    /// Send the request as-is (evidence kept if present).
    Send,
    /// Drop evidence and send the plain positional `done` (--legacy-done).
    Downgrade,
    /// Refuse: evidence requires a v3 app and no downgrade was requested.
    Reject,
}

/// Decide what to do with `--evidence` given the negotiated protocol. Pure so
/// the gate is unit-testable without a socket (PLAN §B.6 / D.2.4).
fn evidence_gate(has_evidence: bool, negotiated_protocol: u32, legacy_done: bool) -> EvidenceGate {
    if has_evidence && negotiated_protocol < 3 {
        if legacy_done {
            EvidenceGate::Downgrade
        } else {
            EvidenceGate::Reject
        }
    } else {
        EvidenceGate::Send
    }
}

/// Read + parse + cap-validate an evidence.json file. Any failure (missing
/// file, oversized file, bad JSON, cap violation) is a usage error (exit 2).
fn load_evidence(path: &std::path::Path) -> Result<ops::done::Evidence> {
    let meta = std::fs::metadata(path)
        .map_err(|e| anyhow::Error::new(UsageError(format!("evidence file {path:?}: {e}"))))?;
    if meta.len() > aliases::evidence_caps::MAX_FILE_BYTES {
        return Err(anyhow::Error::new(UsageError(format!(
            "evidence file too large: {} bytes (max {})",
            meta.len(),
            aliases::evidence_caps::MAX_FILE_BYTES
        ))));
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::Error::new(UsageError(format!("read evidence {path:?}: {e}"))))?;
    let ev: ops::done::Evidence = serde_json::from_str(&raw)
        .map_err(|e| anyhow::Error::new(UsageError(format!("malformed evidence JSON: {e}"))))?;
    aliases::validate_evidence(&ev)
        .map_err(|e| anyhow::Error::new(UsageError(format!("invalid evidence: {e}"))))?;
    Ok(ev)
}

async fn cmd_plan(
    client: &mut Client,
    json: bool,
    task_id: String,
    body: Option<String>,
    replace: bool,
) -> Result<()> {
    let body = match body {
        Some(b) => b,
        None => {
            use std::io::Read;
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .context("read plan body from stdin")?;
            s
        }
    };
    let _: ops::update_body::Result = client
        .request(
            ops::OP_UPDATE_BODY,
            ops::update_body::Args {
                task_id,
                body,
                append_plan: !replace,
            },
        )
        .await?;
    if json {
        println!("{{\"ok\":true}}");
    } else {
        println!("ok");
    }
    Ok(())
}

/// Resolver `--project`, ou a env injetada pelo app. O app injeta
/// `TASKAI_PROJECT_ID` no PTY do agente; aceitamos também o nome legado
/// `CADENZA_PROJECT_ID` como fallback de compatibilidade. Erro útil quando
/// nada disso está presente (agente rodado fora do PTY do app e sem
/// `--project`; use `cadenza-cli projects` para descobrir o id).
fn resolve_project(explicit: Option<String>) -> Result<String> {
    if let Some(p) = explicit.filter(|s| !s.trim().is_empty()) {
        return Ok(p);
    }
    for var in ["TASKAI_PROJECT_ID", "CADENZA_PROJECT_ID"] {
        if let Ok(p) = std::env::var(var) {
            if !p.trim().is_empty() {
                return Ok(p);
            }
        }
    }
    Err(anyhow::anyhow!(
        "project required (pass --project or set $TASKAI_PROJECT_ID)"
    ))
}

async fn cmd_new_task(
    client: &mut Client,
    json: bool,
    titulo: String,
    body: String,
    project: Option<String>,
    from_ideia: Option<String>,
) -> Result<()> {
    let project_id = resolve_project(project)?;
    let from_ideia = from_ideia.or_else(|| std::env::var("CADENZA_IDEIA_ID").ok());
    let args = ops::create_task::Args {
        id: None,
        titulo,
        body,
        project_id,
        from_ideia: from_ideia.filter(|s| !s.trim().is_empty()),
    };
    let result: ops::create_task::Result = client.request(ops::OP_CREATE_TASK, args).await?;
    if json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        println!("{}", result.task_id);
    }
    Ok(())
}

async fn cmd_list_ideias(client: &mut Client, json: bool) -> Result<()> {
    let ideias: ops::list_ideias::Result = client
        .request(ops::OP_LIST_IDEIAS, ops::list_ideias::Args::default())
        .await?;
    if json {
        println!("{}", serde_json::to_string(&ideias)?);
    } else if ideias.is_empty() {
        println!("(no ideias)");
    } else {
        for i in &ideias {
            println!("{}\t[{}]\t{}", i.id, i.status.as_str(), i.titulo);
        }
    }
    Ok(())
}

async fn cmd_read_ideia(client: &mut Client, json: bool, ideia_id: String) -> Result<()> {
    let ideia: ops::read_ideia::Result = client
        .request(
            ops::OP_READ_IDEIA,
            ops::read_ideia::Args {
                id: ideia_id.clone(),
            },
        )
        .await?;
    match ideia {
        None => {
            if json {
                println!("null");
            } else {
                eprintln!("ideia not found: {ideia_id}");
            }
            // Mesmo exit code que task_not_found.
            return Err(anyhow::Error::new(WireError(
                cadenza_proto::ErrorBody::new(
                    "task_not_found",
                    format!("ideia not found: {ideia_id}"),
                ),
            )));
        }
        Some(i) => {
            if json {
                println!("{}", serde_json::to_string(&i)?);
            } else {
                println!("# {}", i.titulo);
                println!("[{}]  project={}", i.status.as_str(), i.project_id);
                println!();
                println!("{}", i.body);
            }
        }
    }
    Ok(())
}

async fn cmd_create_ideia(
    client: &mut Client,
    json: bool,
    titulo: String,
    body: String,
    project: Option<String>,
) -> Result<()> {
    let project_id = resolve_project(project)?;
    let args = ops::create_ideia::Args {
        id: None,
        titulo,
        body,
        project_id,
    };
    let ideia: ops::create_ideia::Result = client.request(ops::OP_CREATE_IDEIA, args).await?;
    if json {
        println!("{}", serde_json::to_string(&ideia)?);
    } else {
        println!("{}", ideia.id);
    }
    Ok(())
}

async fn cmd_delete_ideia(client: &mut Client, json: bool, ideia_id: String) -> Result<()> {
    let _: ops::delete_ideia::Result = client
        .request(
            ops::OP_DELETE_IDEIA,
            ops::delete_ideia::Args { id: ideia_id },
        )
        .await?;
    if json {
        println!("{{\"ok\":true}}");
    } else {
        println!("ok");
    }
    Ok(())
}

async fn cmd_memory(client: &mut Client, json: bool, cmd: MemoryCmd) -> Result<()> {
    match cmd {
        MemoryCmd::List { project } => {
            let project_id = resolve_project(project)?;
            let items: ops::list_memory::Result = client
                .request(ops::OP_LIST_MEMORY, ops::list_memory::Args { project_id })
                .await?;
            if json {
                println!("{}", serde_json::to_string(&items)?);
            } else if items.is_empty() {
                println!("(memória vazia)");
            } else {
                for it in &items {
                    println!("{}\t{}", it.id, it.texto);
                }
            }
        }
        MemoryCmd::Suggest {
            texto,
            project,
            task,
        } => {
            let project_id = resolve_project(project)?;
            let origem_task = task
                .or_else(|| std::env::var("TASKAI_TASK_ID").ok())
                .filter(|s| !s.trim().is_empty());
            let result: ops::suggest_learning::Result = client
                .request(
                    ops::OP_SUGGEST_LEARNING,
                    ops::suggest_learning::Args {
                        project_id,
                        texto,
                        origem_task,
                    },
                )
                .await?;
            if json {
                println!("{}", serde_json::to_string(&result)?);
            } else {
                println!("{}", result.suggestion_id);
            }
        }
        MemoryCmd::Revise {
            op,
            project,
            targets,
            texto,
            nota,
        } => {
            let project_id = resolve_project(project)?;
            let kind = build_reeval_kind(&op, targets, texto, nota)?;
            let result: ops::revise_memory::Result = client
                .request(
                    ops::OP_REVISE_MEMORY,
                    ops::revise_memory::Args { project_id, kind },
                )
                .await?;
            if json {
                println!("{}", serde_json::to_string(&result)?);
            } else {
                println!("{}", result.suggestion_id);
            }
        }
    }
    Ok(())
}

/// Map `--op` + flags onto a reeval `SuggestionKind`, validating that the
/// flags required by each op are present. `aprendizado` is rejected here
/// — that's `memory suggest`, not `revise`.
fn build_reeval_kind(
    op: &str,
    targets: Vec<String>,
    texto: Option<String>,
    nota: Option<String>,
) -> Result<cadenza_proto::SuggestionKind> {
    use cadenza_proto::SuggestionKind;
    let one_target = |targets: &[String]| -> Result<String> {
        match targets {
            [t] => Ok(t.clone()),
            _ => Err(anyhow::anyhow!("op '{op}' requires exactly one --target")),
        }
    };
    let need_texto = |texto: Option<String>| -> Result<String> {
        texto
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("op '{op}' requires --texto"))
    };
    match op {
        "remover" => Ok(SuggestionKind::Remover {
            target_id: one_target(&targets)?,
        }),
        "reescrever" => Ok(SuggestionKind::Reescrever {
            target_id: one_target(&targets)?,
            novo_texto: need_texto(texto)?,
        }),
        "mesclar" => {
            if targets.len() < 2 {
                return Err(anyhow::anyhow!(
                    "op 'mesclar' requires at least two --target"
                ));
            }
            Ok(SuggestionKind::Mesclar {
                target_ids: targets,
                texto_mesclado: need_texto(texto)?,
            })
        }
        "nova" => Ok(SuggestionKind::Nova {
            texto: need_texto(texto)?,
        }),
        "contradicao" => {
            if targets.len() < 2 {
                return Err(anyhow::anyhow!(
                    "op 'contradicao' requires at least two --target"
                ));
            }
            Ok(SuggestionKind::Contradicao {
                target_ids: targets,
                nota: nota
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow::anyhow!("op 'contradicao' requires --nota"))?,
            })
        }
        other => Err(anyhow::anyhow!(
            "unknown --op '{other}' (use remover|reescrever|mesclar|nova|contradicao)"
        )),
    }
}

async fn cmd_set_worktree(
    client: &mut Client,
    json: bool,
    task_id: String,
    worktree_path: Option<String>,
    branch: Option<String>,
) -> Result<()> {
    let args = cadenza_proto::ops::set_task_worktree::Args {
        task_id,
        worktree_path,
        branch,
    };
    let _: cadenza_proto::ops::set_task_worktree::Result = client
        .request(cadenza_proto::ops::OP_SET_TASK_WORKTREE, args)
        .await?;
    if json {
        println!("{{\"ok\":true}}");
    } else {
        println!("ok");
    }
    Ok(())
}

fn run_diag() -> Result<()> {
    let home = data_dir();
    let auth_path = home.join("auth");
    let socket_hint = if cfg!(windows) {
        format!(
            "\\\\.\\pipe\\cadenza-{}",
            std::env::var("USERNAME").unwrap_or_else(|_| "<user>".into())
        )
    } else {
        home.join("run").join("socket").display().to_string()
    };

    println!("cadenza-cli {}", env!("CARGO_PKG_VERSION"));
    println!("protocol: {}", cadenza_proto::MAX_PROTOCOL);
    println!("data dir: {}", home.display());
    println!(
        "auth file: {} ({})",
        auth_path.display(),
        if auth_path.exists() {
            "exists"
        } else {
            "MISSING"
        }
    );
    println!("socket: {socket_hint}");
    Ok(())
}

pub(crate) fn data_dir() -> PathBuf {
    // CADENZA_DATA_DIR overrides the default so integration tests can point
    // to a temp directory without touching the real ~/.cadenza. An empty
    // value (`export CADENZA_DATA_DIR=`) falls through to the home_dir
    // branch — otherwise PathBuf::from("") resolves to the cwd and
    // read_token would read `./auth` from whatever directory the agent ran in.
    if let Ok(dir) = std::env::var("CADENZA_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".cadenza")
}

fn read_token() -> Result<String> {
    let path = data_dir().join("auth");
    let s = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "read CLI token at {} (is the Cadenza app running?)",
            path.display()
        )
    })?;
    Ok(s.trim().to_string())
}

#[derive(Debug)]
struct TokenError(String);
impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for TokenError {}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn jira_materialize_reads_secret_from_env() {
        // env path (secret_stdin = false) takes the env value.
        let got = resolve_run_secret(false, Some("env-secret".into()), None).unwrap();
        assert_eq!(got, "env-secret");
    }

    #[test]
    fn jira_materialize_secret_stdin_flag_reads_stdin() {
        // stdin path (secret_stdin = true) takes the stdin line, ignoring env.
        let got = resolve_run_secret(
            true,
            Some("env-secret".into()),
            Some("stdin-secret\n".into()),
        )
        .unwrap();
        assert_eq!(got, "stdin-secret");
    }

    #[test]
    fn jira_materialize_missing_secret_is_usage_error() {
        let err = resolve_run_secret(false, None, None).unwrap_err();
        // Maps to the bad-usage exit code (2) via UsageError.
        assert!(err.downcast_ref::<UsageError>().is_some());
        // Empty/whitespace env is also rejected.
        let err2 = resolve_run_secret(false, Some("   ".into()), None).unwrap_err();
        assert!(err2.downcast_ref::<UsageError>().is_some());
    }

    #[test]
    fn jira_materialize_parses_subtasks_file() {
        let text = r#"[{"title":"a","body":"b1"},{"title":"c","body":""}]"#;
        let parsed = parse_subtasks(text).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].title, "a");
        assert_eq!(parsed[1].body, "");
        // Malformed JSON ⇒ UsageError (exit 2).
        let err = parse_subtasks("not json").unwrap_err();
        assert!(err.downcast_ref::<UsageError>().is_some());
    }

    #[test]
    fn jira_materialize_clap_parses() {
        let cli = Cli::try_parse_from([
            "cadenza-cli",
            "jira-materialize",
            "--analysis-run-id",
            "run-1",
            "--subtasks-file",
            "subs.json",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::JiraMaterialize {
                analysis_run_id,
                secret_stdin,
                subtasks_file,
            } => {
                assert_eq!(analysis_run_id, "run-1");
                assert!(!secret_stdin);
                assert_eq!(subtasks_file, "subs.json");
            }
            other => panic!("expected Cmd::JiraMaterialize, got {other:?}"),
        }
    }

    #[test]
    fn jira_review_clap_parses() {
        let cli = Cli::try_parse_from([
            "cadenza-cli",
            "jira-review",
            "--site",
            "https://x.atlassian.net",
            "--issue",
            "10001",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::JiraReview { site, issue, json } => {
                assert_eq!(site, "https://x.atlassian.net");
                assert_eq!(issue, "10001");
                assert!(!json);
            }
            other => panic!("expected Cmd::JiraReview, got {other:?}"),
        }
    }

    /// `plan <id> --body "..."` parses with append-by-default semantics.
    #[test]
    fn plan_with_body_parses() {
        let cli = Cli::try_parse_from(["cadenza-cli", "plan", "T-1", "--body", "do X"]).unwrap();
        match cli.cmd {
            Cmd::Plan {
                task_id,
                body,
                replace,
            } => {
                assert_eq!(task_id, "T-1");
                assert_eq!(body.as_deref(), Some("do X"));
                assert!(!replace);
            }
            other => panic!("expected Cmd::Plan, got {other:?}"),
        }
    }

    #[test]
    fn reeval_kind_remover_needs_one_target() {
        use cadenza_proto::SuggestionKind;
        let ok = build_reeval_kind("remover", vec!["M-1".into()], None, None).unwrap();
        assert!(matches!(ok, SuggestionKind::Remover { .. }));
        assert!(build_reeval_kind("remover", vec![], None, None).is_err());
        assert!(
            build_reeval_kind("remover", vec!["M-1".into(), "M-2".into()], None, None).is_err()
        );
    }

    #[test]
    fn reeval_kind_reescrever_needs_target_and_texto() {
        assert!(
            build_reeval_kind("reescrever", vec!["M-1".into()], Some("novo".into()), None).is_ok()
        );
        assert!(build_reeval_kind("reescrever", vec!["M-1".into()], None, None).is_err());
    }

    #[test]
    fn reeval_kind_mesclar_needs_two_targets_and_texto() {
        assert!(build_reeval_kind(
            "mesclar",
            vec!["M-1".into(), "M-2".into()],
            Some("fundido".into()),
            None
        )
        .is_ok());
        assert!(build_reeval_kind("mesclar", vec!["M-1".into()], Some("x".into()), None).is_err());
    }

    #[test]
    fn reeval_kind_rejects_unknown_and_aprendizado() {
        assert!(build_reeval_kind("aprendizado", vec![], Some("x".into()), None).is_err());
        assert!(build_reeval_kind("bogus", vec![], None, None).is_err());
    }

    #[test]
    fn done_positional_summary_parses() {
        let cli = Cli::try_parse_from(["cadenza-cli", "done", "T-1", "all good"]).unwrap();
        match cli.cmd {
            Cmd::Done {
                task_id,
                summary,
                evidence,
                summary_flag,
                idempotency_key,
                legacy_done,
            } => {
                assert_eq!(task_id, "T-1");
                assert_eq!(summary, "all good");
                assert!(evidence.is_none());
                assert!(summary_flag.is_none());
                assert!(idempotency_key.is_none());
                assert!(!legacy_done);
            }
            other => panic!("expected Cmd::Done, got {other:?}"),
        }
    }

    #[test]
    fn done_full_flags_parse() {
        let cli = Cli::try_parse_from([
            "cadenza-cli",
            "done",
            "T-2",
            "--summary",
            "via flag",
            "--evidence",
            "ev.json",
            "--idempotency-key",
            "k-1",
            "--legacy-done",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Done {
                task_id,
                summary,
                evidence,
                summary_flag,
                idempotency_key,
                legacy_done,
            } => {
                assert_eq!(task_id, "T-2");
                assert_eq!(summary, ""); // positional omitted → default
                assert_eq!(summary_flag.as_deref(), Some("via flag"));
                assert_eq!(evidence.as_deref(), Some(std::path::Path::new("ev.json")));
                assert_eq!(idempotency_key.as_deref(), Some("k-1"));
                assert!(legacy_done);
            }
            other => panic!("expected Cmd::Done, got {other:?}"),
        }
    }

    #[test]
    fn quality_parses_flags_and_defaults() {
        let cli = Cli::try_parse_from(["cadenza-cli", "quality"]).unwrap();
        match cli.cmd {
            Cmd::Quality { task, project } => {
                assert!(task.is_none());
                assert!(project.is_none());
            }
            other => panic!("expected Cmd::Quality, got {other:?}"),
        }
        let cli = Cli::try_parse_from([
            "cadenza-cli",
            "quality",
            "--project",
            "P-1",
            "--task",
            "T-9",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Quality { task, project } => {
                assert_eq!(task.as_deref(), Some("T-9"));
                assert_eq!(project.as_deref(), Some("P-1"));
            }
            other => panic!("expected Cmd::Quality, got {other:?}"),
        }
    }

    #[test]
    fn evidence_gate_no_evidence_always_sends() {
        assert_eq!(evidence_gate(false, 1, false), EvidenceGate::Send);
        assert_eq!(evidence_gate(false, 3, false), EvidenceGate::Send);
    }

    #[test]
    fn evidence_gate_v3_sends() {
        assert_eq!(evidence_gate(true, 3, false), EvidenceGate::Send);
        assert_eq!(evidence_gate(true, 4, false), EvidenceGate::Send);
    }

    #[test]
    fn evidence_gate_pre_v3_rejects_without_legacy() {
        assert_eq!(evidence_gate(true, 2, false), EvidenceGate::Reject);
    }

    #[test]
    fn evidence_gate_pre_v3_downgrades_with_legacy() {
        assert_eq!(evidence_gate(true, 2, true), EvidenceGate::Downgrade);
    }

    /// `plan <id> --replace` with no `--body` → stdin path, replace=true.
    #[test]
    fn plan_replace_without_body_parses() {
        let cli = Cli::try_parse_from(["cadenza-cli", "plan", "T-2", "--replace"]).unwrap();
        match cli.cmd {
            Cmd::Plan {
                task_id,
                body,
                replace,
            } => {
                assert_eq!(task_id, "T-2");
                assert!(body.is_none());
                assert!(replace);
            }
            other => panic!("expected Cmd::Plan, got {other:?}"),
        }
    }
}
