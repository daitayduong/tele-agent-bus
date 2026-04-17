//! PreToolUse hook binary. Spawned per Bash tool invocation by Claude Code.
//!
//! Flow (per spec §3.4, §7, §10):
//!   1. Read hook-call JSON from stdin
//!   2. Connect to `~/.agent-bus/daemon.sock`
//!   3. POST /perm/check with PROTOCOL_VERSION=1
//!   4. If daemon unreachable (timeout 2s): read local blacklist cache, apply fail_mode
//!   5. Exit: 0 approve, 2 deny, 3 config error, 4 protocol mismatch

#![deny(unsafe_code)]

fn main() {
    // TODO: implement per spec §3.4, §7, §10
    std::process::exit(3);
}
