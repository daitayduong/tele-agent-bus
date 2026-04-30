use agent_bus_core::redact::redact_secrets;
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct TranscriptMessage {
    pub role: String,
    pub text: String,
    pub ts: Option<String>,
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct RenderStats {
    pub bytes: usize,
    pub messages_used: usize,
    pub trimmed: usize,
    #[allow(dead_code)]
    pub truncated_single: bool,
}

pub fn read_claude_jsonl(path: &Path) -> Result<Vec<TranscriptMessage>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut msgs = Vec::new();

    for line_res in reader.lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("failed to read line from {}: {}", path.display(), e);
                continue;
            }
        };

        let stripped = line.trim_start_matches(|c: char| c == '\0' || c.is_whitespace());
        if stripped.is_empty() {
            continue;
        }

        let v: Value = match serde_json::from_str(stripped) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to parse jsonl line from {}: {}", path.display(), e);
                continue;
            }
        };

        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let role = v
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            .unwrap_or(msg_type);

        let mut text_parts = Vec::new();
        if let Some(content) = v.get("message").and_then(|m| m.get("content")) {
            if let Some(s) = content.as_str() {
                text_parts.push(s.to_string());
            } else if let Some(arr) = content.as_array() {
                for block in arr {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                            text_parts.push(t.to_string());
                        }
                    } else if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                            text_parts.push(format!("[tool_use: {}]", name));
                        }
                    }
                }
            }
        } else if let Some(content) = v.get("content") {
            if let Some(s) = content.as_str() {
                text_parts.push(s.to_string());
            }
        }

        if text_parts.is_empty() {
            continue;
        }

        msgs.push(TranscriptMessage {
            role: role.to_string(),
            text: text_parts.join("\n\n"),
            ts: v
                .get("timestamp")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string()),
        });
    }

    Ok(msgs)
}

pub fn render_context(
    msgs: &[TranscriptMessage],
    max_bytes: usize,
    max_messages: usize,
    include_tool_use: bool,
) -> (String, RenderStats) {
    let mut messages: Vec<&TranscriptMessage> = msgs.iter().collect();
    if !include_tool_use {
        messages.retain(|m| m.role != "tool_use" && m.role != "tool_result");
    }

    let messages_used = messages.len().min(max_messages);
    let mut trimmed = messages.len().saturating_sub(messages_used);
    let mut candidates = &messages[trimmed..];

    loop {
        let (rendered, stats) = do_render(candidates, trimmed, max_bytes);
        if stats.bytes <= max_bytes || candidates.is_empty() {
            return (rendered, stats);
        }

        candidates = &candidates[1..];
        trimmed += 1;
    }
}

fn do_render(
    msgs: &[&TranscriptMessage],
    trimmed: usize,
    max_bytes: usize,
) -> (String, RenderStats) {
    let mut truncated_single = false;
    let mut body_parts = Vec::new();

    for m in msgs {
        let mut text = redact_secrets(&m.text);
        if text.len() > max_bytes / 2 {
            text = truncate_middle(&text, max_bytes / 2);
            truncated_single = true;
        }

        let header = if let Some(ts) = &m.ts {
            format!("## {} ({})", m.role, ts)
        } else {
            format!("## {}", m.role)
        };
        body_parts.push(format!("{}\n{}", header, text));
    }

    let body = body_parts.join("\n\n");
    let bytes_val = body.len();
    let kb = bytes_val as f64 / 1024.0;

    let mut info = vec![
        format!("recent {} messages", msgs.len()),
        format!("{:.1}KB", kb),
    ];
    if trimmed > 0 {
        info.push(format!("trimmed={}", trimmed));
    }
    if truncated_single {
        info.push("truncated=true".to_string());
    }

    let rendered = format!(
        "<mobile_session_context>\n# Mobile session transcript ({})\n\n{}\n</mobile_session_context>",
        info.join(", "),
        body
    );
    let final_bytes = rendered.len();

    (
        rendered,
        RenderStats {
            bytes: final_bytes,
            messages_used: msgs.len(),
            trimmed,
            truncated_single,
        },
    )
}

fn truncate_middle(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let marker_template = "[…truncated 000000 bytes…]";
    if limit <= marker_template.len() + 10 {
        return text.chars().take(limit.min(text.len())).collect();
    }

    let budget = limit - marker_template.len();
    let head_limit = budget / 2;
    let tail_limit = budget - head_limit;

    let mut head_end = 0;
    for (idx, _) in text.char_indices() {
        if idx > head_limit {
            break;
        }
        head_end = idx;
    }
    let head = &text[..head_end];

    let mut tail_start = text.len();
    for (idx, _) in text.char_indices().rev() {
        if text.len() - idx > tail_limit {
            break;
        }
        tail_start = idx;
    }
    let tail = &text[tail_start..];

    let removed = text.len() - head.len() - tail.len();
    format!("{}[…truncated {} bytes…]{}", head, removed, tail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_read_claude_jsonl_basic() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"type":"user","timestamp":"2026-04-18T14:10:00Z","message":{{"role":"user","content":"hello"}}}}"#).unwrap();
        writeln!(file, r#"{{"type":"assistant","timestamp":"2026-04-18T14:10:05Z","message":{{"role":"assistant","content":"hi"}}}}"#).unwrap();

        let msgs = read_claude_jsonl(file.path()).unwrap();
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn test_render_context_ac_l5() {
        let msgs = vec![
            TranscriptMessage {
                role: "user".into(),
                text: "hello".into(),
                ts: Some("2026-04-18T14:10:00Z".into()),
            },
            TranscriptMessage {
                role: "assistant".into(),
                text: "hi".into(),
                ts: None,
            },
        ];
        let (rendered, stats) = render_context(&msgs, 1000, 40, false);
        assert!(rendered.contains("<mobile_session_context>"));
        assert_eq!(stats.messages_used, 2);
    }
}
