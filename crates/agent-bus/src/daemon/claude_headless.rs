//! Phase 3.3 — spawn `claude --resume <uuid>` in headless mode, pipe prompt
//! via stdin, capture stdout.
//!
//! Behavior:
//! - Timeout after `timeout_secs` (default 600s, override via `AGENT_BUS_CLAUDE_TIMEOUT_SECS`).
//! - Returns stdout on success.
//! - Returns error with tail of stderr on failure.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

pub const DEFAULT_CLAUDE_TIMEOUT_SECS: u64 = 600;

/// Resolve effective timeout: env `AGENT_BUS_CLAUDE_TIMEOUT_SECS` if set and
/// parseable, else `DEFAULT_CLAUDE_TIMEOUT_SECS`.
pub fn resolved_timeout_secs() -> u64 {
    std::env::var("AGENT_BUS_CLAUDE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_CLAUDE_TIMEOUT_SECS)
}

/// Spawn `claude --resume <uuid> --print --output-format text` from `cwd`,
/// pipe `prompt` via stdin, return stdout.
pub async fn spawn_claude_resume(
    claude_bin: &str,
    cwd: &Path,
    mobile_uuid: &str,
    prompt: &str,
    timeout_secs: u64,
) -> Result<String> {
    let mut child = Command::new(claude_bin)
        .args([
            "--resume",
            mobile_uuid,
            "--print",
            "--output-format",
            "text",
        ])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {}", claude_bin))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("write prompt to claude stdin")?;
        stdin.shutdown().await.ok();
    }

    let dur = Duration::from_secs(timeout_secs);
    let output = match timeout(dur, child.wait_with_output()).await {
        Ok(res) => res.context("claude process io error")?,
        Err(_) => {
            return Err(anyhow!("claude headless timeout after {}s", timeout_secs));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr.chars().rev().take(400).collect::<String>();
        let tail: String = tail.chars().rev().collect();
        return Err(anyhow!(
            "claude exited {}: {}",
            output.status.code().unwrap_or(-1),
            tail.trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok(stdout)
}

/// Split a long message into chunks at most `max_bytes` for Telegram's 4096 limit.
/// Splits on newline boundaries when possible.
pub fn chunk_for_telegram(text: &str, max_bytes: usize) -> Vec<String> {
    if text.len() <= max_bytes {
        return vec![text.to_string()];
    }

    let mut out = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if current.len() + line.len() > max_bytes && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        if line.len() > max_bytes {
            // hard-split oversize single line on char boundaries
            let mut remaining = line;
            while remaining.len() > max_bytes {
                let split_at = remaining
                    .char_indices()
                    .take_while(|(i, c)| i + c.len_utf8() <= max_bytes)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                if split_at == 0 {
                    break; // single char exceeds max_bytes — give up (caller's fault)
                }
                out.push(remaining[..split_at].to_string());
                remaining = &remaining[split_at..];
            }
            if !remaining.is_empty() {
                current.push_str(remaining);
            }
        } else {
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_short_returns_single() {
        let chunks = chunk_for_telegram("hello", 4096);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn chunk_splits_at_newline_boundary() {
        let text = "line1\nline2\nline3\n";
        let chunks = chunk_for_telegram(text, 12);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(c.len() <= 12, "chunk {:?} exceeds limit", c);
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn chunk_hard_splits_single_oversize_line() {
        let text = "x".repeat(100);
        let chunks = chunk_for_telegram(&text, 40);
        assert!(chunks.len() >= 3);
        for c in &chunks {
            assert!(c.len() <= 40);
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn chunk_preserves_unicode_boundaries() {
        let text = "café☕☕☕☕☕☕☕☕☕☕☕☕";
        let chunks = chunk_for_telegram(text, 10);
        for c in &chunks {
            assert!(c.len() <= 10);
            // valid utf8 by construction
            assert_eq!(c, c);
        }
        assert_eq!(chunks.concat(), text);
    }
}
