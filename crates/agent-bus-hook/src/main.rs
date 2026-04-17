//! PreToolUse hook binary. Spawned per Bash tool invocation by Claude Code.
//!
//! Flow (per spec §3.4, §7, §10):
//!   1. Read hook-call JSON from stdin
//!   2. Connect to `~/.agent-bus/daemon.sock`
//!   3. POST /perm/check with PROTOCOL_VERSION=1
//!   4. If daemon unreachable (timeout 2s): read local blacklist cache, apply fail_mode
//!   5. Exit: 0 approve, 2 deny, 3 config error, 4 protocol mismatch

#![deny(unsafe_code)]

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Approve,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailMode {
    Approve,
    Deny,
    Hybrid,
}

fn local_fallback_verdict(
    _command: &str,
    _blacklist_cache: &Path,
    _fail_mode: FailMode,
) -> Result<Verdict, HookError> {
    todo!("RED: implemented after tests")
}

#[derive(Debug, thiserror::Error)]
enum HookError {
    #[error("config error: {0}")]
    Config(String),
    #[error("protocol mismatch: {0}")]
    Protocol(String),
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // TODO: implement per spec §3.4, §7, §10
    std::process::exit(3);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hybrid_fallback_denies_destructive_cached_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("blacklist.txt");
        std::fs::write(&cache, "git reset --hard\tdestructive\nrm -rf\tdestructive\n").unwrap();

        let verdict =
            local_fallback_verdict("git reset --hard", &cache, FailMode::Hybrid).unwrap();

        assert_eq!(verdict, Verdict::Deny);
    }

    #[test]
    fn hybrid_fallback_approves_non_destructive_command() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("blacklist.txt");
        std::fs::write(&cache, "git reset --hard\tdestructive\n").unwrap();

        let verdict = local_fallback_verdict("ls /tmp", &cache, FailMode::Hybrid).unwrap();

        assert_eq!(verdict, Verdict::Approve);
    }

    #[test]
    fn fallback_detects_suspicious_shell_without_cache_match() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("blacklist.txt");
        std::fs::write(&cache, "").unwrap();

        let verdict = local_fallback_verdict(
            "echo cm0gLXJmIC8= | base64 -d | sh",
            &cache,
            FailMode::Hybrid,
        )
        .unwrap();

        assert_eq!(verdict, Verdict::Deny);
    }
}
