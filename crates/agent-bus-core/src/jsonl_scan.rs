//! Phase 4a.11 - secondary classifier for Claude JSONL transcripts.
//!
//! When `ResultKind::UnknownFailure` occurs for a `claude` agent in
//! `ClaudeResume` or `WithMobileContext` mode, we tail the session JSONL
//! and re-apply classifier patterns against the text content of each
//! message. Matches upgrade the result kind with a `jsonl_tail:<pattern_name>`
//! classifier tag.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use crate::classifier::{classify_text, ResultKind};

pub const TAIL_CAP_BYTES: usize = 64 * 1024;
pub const MAX_LINES: usize = 50;

/// Tail the JSONL file and return the concatenated rendered text of the
/// last up-to-`MAX_LINES` messages (best-effort).
///
/// Malformed lines are skipped. Returns empty string if the file is
/// missing or unreadable.
pub fn tail_text(path: &Path) -> String {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return String::new(),
    };
    let len = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => return String::new(),
    };
    if len == 0 {
        return String::new();
    }

    let tail_len = len.min(TAIL_CAP_BYTES as u64);
    let seeked_into_file = tail_len < len;
    let mut bytes = Vec::with_capacity(tail_len as usize);
    if file.seek(SeekFrom::End(-(tail_len as i64))).is_err() {
        return String::new();
    }
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    render_tail(&bytes, seeked_into_file)
}

/// Re-classify Claude output by scanning the JSONL tail.
/// Returns `Some((kind, "jsonl_tail:<pattern_name>"))` on match, `None` otherwise.
pub fn scan_and_classify(path: &Path) -> Option<(ResultKind, String)> {
    let text = tail_text(path);
    if text.is_empty() {
        return None;
    }
    let (kind, matched) = classify_text("claude", &text)?;
    Some((kind, format!("jsonl_tail:{matched}")))
}

fn render_tail(bytes: &[u8], discard_first_line: bool) -> String {
    let tail = String::from_utf8_lossy(bytes);
    let mut lines: Vec<&str> = tail.split('\n').collect();
    if discard_first_line && !lines.is_empty() {
        lines.remove(0);
    }
    if lines.last() == Some(&"") {
        lines.pop();
    }

    let start = lines.len().saturating_sub(MAX_LINES);
    let mut out = String::new();
    for line in &lines[start..] {
        if let Some(text) = extract_text_line(line) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&text);
            truncate_to_cap(&mut out);
            if out.len() >= TAIL_CAP_BYTES {
                break;
            }
        }
    }
    out
}

fn extract_text_line(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    let content = value.get("content").or_else(|| {
        value
            .get("message")
            .and_then(|message| message.get("content"))
    })?;
    extract_content_text(content)
}

fn extract_content_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }

    let blocks = content.as_array()?;
    let text = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn truncate_to_cap(s: &mut String) {
    if s.len() <= TAIL_CAP_BYTES {
        return;
    }
    let mut end = TAIL_CAP_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn missing_file_returns_none() {
        let path = std::env::temp_dir().join("agent-bus-missing-jsonl-scan-test.jsonl");
        let _ = std::fs::remove_file(&path);
        assert_eq!(scan_and_classify(&path), None);
    }

    #[test]
    fn empty_file_returns_none() {
        let file = NamedTempFile::new().unwrap();
        assert_eq!(scan_and_classify(file.path()), None);
    }

    #[test]
    fn quota_phrase_in_assistant_text_matches() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"role":"user","content":"hello"}}"#).unwrap();
        writeln!(
            file,
            r#"{{"role":"assistant","content":[{{"type":"text","text":"working"}}]}}"#
        )
        .unwrap();
        writeln!(file, r#"{{"role":"assistant","content":[{{"type":"text","text":"Claude usage limit reached"}}]}}"#).unwrap();

        let (kind, classifier) = scan_and_classify(file.path()).unwrap();
        assert_eq!(kind, ResultKind::QuotaExhausted);
        assert!(classifier.starts_with("jsonl_tail:"));
    }

    #[test]
    fn hit_limit_phrase_in_synthetic_assistant_message_matches() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"You've hit your limit · resets 2pm (America/Regina)"}}]}}}}"#
        )
        .unwrap();

        let (kind, classifier) = scan_and_classify(file.path()).unwrap();
        assert_eq!(kind, ResultKind::QuotaExhausted);
        assert_eq!(classifier, "jsonl_tail:claude_hit_limit");
    }

    #[test]
    fn auth_phrase_in_content_matches() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"role":"assistant","content":"not logged in"}}"#).unwrap();

        let (kind, classifier) = scan_and_classify(file.path()).unwrap();
        assert_eq!(kind, ResultKind::AuthExpired);
        assert_eq!(classifier, "jsonl_tail:claude_not_logged_in");
    }

    #[test]
    fn malformed_lines_skipped_valid_still_matches() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "not-json").unwrap();
        writeln!(file, r#"{{"role":"assistant","content":"please log in"}}"#).unwrap();

        let (kind, classifier) = scan_and_classify(file.path()).unwrap();
        assert_eq!(kind, ResultKind::AuthExpired);
        assert_eq!(classifier, "jsonl_tail:claude_please_log_in");
    }

    #[test]
    fn tool_use_blocks_ignored() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"role":"assistant","content":[{{"type":"tool_use","input":{{"cmd":"usage limit"}}}}]}}"#
        )
        .unwrap();

        assert_eq!(scan_and_classify(file.path()), None);
    }

    #[test]
    fn tail_cap_respected_for_large_file() {
        let mut file = NamedTempFile::new().unwrap();
        for _ in 0..80 {
            writeln!(
                file,
                r#"{{"role":"assistant","content":"{}"}}"#,
                "all clear ".repeat(200)
            )
            .unwrap();
        }
        writeln!(
            file,
            r#"{{"role":"assistant","content":"Claude usage limit reached"}}"#
        )
        .unwrap();

        let (kind, classifier) = scan_and_classify(file.path()).unwrap();
        assert_eq!(kind, ResultKind::QuotaExhausted);
        assert!(classifier.starts_with("jsonl_tail:"));
    }
}
