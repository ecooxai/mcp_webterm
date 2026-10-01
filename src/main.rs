use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use tracing::info;
use tracing_subscriber::EnvFilter;
use webterm::{
    VERSION,
    config::Config,
    db::{Database, Terminal, Workspace, canonical_workspace_path},
    runtime,
    terminal::TerminalManager,
    tui, web,
};

#[derive(Parser)]
#[command(name = "webterm", version = VERSION, about = "Persistent named terminal workspaces")]
struct Cli {
    /// Configuration TOML file. WEBTERM_CONFIG is also supported.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Development web password. Prefer a protected password-hash file in production.
    #[arg(long, global = true, env = "WEBTERM_PASSWORD")]
    passwd: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP service.
    Serve,
    /// Run the native PTY runtime daemon in the foreground.
    Runtime,
    /// Print the effective non-secret configuration.
    Config,
    /// List workspaces, with their terminals nested underneath.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Create, inspect, update, or remove workspaces.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Manage persistent named terminal sessions.
    Terminal {
        #[command(subcommand)]
        command: TerminalCommand,
    },
    /// Execute the compact MCP grammar, e.g. webterm cmd 'read /workspace 1'.
    Cmd { cmd: String },
    /// Compact aliases: new/read/write/run/python/ls/ensure/resize/stop/status.
    #[command(external_subcommand)]
    Compact(Vec<String>),
    /// Open the interactive workspace/terminal hierarchy.
    Tui,
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    /// Add an existing folder as a workspace.
    Add(WorkspaceAdd),
    /// List all workspaces.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one workspace by numeric ID or name.
    Show {
        workspace: String,
        #[arg(long)]
        json: bool,
    },
    /// Change a workspace's name and/or folder.
    Update(WorkspaceUpdate),
    /// Remove a workspace record (never removes its folder).
    Remove {
        workspace: String,
        /// Stop and delete all terminal records in this workspace.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum TerminalCommand {
    /// Create a persistent shell in a workspace folder.
    Create {
        workspace: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// List terminals, optionally restricted to one workspace.
    List {
        workspace: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Attach the current TTY to a running terminal.
    Attach { workspace: String, terminal: String },
    /// Send literal text to a terminal.
    Write {
        workspace: String,
        terminal: String,
        #[arg(long)]
        data: String,
        /// Send the Enter key after the literal text.
        #[arg(long)]
        enter: bool,
    },
    /// Set a terminal's window dimensions.
    Resize {
        workspace: String,
        terminal: String,
        #[arg(long)]
        columns: u16,
        #[arg(long)]
        rows: u16,
    },
    /// Stop a terminal while retaining its history record.
    Stop {
        workspace: String,
        terminal: String,
        #[arg(long)]
        json: bool,
    },
    /// Rename a terminal while preserving its running session.
    Rename {
        workspace: String,
        terminal: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Stop a terminal and delete its history record.
    Delete {
        workspace: String,
        terminal: String,
        #[arg(long)]
        json: bool,
    },
    /// Print recent visible terminal output.
    Capture {
        workspace: String,
        terminal: String,
        #[arg(long, default_value_t = 100)]
        lines: u16,
    },
}

#[derive(Args)]
struct WorkspaceAdd {
    name: String,
    path: PathBuf,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct WorkspaceUpdate {
    workspace: String,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    path: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("webterm=info")),
        )
        .init();
    let cli = Cli::parse();
    let mut config = Config::load(cli.config.as_deref())?;
    config.web_password = cli.passwd;
    match cli.command.unwrap_or(Command::Tui) {
        Command::Serve => serve(config).await,
        Command::Runtime => {
            config.ensure_state_dirs()?;
            runtime::serve(&config.runtime_socket)
        }
        Command::Config => {
            println!("{}", toml::to_string_pretty(&config)?);
            Ok(())
        }
        Command::List { json } => list_workspaces(&config, json),
        Command::Workspace { command } => workspace_command(&config, command),
        Command::Terminal { command } => terminal_command(&config, command),
        Command::Cmd { cmd } => webterm::shell_tool::native(&config, &["cmd".into(), cmd]),
        Command::Compact(args) => webterm::shell_tool::native(&config, &args),
        Command::Tui => {
            let database = Database::open_config(&config)?;
            let terminals = TerminalManager::new(&config)?;
            reconcile_all(&database, &terminals)?;
            tui::run(&database, &terminals)
        }
    }
}

fn workspace_command(config: &Config, command: WorkspaceCommand) -> Result<()> {
    let database = Database::open_config(config)?;
    match command {
        WorkspaceCommand::Add(args) => {
            let path = canonical_workspace_path(config, &args.path)?;
            print_workspace(&database.create_workspace(&args.name, &path)?, args.json)
        }
        WorkspaceCommand::List { json } => print_workspaces(&database.list_workspaces()?, json),
        WorkspaceCommand::Show { workspace, json } => {
            print_workspace(&database.workspace(&workspace)?, json)
        }
        WorkspaceCommand::Update(args) => {
            let path = args
                .path
                .as_deref()
                .map(|path| canonical_workspace_path(config, path))
                .transpose()?;
            let workspace = database.update_workspace(
                &args.workspace,
                args.name.as_deref(),
                path.as_deref(),
            )?;
            print_workspace(&workspace, args.json)
        }
        WorkspaceCommand::Remove {
            workspace,
            force,
            json,
        } => {
            let record = database.workspace(&workspace)?;
            let terminals = database.list_terminals(Some(record.id))?;
            if !terminals.is_empty() && !force {
                anyhow::bail!(
                    "workspace has {} terminal record(s); stop/remove them with --force",
                    terminals.len()
                )
            }
            if force {
                let manager = TerminalManager::new(config)?;
                for terminal in terminals {
                    manager.stop(terminal.session_id())?;
                    database.delete_terminal(terminal.id)?;
                }
            }
            print_workspace(&database.remove_workspace(&workspace)?, json)
        }
    }
}

fn list_workspaces(config: &Config, json: bool) -> Result<()> {
    let database = Database::open_config(config)?;
    let manager = TerminalManager::new(config)?;
    reconcile_all(&database, &manager)?;
    print_hierarchy(
        &database.list_workspaces()?,
        &database.list_terminals(None)?,
        json,
    )
}

fn print_workspaces(workspaces: &[Workspace], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(workspaces)?);
    } else if workspaces.is_empty() {
        println!("No workspaces. Add one with: webterm workspace add NAME PATH");
    } else {
        for workspace in workspaces {
            println!(
                "{}  {}  {}",
                workspace.id,
                workspace.name,
                workspace.path.display()
            );
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct WorkspaceTree<'a> {
    #[serde(flatten)]
    workspace: &'a Workspace,
    terminals: Vec<&'a Terminal>,
}

fn print_hierarchy(workspaces: &[Workspace], terminals: &[Terminal], json: bool) -> Result<()> {
    if json {
        let tree: Vec<_> = workspaces
            .iter()
            .map(|workspace| WorkspaceTree {
                workspace,
                terminals: terminals
                    .iter()
                    .filter(|terminal| terminal.workspace_id == workspace.id)
                    .collect(),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&tree)?);
    } else if workspaces.is_empty() {
        println!("No workspaces. Add one with: webterm workspace add NAME PATH");
    } else {
        for workspace in workspaces {
            println!(
                "{}  {}  {}",
                workspace.id,
                workspace.name,
                workspace.path.display()
            );
            for terminal in terminals
                .iter()
                .filter(|terminal| terminal.workspace_id == workspace.id)
            {
                println!("  {}  {}  {}", terminal.id, terminal.name, terminal.status);
            }
        }
    }
    Ok(())
}

fn print_workspace(workspace: &Workspace, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(workspace)?);
    } else {
        println!(
            "{}  {}  {}",
            workspace.id,
            workspace.name,
            workspace.path.display()
        );
    }
    Ok(())
}

fn terminal_command(config: &Config, command: TerminalCommand) -> Result<()> {
    let database = Database::open_config(config)?;
    let manager = TerminalManager::new(config)?;
    match command {
        TerminalCommand::Create {
            workspace,
            name,
            json,
        } => {
            let workspace = database.workspace(&workspace)?;
            let terminal = database.reserve_terminal(workspace.id, &name)?;
            if let Err(error) = manager.create(terminal.session_id(), &workspace.path) {
                database.delete_terminal(terminal.id)?;
                return Err(error);
            }
            print_terminal(&database.set_terminal_status(terminal.id, "running")?, json)
        }
        TerminalCommand::List { workspace, json } => {
            let workspace_id = workspace
                .as_deref()
                .map(|selector| database.workspace(selector).map(|item| item.id))
                .transpose()?;
            reconcile_all(&database, &manager)?;
            print_terminals(&database.list_terminals(workspace_id)?, json)
        }
        TerminalCommand::Attach {
            workspace,
            terminal,
        } => {
            let terminal = resolve_terminal(&database, &workspace, &terminal)?;
            manager.attach(terminal.session_id())
        }
        TerminalCommand::Write {
            workspace,
            terminal,
            data,
            enter,
        } => {
            let terminal = resolve_terminal(&database, &workspace, &terminal)?;
            manager.write(terminal.session_id(), &data, enter)
        }
        TerminalCommand::Resize {
            workspace,
            terminal,
            columns,
            rows,
        } => {
            let terminal = resolve_terminal(&database, &workspace, &terminal)?;
            manager.resize(terminal.session_id(), columns, rows)
        }
        TerminalCommand::Stop {
            workspace,
            terminal,
            json,
        } => {
            let terminal = resolve_terminal(&database, &workspace, &terminal)?;
            manager.stop(terminal.session_id())?;
            print_terminal(&database.set_terminal_status(terminal.id, "stopped")?, json)
        }
        TerminalCommand::Rename {
            workspace,
            terminal,
            name,
            json,
        } => {
            let terminal = resolve_terminal(&database, &workspace, &terminal)?;
            print_terminal(&database.rename_terminal(terminal.id, &name)?, json)
        }
        TerminalCommand::Delete {
            workspace,
            terminal,
            json,
        } => {
            let terminal = resolve_terminal(&database, &workspace, &terminal)?;
            manager.stop(terminal.session_id())?;
            database.delete_terminal(terminal.id)?;
            print_terminal(&terminal, json)
        }
        TerminalCommand::Capture {
            workspace,
            terminal,
            lines,
        } => {
            let terminal = resolve_terminal(&database, &workspace, &terminal)?;
            print!("{}", manager.capture(terminal.session_id(), lines)?);
            Ok(())
        }
    }
}

fn resolve_terminal(database: &Database, workspace: &str, terminal: &str) -> Result<Terminal> {
    let workspace = database.workspace(workspace)?;
    database.terminal(workspace.id, terminal)
}

fn reconcile_all(database: &Database, manager: &TerminalManager) -> Result<()> {
    for terminal in database.list_terminals(None)? {
        // An unreachable backend is an error, not evidence that a live record
        // stopped. Propagate it without changing this record's status.
        let actual = if manager.has_session(terminal.session_id())? {
            "running"
        } else {
            "stopped"
        };
        if terminal.status != actual {
            database.set_terminal_status(terminal.id, actual)?;
        }
    }
    Ok(())
}

fn print_terminals(terminals: &[Terminal], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(terminals)?);
    } else if terminals.is_empty() {
        println!("No terminals.");
    } else {
        for terminal in terminals {
            println!(
                "{}  workspace={}  {}  {}",
                terminal.id, terminal.workspace_id, terminal.name, terminal.status
            );
        }
    }
    Ok(())
}

