#![deny(unsafe_code)]

use std::env;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg_attr(not(unix), allow(dead_code))]
const PROTOCOL_VERSION: u32 = 1;
const EXIT_APPROVE: i32 = 0;
const EXIT_DENY: i32 = 2;
const EXIT_CONFIG: i32 = 3;
const EXIT_PROTOCOL: i32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Approve,
    Deny,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let code = match run().await {
        Ok(Verdict::Approve) => EXIT_APPROVE,
        Ok(Verdict::Deny) => EXIT_DENY,
        Err(HookError::Protocol(err)) => {
            eprintln!("update agent-bus-hook: {err}");
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
    let input = read_input()?;
    let command = build_command(&input)?;

    let socket = socket_path()?;
    if !socket.exists() {
        return Ok(local_fallback(&command, "socket missing"));
    }

    let mut last_err = "connect failed".to_string();
    for _ in 0..3 {
        match ask_daemon(&socket, &input, &command).await {
            Ok(verdict) => return Ok(verdict),
            Err(HookError::Protocol(err)) => return Err(HookError::Protocol(err)),
            Err(err) => {
                last_err = err.to_string();
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    Ok(local_fallback(&command, &last_err))
}

fn build_command(input: &Value) -> Result<String, HookError> {
    let tool = input
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("Bash");

    match tool {
        "Write" => {
            let file_path = input
                .pointer("/tool_input/file_path")
                .and_then(Value::as_str)
                .ok_or_else(|| HookError::Config("missing tool_input.file_path".to_string()))?;
            let content = input
                .pointer("/tool_input/content")
                .and_then(Value::as_str)
                .unwrap_or("");
            Ok(format!(
                "Write {file_path} ({bytes} bytes)\n──── content ────\n{preview}\n──── end ────",
                bytes = content.len(),
                preview = preview_text(content, 600),
            ))
        }
        "Edit" => {
            let file_path = input
                .pointer("/tool_input/file_path")
                .and_then(Value::as_str)
                .ok_or_else(|| HookError::Config("missing tool_input.file_path".to_string()))?;
            let old_str = input
                .pointer("/tool_input/old_string")
                .and_then(Value::as_str)
                .unwrap_or("");
            let new_str = input
                .pointer("/tool_input/new_string")
                .and_then(Value::as_str)
                .unwrap_or("");
            let replace_all = input
                .pointer("/tool_input/replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let scope = if replace_all { " (replace_all)" } else { "" };
            Ok(format!(
                "Edit {file_path}{scope} (-{old_b} +{new_b} bytes)\n──── old ────\n{old_p}\n──── new ────\n{new_p}\n──── end ────",
                old_b = old_str.len(),
                new_b = new_str.len(),
                old_p = preview_text(old_str, 300),
                new_p = preview_text(new_str, 300),
            ))
        }
        "MultiEdit" => {
            let file_path = input
                .pointer("/tool_input/file_path")
                .and_then(Value::as_str)
                .ok_or_else(|| HookError::Config("missing tool_input.file_path".to_string()))?;
            let edits = input
                .pointer("/tool_input/edits")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let mut body = format!("MultiEdit {file_path} ({} edits)", edits.len());
            for (i, edit) in edits.iter().enumerate() {
                let old_str = edit
                    .get("old_string")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let new_str = edit
                    .get("new_string")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                body.push_str(&format!(
                    "\n──── edit {idx}: -{old_b} +{new_b} bytes ────\nOLD:\n{old_p}\nNEW:\n{new_p}",
                    idx = i + 1,
                    old_b = old_str.len(),
                    new_b = new_str.len(),
                    old_p = preview_text(old_str, 200),
                    new_p = preview_text(new_str, 200),
                ));
            }
            body.push_str("\n──── end ────");
            Ok(body)
        }
        _ => input
            .pointer("/tool_input/command")
            .or_else(|| input.pointer("/tool_input/file_path"))
            .or_else(|| input.get("command"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| HookError::Config("missing tool_input.command".to_string())),
    }
}

fn preview_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(max_chars).collect();
        format!("{truncated}…(truncated, {} bytes total)", text.len())
    }
}

fn read_input() -> Result<Value, HookError> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    if raw.trim().is_empty() {
        return Err(HookError::Config("empty stdin".to_string()));
    }
    Ok(serde_json::from_str(&raw)?)
}

async fn ask_daemon(socket: &PathBuf, input: &Value, command: &str) -> Result<Verdict, HookError> {
    #[cfg(not(unix))]
    {
        let _ = (socket, input, command);
        return Err(HookError::Daemon(
            "daemon IPC is not available on this platform yet".to_string(),
        ));
    }
    #[cfg(unix)]
    {
        let mut stream = UnixStream::connect(socket).await?;
        let body = build_request_body(input, command);
        let bytes = serde_json::to_vec(&body)?;
        let headers = format!(
        "POST /perm/check HTTP/1.1\r\nHost: agent-bus\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    );
        stream.write_all(headers.as_bytes()).await?;
        stream.write_all(&bytes).await?;
        stream.shutdown().await?;

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        parse_response(&response)
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
fn build_request_body(input: &Value, command: &str) -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "request_id": format!("hook-{}-{}", std::process::id(), monotonic_nanos()),
        "session_id": input.get("session_id").and_then(Value::as_str).unwrap_or("unknown"),
        "tool": input.get("tool_name").and_then(Value::as_str).unwrap_or("Bash"),
        "command": command,
        "repo_id": input.get("cwd").and_then(Value::as_str).and_then(repo_hint),
        "timeout_ms": 300000
    })
}

#[cfg_attr(not(unix), allow(dead_code))]
fn parse_response(response: &[u8]) -> Result<Verdict, HookError> {
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| HookError::Protocol("malformed HTTP response".to_string()))?;
    let headers = String::from_utf8_lossy(&response[..split]);
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| HookError::Protocol("missing HTTP status".to_string()))?;
    if status == 426 {
        return Err(HookError::Protocol("daemon returned 426".to_string()));
    }
    if status >= 400 {
        return Err(HookError::Daemon(format!("HTTP {status}")));
    }

    let body: Value = serde_json::from_slice(&response[split + 4..])?;
    let version = body
        .get("protocol_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| HookError::Protocol("missing protocol_version".to_string()))?;
    if version != PROTOCOL_VERSION as u64 {
        return Err(HookError::Protocol(format!(
            "expected protocol {PROTOCOL_VERSION}, got {version}"
        )));
    }
    match body.get("verdict").and_then(Value::as_str) {
        Some("approve") => Ok(Verdict::Approve),
        Some("deny") => {
            if let Some(reason) = body.get("reason").and_then(Value::as_str) {
                eprintln!("{reason}");
            }
            Ok(Verdict::Deny)
        }
        other => Err(HookError::Protocol(format!("unknown verdict {other:?}"))),
    }
}

fn local_fallback(command: &str, reason: &str) -> Verdict {
    let deny = is_destructive(command);
    let verdict = if deny { "deny" } else { "approve" };
    eprintln!(
        "{{\"event\":\"hook_local_fallback\",\"verdict\":\"{verdict}\",\"reason\":\"{reason}\"}}"
    );
    if deny {
        Verdict::Deny
    } else {
        Verdict::Approve
    }
}

fn is_destructive(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    [
        "rm -rf",
        "git push -f",
        "git push --force",
        "drop table",
        "truncate table",
        "delete from",
        "mkfs",
        ":(){",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn socket_path() -> Result<PathBuf, HookError> {
    if let Ok(path) = env::var("AGENT_BUS_SOCKET") {
        return Ok(PathBuf::from(path));
    }
    if let Ok(path) = env::var("AGENT_BUS_HOME") {
        return Ok(PathBuf::from(path).join("daemon.sock"));
    }
    let home = env::var("HOME")
        .or_else(|_| env::var("USERPROFILE"))
        .map_err(|_| HookError::Config("HOME or USERPROFILE is not set".to_string()))?;
    Ok(PathBuf::from(home).join(".agent-bus/daemon.sock"))
}

#[cfg_attr(not(unix), allow(dead_code))]
fn repo_hint(cwd: &str) -> Option<String> {
    let path = std::path::Path::new(cwd);
    let display = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(slugify)?;
    let canonical = path.canonicalize().ok()?;
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let hash = hex::encode(digest);
    Some(format!("{display}_{}", &hash[..8]))
}

#[cfg_attr(not(unix), allow(dead_code))]
fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;

    for byte in input.bytes() {
        let ch = byte.to_ascii_lowercase() as char;
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }

    while out.ends_with('-') {
        out.pop();
    }

    if out.is_empty() {
        "repo".to_string()
    } else {
        out
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
fn monotonic_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[derive(Debug)]
#[cfg_attr(not(unix), allow(dead_code))]
enum HookError {
    Config(String),
    Protocol(String),
    Daemon(String),
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for HookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(err) => write!(f, "config error: {err}"),
            Self::Protocol(err) => write!(f, "protocol error: {err}"),
            Self::Daemon(err) => write!(f, "daemon unavailable: {err}"),
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::Json(err) => write!(f, "json error: {err}"),
        }
    }
}

impl From<std::io::Error> for HookError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for HookError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_denies_cached_destructive_commands() {
        assert_eq!(local_fallback("rm -rf /tmp/x", "test"), Verdict::Deny);
        assert_eq!(
            local_fallback("git push -f origin main", "test"),
            Verdict::Deny
        );
        assert_eq!(local_fallback("DROP TABLE users", "test"), Verdict::Deny);
    }

    #[test]
    fn fallback_approves_non_destructive_commands() {
        assert_eq!(local_fallback("ls /tmp", "test"), Verdict::Approve);
    }

    #[test]
    fn parses_verdict_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 43\r\n\r\n{\"protocol_version\":1,\"verdict\":\"deny\"}";
        assert_eq!(parse_response(raw).unwrap(), Verdict::Deny);
    }

    #[test]
    fn request_body_matches_daemon_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("SampleRepo");
        std::fs::create_dir(&repo).unwrap();
        let input = json!({
            "session_id": "sess-1",
            "tool_name": "Bash",
            "cwd": repo,
            "tool_input": {"command": "ls"}
        });

        let body = build_request_body(&input, "ls");

        assert_eq!(body["protocol_version"], 1);
        assert_eq!(body["session_id"], "sess-1");
        assert_eq!(body["tool"], "Bash");
        assert_eq!(body["command"], "ls");
        assert!(body["repo_id"].as_str().unwrap().starts_with("samplerepo_"));
    }
}
