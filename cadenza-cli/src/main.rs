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
    Current,
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
    /// Request completion — the human still has the final word.
    Done { task_id: String, summary: String },
    /// Create a new task in `a_fazer`, bound to a project. Used by the
    /// "destrinchar ideia" flow: o agente chama isso N vezes para
    /// transformar uma ideia em tasks concretas. Defaults pegam do
    /// ambiente do PTY do agente (`$CADENZA_PROJECT_ID`,
    /// `$CADENZA_IDEIA_ID`).
    NewTask {
        #[arg(long)]
        titulo: String,
        #[arg(long, default_value = "")]
        body: String,
        /// Project ID (default: $CADENZA_PROJECT_ID).
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
        /// Project ID (default: $CADENZA_PROJECT_ID).
        #[arg(long)]
        project: Option<String>,
    },
    /// Delete an ideia.
    DeleteIdeia { ideia_id: String },
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
    /// Install / remove the Cadenza skill in Claude or Codex.
    Skill(SkillCmd),
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
    let _ = client.hello(&token).await?;

    match cli.cmd {
        Cmd::List { estado } => cmd_list(&mut client, cli.json, estado).await?,
        Cmd::Current => cmd_current(&mut client, cli.json).await?,
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
        Cmd::Done { task_id, summary } => cmd_done(&mut client, cli.json, task_id, summary).await?,
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

async fn cmd_done(client: &mut Client, json: bool, task_id: String, summary: String) -> Result<()> {
    let _: ops::done::Result = client
        .request(ops::OP_DONE, ops::done::Args { task_id, summary })
        .await?;
    if json {
        println!("{{\"ok\":true}}");
    } else {
        println!("ok");
    }
    Ok(())
}

/// Resolver `--project` ou env `CADENZA_PROJECT_ID`. Erro útil quando
/// nenhum dos dois está presente (agente foi rodado fora do PTY do app
/// e esqueceu de passar `--project`).
fn resolve_project(explicit: Option<String>) -> Result<String> {
    if let Some(p) = explicit.filter(|s| !s.trim().is_empty()) {
        return Ok(p);
    }
    if let Ok(p) = std::env::var("CADENZA_PROJECT_ID") {
        if !p.trim().is_empty() {
            return Ok(p);
        }
    }
    Err(anyhow::anyhow!(
        "project required (pass --project or set $CADENZA_PROJECT_ID)"
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
