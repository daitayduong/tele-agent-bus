//! tele-agent-bus daemon + CLI.
//!
//! Subcommands (per spec §3.3):
//!   - `agent-bus init` — first-time setup
//!   - `agent-bus daemon` — start daemon (invoked by systemd)
//!   - `agent-bus repo add|list|remove|install-hook`
//!   - `agent-bus start|stop|restart|status`
//!   - `agent-bus logs [--follow]`
//!   - `agent-bus perms [--pending]`
//!   - `agent-bus migrate <path>`
//!   - `agent-bus config [show|validate]`

#![deny(unsafe_code)]

use clap::{Parser, Subcommand};

mod daemon;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Daemon) => {
            tracing_subscriber::fmt::init();
            let config = daemon::load_daemon_config()?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(daemon::run_daemon(config))
        }
        None => Ok(()),
    }
}

#[derive(Debug, Parser)]
#[command(name = "agent-bus")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Daemon,
}
