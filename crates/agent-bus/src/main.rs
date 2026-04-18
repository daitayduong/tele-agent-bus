//! tele-agent-bus daemon + CLI.
#![deny(unsafe_code)]

mod cli;
mod daemon;

use clap::{Parser, Subcommand};
use crate::cli::auth::RegisterArgs;
use crate::cli::blacklist::BlacklistCommands;

#[derive(Parser)]
#[command(name = "agent-bus")]
#[command(about = "tele-agent-bus daemon + CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize the agent-bus configuration
    Init,
    /// Repository management
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
    },
    /// Configuration management
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Blacklist management
    Blacklist {
        #[command(subcommand)]
        command: BlacklistCommands,
    },
    /// Auth-context management (per-agent OAuth profiles for rotation)
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },
    /// Start the agent-bus daemon
    Daemon,
}

#[derive(Subcommand)]
enum AuthCommands {
    /// Register a new auth context (creates profile_dir with 0700)
    Register {
        agent: String,
        id: String,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        require_owner_approval: bool,
    },
    /// Launch the provider's `login` flow inside the context's profile_dir
    Login { agent: String, id: String },
    /// List registered auth contexts
    List { agent: Option<String> },
    /// Mark a context as the persistent active one for its agent
    Use { agent: String, id: String },
    /// Disable a context (enabled=false)
    Pause { agent: String, id: String },
    /// Re-enable a context (enabled=true)
    Resume { agent: String, id: String },
    /// Quick health check: run `<bin> --version` under the context env
    Recheck { agent: String, id: String },
}

#[derive(Subcommand)]
enum RepoCommands {
    /// Add a repository
    Add { path: String },
    /// List registered repositories
    List,
    /// Remove a repository by ID
    Remove { id: String },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Show current configuration
    Show,
    /// Validate configuration
    Validate,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init => cli::init::run()?,
        Commands::Repo { command } => match command {
            RepoCommands::Add { path } => cli::repo::add(&path)?,
            RepoCommands::List => cli::repo::list()?,
            RepoCommands::Remove { id } => cli::repo::remove(&id)?,
        },
        Commands::Config { command } => match command {
            ConfigCommands::Show => cli::config::show()?,
            ConfigCommands::Validate => cli::config::validate()?,
        },
        Commands::Blacklist { command } => cli::blacklist::handle(command)?,
        Commands::Auth { command } => match command {
            AuthCommands::Register {
                agent,
                id,
                label,
                require_owner_approval,
            } => cli::auth::register(RegisterArgs {
                agent,
                id,
                label,
                require_owner_approval: if require_owner_approval { Some(true) } else { None },
            })?,
            AuthCommands::Login { agent, id } => cli::auth::login(agent, id)?,
            AuthCommands::List { agent } => cli::auth::list(agent)?,
            AuthCommands::Use { agent, id } => cli::auth::set_use(agent, id)?,
            AuthCommands::Pause { agent, id } => cli::auth::pause(agent, id)?,
            AuthCommands::Resume { agent, id } => cli::auth::resume(agent, id)?,
            AuthCommands::Recheck { agent, id } => cli::auth::recheck(agent, id)?,
        },
        Commands::Daemon => {
            tracing_subscriber::fmt::init();
            let config = daemon::load_daemon_config()?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(daemon::run_daemon(config))?;
        }
    }

    Ok(())
}
