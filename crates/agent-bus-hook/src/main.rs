//! PreToolUse hook binary. Spawned per Bash tool invocation by Claude Code.
//!
//! Flow (per spec §3.4, §7, §10):
//!   1. Read hook-call JSON from stdin
//!   2. Connect to `~/.agent-bus/daemon.sock`
//!   3. POST /perm/check with PROTOCOL_VERSION=1
//!   4. If daemon unreachable (timeout 2s): read local blacklist cache, apply fail_mode
//!   5. Exit: 0 approve, 2 deny, 3 config error, 4 protocol mismatch

#![deny(unsafe_code)]

use std::env;
use std::io::Read;
use std::path::{Path, PathBuf};

use agent_bus_proto::PROTOCOL_VERSION;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::{timeout, Duration};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);
const EXIT_APPROVE: i32 = 0;
const EXIT_DENY: i32 = 2;
const EXIT_CONFIG: i32 = 3;
const EXIT_PROTOCOL: i32 = 4;

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
    command: &str,
    blacklist_cache: &Path,
    fail_mode: FailMode,
) -> Result<Verdict, HookError> {
    Ok(match fail_mode {
        FailMode::Approve => Verdict::Approve,
        FailMode::Deny => Verdict::Deny,
        FailMode::Hybrid => {
            if is_destructive(command, blacklist_cache)? {
                Verdict::Deny
            } else {
                Verdict::Approve
            }
        }
    })
}

#[derive(Debug, thiserror::Error)]
enum HookError {
    #[error("config error: {0}")]
    Config(String),
    #[error("protocol mismatch: {0}")]
    Protocol(String),
    #[error("daemon unavailable: {0}")]
    DaemonUnavailable(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Deserialize)]
struct HookInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    tool_input: Option<ToolInput>,
}

#[derive(Debug, Deserialize)]
struct ToolInput {
    #[serde(default)]
    command: Option<String>,
}

#[derive(Debug, Serialize)]
struct PermCheckRequest {
    protocol_version: u32,
    request_id: String,
    session_id: String,
    tool: String,
    command: String,
    repo_hint: Option<String>,
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
struct PermCheckResponse {
    protocol_version: u32,
    verdict: String,
    #[serde(default)]
    reason: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let code = match run().await {
        Ok(Verdict::Approve) => EXIT_APPROVE,
        Ok(Verdict::Deny) => EXIT_DENY,
        Err(HookError::Protocol(err)) => {
            eprintln!("update agent-bus-hook to match daemon: {err}");
            EXIT_PROTOCOL
        }
        Err(err) => {
            eprintln!("{err}");
            EXIT_CONFIG
        }
    };

    std::process::exit(code);
}

async fn run() -> Result<Verdict, HookError> {
    let input = read_hook_input()?;
    let command = input
        .command
        .as_ref()
        .or_else(|| {
            input
                .tool_input
                .as_ref()
                .and_then(|tool_input| tool_input.command.as_ref())
        })
        .cloned()
        .ok_or_else(|| HookError::Config("missing Bash command in hook input".to_string()))?;

    match timeout(DEFAULT_TIMEOUT, ask_daemon(&input, &command)).await {
        Ok(Ok(verdict)) => Ok(verdict),
        Ok(Err(HookError::Protocol(err))) => Err(HookError::Protocol(err)),
        Ok(Err(err)) => degraded_verdict(&command, err),
        Err(_) => degraded_verdict(
            &command,
            HookError::DaemonUnavailable("timeout after 2s".to_string()),
        ),
    }
}

fn degraded_verdict(command: &str, err: HookError) -> Result<Verdict, HookError> {
    let cache = blacklist_cache_path()?;
    let fail_mode = fail_mode();
    let verdict = local_fallback_verdict(command, &cache, fail_mode)?;
    match verdict {
        Verdict::Approve => {
            eprintln!(
                r#"{{"event":"daemon_unreachable","source":"hook","verdict":"approve_degraded","reason":"{err}"}}"#
            );
        }
        Verdict::Deny => {
            eprintln!("daemon down; destructive command blocked by fail-closed policy");
            eprintln!(
                r#"{{"event":"daemon_unreachable","source":"hook","verdict":"deny_destructive","reason":"{err}"}}"#
            );
        }
    }
    Ok(verdict)
}

fn read_hook_input() -> Result<HookInput, HookError> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    if input.trim().is_empty() {
        return Err(HookError::Config("empty hook input".to_string()));
    }

    Ok(serde_json::from_str(&input)?)
}

