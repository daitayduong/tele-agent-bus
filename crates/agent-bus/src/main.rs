//! tele-agent-bus daemon + CLI.
#![deny(unsafe_code)]

mod cli;
mod daemon;

use clap::{Parser, Subcommand};

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
    /// Start the agent-bus daemon
    Daemon,
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