fn print_terminal(terminal: &Terminal, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(terminal)?);
    } else {
        println!(
            "{}  workspace={}  {}  {}",
            terminal.id, terminal.workspace_id, terminal.name, terminal.status
        );
    }
    Ok(())
}

async fn serve(config: Config) -> Result<()> {
    config.ensure_state_dirs()?;
    webterm::db::ensure_default_workspace_folder(&config)?;

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("bind {}", config.listen))?;
    let proxy_addr: std::net::SocketAddr = match std::env::var("WEBTERM_PROXY_LISTEN") {
        Ok(value) => value.parse().context("parse WEBTERM_PROXY_LISTEN")?,
        Err(_) => std::net::SocketAddr::from(([0, 0, 0, 0], webterm::generic_proxy::PROXY_PORT)),
    };
    let proxy_listener = tokio::net::TcpListener::bind(proxy_addr)
        .await
        .with_context(|| format!("bind URL proxy {proxy_addr}"))?;

    info!(address = %config.listen, version = VERSION, "webterm listening");
    info!(address = %proxy_listener.local_addr()?, "webterm URL proxy listening");

    let web_router = web::router(config)?;
    let proxy_router = webterm::generic_proxy::router()?;

    let web_task = tokio::spawn(async move { axum::serve(listener, web_router).await });
    let proxy_task = tokio::spawn(async move {
        axum::serve(
            proxy_listener,
            proxy_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    });

    tokio::select! {
        result = web_task => {
            result.context("join WebTerm HTTP server")?
                .context("HTTP server")
        }
        result = proxy_task => {
            result.context("join URL proxy server")?
                .context("URL proxy server")
        }
        _ = shutdown_signal() => Ok(()),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler")
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}