async fn ask_daemon(input: &HookInput, command: &str) -> Result<Verdict, HookError> {
    let socket = socket_path()?;
    let mut stream = UnixStream::connect(&socket)
        .await
        .map_err(|err| HookError::DaemonUnavailable(err.to_string()))?;
    let request_id = format!("req-client-{}", std::process::id());
    let body = serde_json::to_vec(&PermCheckRequest {
        protocol_version: PROTOCOL_VERSION,
        request_id,
        session_id: input
            .session_id
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        tool: "Bash".to_string(),
        command: command.to_string(),
        repo_hint: input.cwd.as_deref().and_then(repo_hint_from_cwd),
        timeout_ms: 12_000,
    })?;
    let http = format!(
        "POST /perm/check HTTP/1.1\r\nHost: agent-bus\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );

    stream.write_all(http.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.shutdown().await?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    parse_http_response(&response)
}

fn parse_http_response(response: &[u8]) -> Result<Verdict, HookError> {
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| HookError::Protocol("malformed HTTP response".to_string()))?;
    let (headers, body) = response.split_at(split + 4);
    let header_text = String::from_utf8_lossy(headers);
    let status = header_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| HookError::Protocol("missing HTTP status".to_string()))?;

    if status == 426 {
        return Err(HookError::Protocol("daemon returned 426".to_string()));
    }
    if status >= 400 {
        return Err(HookError::DaemonUnavailable(format!("HTTP {status}")));
    }

    let decoded: PermCheckResponse = serde_json::from_slice(body)?;
    if decoded.protocol_version != PROTOCOL_VERSION {
        return Err(HookError::Protocol(format!(
            "expected protocol {}, got {}",
            PROTOCOL_VERSION, decoded.protocol_version
        )));
    }

    match decoded.verdict.as_str() {
        "approve" => Ok(Verdict::Approve),
        "deny" => {
            if let Some(reason) = decoded.reason {
                eprintln!("{reason}");
            }
            Ok(Verdict::Deny)
        }
        other => Err(HookError::Protocol(format!("unknown verdict {other}"))),
    }
}

fn socket_path() -> Result<PathBuf, HookError> {
    if let Ok(path) = env::var("AGENT_BUS_SOCKET") {
        return Ok(PathBuf::from(path));
    }
    Ok(agent_bus_home()?.join("daemon.sock"))
}

fn blacklist_cache_path() -> Result<PathBuf, HookError> {
    if let Ok(path) = env::var("AGENT_BUS_BLACKLIST_CACHE") {
        return Ok(PathBuf::from(path));
    }
    Ok(agent_bus_home()?.join("blacklist.txt"))
}

fn agent_bus_home() -> Result<PathBuf, HookError> {
    if let Ok(path) = env::var("AGENT_BUS_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var("HOME").map_err(|_| HookError::Config("HOME is not set".to_string()))?;
    Ok(PathBuf::from(home).join(".agent-bus"))
}

fn fail_mode() -> FailMode {
    match env::var("AGENT_BUS_FAIL_MODE")
        .unwrap_or_else(|_| "hybrid".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "approve" => FailMode::Approve,
        "deny" => FailMode::Deny,
        _ => FailMode::Hybrid,
    }
}

fn repo_hint_from_cwd(cwd: &str) -> Option<String> {
    Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_ascii_lowercase())
}

fn is_destructive(command: &str, blacklist_cache: &Path) -> Result<bool, HookError> {
    if is_suspicious(command) {
        return Ok(true);
    }

    let Ok(cache) = std::fs::read_to_string(blacklist_cache) else {
        return Ok(false);
    };

    for line in cache.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (pattern, flags) = line.split_once('\t').unwrap_or((line, ""));
        if flags.contains("destructive") && pattern_matches(command, pattern) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn is_suspicious(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains("base64")
        || lower.contains("eval")
        || lower.contains("$(")
        || lower.contains('`')
        || lower.contains("|sh")
        || lower.contains("| sh")
        || lower.contains("|bash")
        || lower.contains("| bash")
        || lower.contains("|python")
        || lower.contains("| python")
        || lower.contains(" exec")
}

fn pattern_matches(command: &str, pattern: &str) -> bool {
    if command.contains(pattern) {
        return true;
    }

    let normalized = normalize_pattern(pattern);
    !normalized.is_empty() && command.contains(&normalized)
}

fn normalize_pattern(pattern: &str) -> String {
    let mut out = pattern
        .replace("(^|\\s)", "")
        .replace("\\s+", " ")
        .replace("\\s*", " ")
        .replace("\\s", " ")
        .replace("\\-", "-")
        .replace("\\/", "/")
        .replace("\\.", ".")
        .replace(['^', '$', '(', ')'], "");
    while out.contains("  ") {
        out = out.replace("  ", " ");
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hybrid_fallback_denies_destructive_cached_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("blacklist.txt");
        std::fs::write(
            &cache,
            "git reset --hard\tdestructive\nrm -rf\tdestructive\n",
        )
        .unwrap();

        let verdict = local_fallback_verdict("git reset --hard", &cache, FailMode::Hybrid).unwrap();

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
