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

fn main() -> anyhow::Result<()> {
    // TODO: wire up clap + subcommand dispatch per spec §3.3
    Ok(())
}
