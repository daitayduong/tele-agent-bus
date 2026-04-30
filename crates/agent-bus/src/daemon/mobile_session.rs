//! Phase 3 — Mobile Claude session management.
//!
//! See docs/specs/phase3-mobile-claude-session.md.
//!
//! This module implements session forking, active-session detection, delta extraction,
//! and command parsing for the Telegram-controlled mobile Claude workflow.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::cmp::Reverse;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use regex::Regex;
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Fixed mobile session uuid reused across all forks. See spec §Data Model.
pub const MOBILE_UUID: &str = "00000000-0000-4000-8000-000000000001";

/// One active Claude desktop session discovered in \`~/.claude/projects/<hash>/\`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub uuid: String,
    pub cwd: String,
    pub ai_title: Option<String>,
    /// First real user prompt (IDE/tool wrappers stripped), used as card label fallback.
    pub first_prompt: Option<String>,
    pub last_modified: OffsetDateTime,
    pub turn_count: usize,
}

/// Outcome of a fork operation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct ForkStats {
    pub lines_rewritten: usize,
    pub archived_previous: bool,
}

/// Commands parsed from Telegram text messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileCommand {
    ListClaude,
    FlushMobile,
    ClaudeMsg(String),
}

/// Rewrite the \`sessionId\` field in a single JSONL line to \`new_uuid\`.
///
/// - Lines that don't contain \`sessionId\` are returned unchanged.
/// - Other fields (cwd, gitBranch, uuid, parentUuid, promptId) are preserved.
/// - Returns error if the line is not valid JSON.
/// - Strips leading NUL/whitespace padding that appears in torn concurrent writes.
#[cfg_attr(not(test), allow(dead_code))]
pub fn rewrite_session_id(line: &str, new_uuid: &str) -> Result<String> {
    let stripped = line.trim_start_matches(|c: char| c == '\0' || c.is_whitespace());
    if stripped.is_empty() {
        return Ok(line.to_string());
    }
    let preview: String = stripped.chars().take(120).collect();
    let mut v: Value = serde_json::from_str(stripped)
        .with_context(|| format!("invalid JSON line (len={}): {}…", stripped.len(), preview))?;

    if let Some(obj) = v.as_object_mut() {
        if obj.contains_key("sessionId") {
            obj.insert("sessionId".to_string(), json!(new_uuid));
            return Ok(serde_json::to_string(&v)?);
        }
    }

    Ok(stripped.to_string())
}

/// Fork a source JSONL session into a target file, rewriting sessionId to \`new_uuid\`.
///
/// If \`target\` already exists, it MUST be archived. Returns stats for logging/Telegram reply.
#[cfg_attr(not(test), allow(dead_code))]
pub fn fork_session(source: &Path, target: &Path, new_uuid: &str) -> Result<ForkStats> {
    let mut archived_previous = false;
    if target.exists() {
        let mtime = fs::metadata(target)?.modified()?;
        let dt: OffsetDateTime = mtime.into();
        let ts = dt.format(&Rfc3339)?.replace(':', "-");
        let archive_path = target.with_extension(format!("archive-{}.jsonl", ts));
        fs::rename(target, &archive_path)?;
        archived_previous = true;
    }

    let src_file = File::open(source)?;
    let reader = BufReader::new(src_file);

    let tmp_path = target.with_file_name(format!(
        "{}.tmp.{}",
        target.file_name().unwrap().to_str().unwrap(),
        std::process::id()
    ));

    let mut lines_rewritten = 0;
    let mut lines_skipped = 0;
    {
        let mut dest_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;

        for line in reader.lines() {
            let line = line?;
            if line.contains("\"sessionId\"") {
                match rewrite_session_id(&line, new_uuid) {
                    Ok(rewritten) => {
                        writeln!(dest_file, "{}", rewritten)?;
                        lines_rewritten += 1;
                    }
                    Err(err) => {
                        tracing::warn!(
                            target: "agent_bus::mobile",
                            error = %err,
                            "skipping unparseable jsonl line during fork"
                        );
                        lines_skipped += 1;
                    }
                }
            } else {
                writeln!(dest_file, "{}", line)?;
            }
        }
        dest_file.sync_all()?;
    }
    if lines_skipped > 0 {
        tracing::warn!(
            target: "agent_bus::mobile",
            skipped = lines_skipped,
            "fork_session skipped corrupt lines"
        );
    }

    fs::rename(&tmp_path, target)?;

    if let Some(parent) = target.parent() {
        File::open(parent)?.sync_all()?;
    }

    Ok(ForkStats {
        lines_rewritten,
        archived_previous,
    })
}

/// Discover active desktop sessions in \`project_dir\`, excluding \`exclude_uuid\` (mobile).
pub fn detect_active_sessions(
    project_dir: &Path,
    exclude_uuid: &str,
    mtime_threshold_secs: u64,
) -> Result<Vec<SessionInfo>> {
    let mut active_uuids = std::collections::HashSet::new();

    // Scan /proc for running claude processes
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            if let Ok(name) = entry.file_name().into_string() {
                if name.chars().all(|c| c.is_ascii_digit()) {
                    let cmdline_path = entry.path().join("cmdline");
                    if let Ok(cmdline) = fs::read(cmdline_path) {
                        let args: Vec<String> = cmdline
                            .split(|&b| b == 0)
                            .map(|b| String::from_utf8_lossy(b).into_owned())
                            .filter(|s| !s.is_empty())
                            .collect();

                        for i in 0..args.len() {
                            if args[i] == "--resume" && i + 1 < args.len() {
                                active_uuids.insert(args[i + 1].clone());
                            }
                        }
                    }
                }
            }
        }
    }

    let mut sessions = Vec::new();
    let now = OffsetDateTime::now_utc();

    if let Ok(entries) = fs::read_dir(project_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if stem == exclude_uuid {
                    continue;
                }

                let metadata = fs::metadata(&path)?;
                let mtime: OffsetDateTime = metadata.modified()?.into();
                let is_fresh = (now - mtime).whole_seconds() < mtime_threshold_secs as i64;
                let is_active = active_uuids.contains(stem);

                if is_fresh || is_active {
                    let file = File::open(&path)?;
                    let reader = BufReader::new(file);

                    let mut cwd = String::new();
                    let mut ai_title = None;
                    let mut first_prompt: Option<String> = None;
                    let mut turn_count = 0;

                    for line in reader.lines().map_while(Result::ok) {
                        let stripped =
                            line.trim_start_matches(|c: char| c == '\0' || c.is_whitespace());
                        if let Ok(v) = serde_json::from_str::<Value>(stripped) {
                            let t = v["type"].as_str().unwrap_or("");
                            if t == "user" || t == "assistant" {
                                turn_count += 1;
                                if cwd.is_empty() {
                                    if let Some(c) = v["cwd"].as_str() {
                                        cwd = c.to_string();
                                    }
                                }
                            }
                            if t == "user" && first_prompt.is_none() {
                                if let Some(text) = extract_user_text(&v) {
                                    if let Some(cleaned) = clean_prompt_snippet(&text) {
                                        first_prompt = Some(cleaned);
                                    }
                                }
                            }
                            if t == "ai-title" {
                                if let Some(title) =
                                    v["aiTitle"].as_str().or_else(|| v["title"].as_str())
                                {
                                    ai_title = Some(title.to_string());
                                }
                            }
                        }
                    }

                    sessions.push(SessionInfo {
                        uuid: stem.to_string(),
                        cwd,
                        ai_title,
                        first_prompt,
                        last_modified: mtime,
                        turn_count,
                    });
                }
            }
        }
    }

    sessions.sort_by_key(|session| Reverse(session.last_modified));
    Ok(sessions)
}

/// Build the inline keyboard rows for the `@list_claude` reply card.
/// Each row is a Vec of (button_text, callback_data) tuples.
pub fn build_session_cards(sessions: &[SessionInfo]) -> Vec<Vec<(String, String)>> {
    let now = OffsetDateTime::now_utc();
    sessions
        .iter()
        .map(|s| {
            let title = pick_session_label(s);
            let rel = relative_time(now, s.last_modified);
            let text = format!("{} · {} turns · {}", title, s.turn_count, rel);
            let data = format!("sel_claude:{}", s.uuid);
            vec![(text, data)]
        })
        .collect()
}

/// Pick a concise label for the session card: ai-title → first-prompt → uuid fallback.
pub(crate) fn pick_session_label(s: &SessionInfo) -> String {
    const MAX: usize = 52;
    if let Some(t) = s.ai_title.as_ref() {
        if !t.trim().is_empty() {
            return truncate_label(t.trim(), MAX);
        }
    }
    if let Some(p) = s.first_prompt.as_ref() {
        if !p.trim().is_empty() {
            return truncate_label(p.trim(), MAX);
        }
    }
    let short = s.uuid.get(..8).unwrap_or(&s.uuid);
    let base = Path::new(&s.cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&s.cwd);
    format!("{} ({})", short, base)
}

fn truncate_label(s: &str, max_chars: usize) -> String {
    let single_line: String = s
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let collapsed = single_line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    let head: String = collapsed
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect();
    format!("{}…", head)
}

/// Extract text content from a Claude `type:"user"` JSONL record.
/// Handles both `message.content: "str"` and `message.content: [{type:"text",text:"..."}]`.
fn extract_user_text(v: &Value) -> Option<String> {
    let c = v.get("message")?.get("content")?;
    if let Some(s) = c.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = c.as_array() {
        for block in arr {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

/// Drop IDE/command wrappers (`<ide_selection>`, `<command-name>`, etc.) and return
/// the first non-empty text chunk, or None if nothing usable remains.
fn clean_prompt_snippet(text: &str) -> Option<String> {
    static TAG_RE: OnceLock<Regex> = OnceLock::new();
    let re = TAG_RE.get_or_init(|| {
        Regex::new(r"(?s)<[a-zA-Z_][a-zA-Z0-9_-]*>.*?</[a-zA-Z_][a-zA-Z0-9_-]*>").unwrap()
    });
    let stripped = re.replace_all(text, " ");
    let cleaned = stripped.trim();
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned.to_string())
}

fn relative_time(now: OffsetDateTime, then: OffsetDateTime) -> String {
    let secs = (now - then).whole_seconds().max(0);
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Extract mobile turns (user+assistant messages) added after \`since\` from the mobile JSONL.
#[cfg_attr(not(test), allow(dead_code))]
pub fn extract_delta(jsonl_path: &Path, since: OffsetDateTime) -> Result<String> {
    let file = File::open(jsonl_path)?;
    let reader = BufReader::new(file);
    let mut turns = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            let t = v["type"].as_str().unwrap_or("");
            if t == "user" || t == "assistant" {
                if let Some(ts_str) = v["timestamp"].as_str() {
                    if let Ok(ts) = OffsetDateTime::parse(ts_str, &Rfc3339) {
                        if ts > since {
                            let role = if t == "user" { "User" } else { "Assistant" };
                            let content = v["message"]["content"].as_str().unwrap_or("");
                            turns.push(format!("**{}:** {}", role, content));
                        }
                    }
                }
            }
        }
    }

    Ok(turns.join("\n\n"))
}

/// Keep only the \`keep\` most-recent files in \`archive_dir\` (matching \`mobile-*.jsonl\`).
#[cfg_attr(not(test), allow(dead_code))]
pub fn rotate_archives(archive_dir: &Path, keep: usize) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(archive_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("mobile-") && name.ends_with(".jsonl") {
                    let mtime = fs::metadata(&path)?.modified()?;
                    files.push((path, mtime));
                }
            }
        }
    }

    files.sort_by_key(|entry| Reverse(entry.1));

    let mut deleted = Vec::new();
    if files.len() > keep {
        for (path, _) in files.iter().skip(keep) {
            fs::remove_file(path)?;
            deleted.push(path.clone());
        }
    }

    Ok(deleted)
}

/// Parse a Telegram inbound text into a `MobileCommand`. Returns `None` if no match.
///
/// Matches:
///   - `@list_claude` or `@ls_cl_ses` (any case, leading whitespace allowed) → ListClaude
///   - `@flush_mobile` → FlushMobile
///   - `@claude <msg>` (with non-empty body) → ClaudeMsg(body.trim())
pub fn parse_mobile_command(text: &str) -> Option<MobileCommand> {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();
    if lower == "@list_claude" || lower == "@ls_cl_ses" {
        return Some(MobileCommand::ListClaude);
    }
    if lower == "@flush_mobile" {
        return Some(MobileCommand::FlushMobile);
    }
    if let Some(rest) = lower.strip_prefix("@claude") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            let prefix_len = "@claude".len();
            let body = trimmed[prefix_len..].trim();
            if body.is_empty() {
                return None;
            }
            return Some(MobileCommand::ClaudeMsg(body.to_string()));
        }
    }
    None
}

/// Append a user-type marker JSONL line to a desktop session file (see AC-5).
#[cfg_attr(not(test), allow(dead_code))]
pub fn append_fork_marker(desktop_jsonl: &Path, mobile_uuid: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .open(desktop_jsonl)?;

    // Ensure we start on a new line if the file doesn't end with one
    let metadata = file.metadata()?;
    if metadata.len() > 0 {
        let mut last_byte = [0u8; 1];
        file.read_exact_at(&mut last_byte, metadata.len() - 1)?;
        if last_byte[0] != b'\n' {
            writeln!(file)?;
        }
    }

    let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let msg = format!(
        "[Mobile session {} forked at {}. Pending merge available on next prompt.]",
        mobile_uuid, now
    );

    let line = json!({
        "type": "user",
        "isSidechain": true,
        "timestamp": now,
        "message": {
            "role": "user",
            "content": msg
        }
    });

    writeln!(file, "{}", serde_json::to_string(&line)?)?;
    file.sync_all()?;

    Ok(())
}

/// Validate that callback data matches `^sel_claude:[0-9a-f-]{36}$`.
/// Returns Some(uuid) on match, None otherwise.
pub fn parse_callback_data(data: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^sel_claude:([0-9a-f-]{36})$").unwrap());
    re.captures(data).map(|c| c[1].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use time::Duration;

    // ── rewrite_session_id ─────────────────────────────────────────────────

    #[test]
    fn test_rewrite_session_id_replaces_value() {
        let input = r#"{"type":"user","sessionId":"old-uuid-123","uuid":"line-uuid","cwd":"/repo","timestamp":"2026-04-18T00:00:00Z"}"#;
        let out = rewrite_session_id(input, "new-uuid-456").unwrap();
        assert!(out.contains(r#""sessionId":"new-uuid-456""#));
        assert!(!out.contains("old-uuid-123"));
    }

    #[test]
    fn test_rewrite_session_id_preserves_other_fields() {
        let input = r#"{"type":"user","sessionId":"old","cwd":"/my/repo","gitBranch":"main","uuid":"abc","parentUuid":"def","promptId":"ghi"}"#;
        let out = rewrite_session_id(input, "new").unwrap();
        assert!(out.contains(r#""cwd":"/my/repo""#));
        assert!(out.contains(r#""gitBranch":"main""#));
        assert!(out.contains(r#""uuid":"abc""#));
        assert!(out.contains(r#""parentUuid":"def""#));
        assert!(out.contains(r#""promptId":"ghi""#));
    }

    #[test]
    fn test_rewrite_session_id_noop_without_field() {
        let input = r#"{"type":"summary","summary":"done","leafUuid":"x"}"#;
        let out = rewrite_session_id(input, "new").unwrap();
        assert_eq!(out.trim(), input);
    }

    #[test]
    fn test_rewrite_session_id_errors_on_invalid_json() {
        let result = rewrite_session_id("not json at all", "new");
        assert!(result.is_err());
    }

    // ── fork_session ───────────────────────────────────────────────────────

    #[test]
    fn test_fork_session_rewrites_all_lines_and_writes_target() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("desktop.jsonl");
        let target = dir.path().join("mobile.jsonl");

        let content = [
            r#"{"type":"user","sessionId":"desktop-uuid","message":"hi"}"#,
            r#"{"type":"assistant","sessionId":"desktop-uuid","message":"hello"}"#,
            r#"{"type":"summary","summary":"chat"}"#,
        ]
        .join("\n");
        fs::write(&source, &content).unwrap();

        let stats = fork_session(&source, &target, "mobile-new").unwrap();

        assert_eq!(stats.lines_rewritten, 2); // summary has no sessionId
        let out = fs::read_to_string(&target).unwrap();
        assert!(out.contains(r#""sessionId":"mobile-new""#));
        assert!(!out.contains("desktop-uuid"));
        assert!(out.contains(r#""summary":"chat""#));
    }

    #[test]
    fn test_fork_session_archives_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("desktop.jsonl");
        let target = dir.path().join("mobile.jsonl");
        fs::write(&source, r#"{"type":"user","sessionId":"d"}"#).unwrap();
        fs::write(&target, "old mobile content").unwrap();

        let stats = fork_session(&source, &target, "new").unwrap();
        assert!(stats.archived_previous);
    }

    // ── detect_active_sessions ─────────────────────────────────────────────

    #[test]
    fn test_detect_active_sessions_excludes_mobile_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let active = dir.path().join("active-uuid.jsonl");
        let mobile = dir.path().join(format!("{MOBILE_UUID}.jsonl"));
        fs::write(
            &active,
            r#"{"type":"user","sessionId":"active-uuid","timestamp":"2026-04-18T00:00:00Z"}"#,
        )
        .unwrap();
        fs::write(&mobile, r#"{"type":"user","sessionId":"mobile"}"#).unwrap();

        let sessions = detect_active_sessions(dir.path(), MOBILE_UUID, 3600).unwrap();

        assert!(sessions.iter().all(|s| s.uuid != MOBILE_UUID));
    }

    #[test]
    fn test_detect_active_sessions_respects_mtime_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("fresh.jsonl");
        let stale = dir.path().join("stale.jsonl");
        fs::write(&fresh, r#"{"type":"user","sessionId":"fresh"}"#).unwrap();
        fs::write(&stale, r#"{"type":"user","sessionId":"stale"}"#).unwrap();

        // Backdate stale file
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        filetime::set_file_mtime(&stale, filetime::FileTime::from_system_time(old)).ok();

        let sessions = detect_active_sessions(dir.path(), MOBILE_UUID, 3600).unwrap();
        let uuids: Vec<_> = sessions.iter().map(|s| s.uuid.as_str()).collect();
        assert!(uuids.contains(&"fresh"));
        // stale may or may not be present depending on whether a running claude process claims it;
        // since we have no running process, mtime threshold must exclude it.
        assert!(!uuids.contains(&"stale"));
    }

    #[test]
    fn test_detect_active_sessions_sorted_desc_by_mtime() {
        let dir = tempfile::tempdir().unwrap();
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            let path = dir.path().join(format!("{name}.jsonl"));
            fs::write(&path, format!(r#"{{"type":"user","sessionId":"{name}"}}"#)).unwrap();
            let ts = std::time::SystemTime::now() - std::time::Duration::from_secs(i as u64 * 60);
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(ts)).ok();
        }
        let sessions = detect_active_sessions(dir.path(), MOBILE_UUID, 3600).unwrap();
        // newest first
        assert!(sessions[0].last_modified >= sessions[1].last_modified);
    }

    #[test]
    fn test_detect_active_sessions_reads_claude_ai_title_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop-session.jsonl");
        fs::write(
            &path,
            concat!(
                r##"{"type":"user","sessionId":"desktop-session","cwd":"/repo","message":{"role":"user","content":"# Plan Command This command invokes the planner"}}"##,
                "\n",
                r#"{"type":"ai-title","sessionId":"desktop-session","aiTitle":"Plan Phase 4 tele-agent-bus quota rotation"}"#,
                "\n",
            ),
        )
        .unwrap();

        let sessions = detect_active_sessions(dir.path(), MOBILE_UUID, 3600).unwrap();
        let session = sessions
            .iter()
            .find(|s| s.uuid == "desktop-session")
            .expect("desktop session should be detected");

        assert_eq!(
            session.ai_title.as_deref(),
            Some("Plan Phase 4 tele-agent-bus quota rotation")
        );
        assert!(build_session_cards(std::slice::from_ref(session))[0][0]
            .0
            .contains("Plan Phase 4 tele-agent-bus quota rotation"));
    }

    // ── build_session_cards ────────────────────────────────────────────────

    #[test]
    fn test_build_session_cards_one_button_per_session() {
        let sessions = vec![
            SessionInfo {
                uuid: "11111111-1111-1111-1111-111111111111".to_string(),
                cwd: "/repo".to_string(),
                ai_title: Some("2FA implementation".to_string()),
                first_prompt: None,
                last_modified: OffsetDateTime::now_utc(),
                turn_count: 47,
            },
            SessionInfo {
                uuid: "22222222-2222-2222-2222-222222222222".to_string(),
                cwd: "/other".to_string(),
                ai_title: None,
                first_prompt: None,
                last_modified: OffsetDateTime::now_utc() - Duration::minutes(5),
                turn_count: 12,
            },
        ];
        let rows = build_session_cards(&sessions);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 1);
        assert!(rows[0][0].1.starts_with("sel_claude:"));
        assert!(rows[0][0].1.contains("11111111"));
        assert!(rows[0][0].0.contains("2FA"));
    }

    #[test]
    fn test_build_session_cards_callback_data_length_limit() {
        // Telegram callback_data must be ≤ 64 bytes.
        let sessions = vec![SessionInfo {
            uuid: "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
            cwd: "/a".to_string(),
            ai_title: None,
            first_prompt: None,
            last_modified: OffsetDateTime::now_utc(),
            turn_count: 1,
        }];
        let rows = build_session_cards(&sessions);
        assert!(rows[0][0].1.len() <= 64);
    }

    // ── parse_callback_data ────────────────────────────────────────────────

    #[test]
    fn test_parse_callback_data_valid_uuid() {
        let uuid = "12345678-1234-1234-1234-123456789abc";
        let parsed = parse_callback_data(&format!("sel_claude:{uuid}")).unwrap();
        assert_eq!(parsed, uuid);
    }

    #[test]
    fn test_parse_callback_data_rejects_bad_format() {
        assert!(parse_callback_data("bogus").is_none());
        assert!(parse_callback_data("sel_claude:not-a-uuid").is_none());
        assert!(parse_callback_data("sel_claude:../../etc/passwd").is_none());
    }

    // ── parse_mobile_command ───────────────────────────────────────────────

    #[test]
    fn test_parse_mobile_command_list_aliases() {
        assert_eq!(
            parse_mobile_command("@list_claude"),
            Some(MobileCommand::ListClaude)
        );
        assert_eq!(
            parse_mobile_command("@ls_cl_ses"),
            Some(MobileCommand::ListClaude)
        );
        assert_eq!(
            parse_mobile_command("  @LIST_CLAUDE  "),
            Some(MobileCommand::ListClaude)
        );
    }

    #[test]
    fn test_parse_mobile_command_flush() {
        assert_eq!(
            parse_mobile_command("@flush_mobile"),
            Some(MobileCommand::FlushMobile)
        );
    }

    #[test]
    fn test_parse_mobile_command_claude_msg() {
        assert_eq!(
            parse_mobile_command("@claude hello world"),
            Some(MobileCommand::ClaudeMsg("hello world".to_string()))
        );
    }

    #[test]
    fn test_parse_mobile_command_claude_empty_body_is_none() {
        assert_eq!(parse_mobile_command("@claude"), None);
        assert_eq!(parse_mobile_command("@claude   "), None);
    }

    #[test]
    fn test_parse_mobile_command_unrelated_returns_none() {
        assert_eq!(parse_mobile_command("hello world"), None);
        assert_eq!(parse_mobile_command("@codex do stuff"), None);
    }

    // ── extract_delta ──────────────────────────────────────────────────────

    #[test]
    fn test_extract_delta_returns_only_turns_after_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mobile.jsonl");
        let content = [
            r#"{"type":"user","timestamp":"2026-04-18T00:00:00Z","message":{"role":"user","content":"old"}}"#,
            r#"{"type":"assistant","timestamp":"2026-04-18T00:00:01Z","message":{"role":"assistant","content":"old reply"}}"#,
            r#"{"type":"user","timestamp":"2026-04-18T01:00:00Z","message":{"role":"user","content":"new"}}"#,
            r#"{"type":"assistant","timestamp":"2026-04-18T01:00:01Z","message":{"role":"assistant","content":"new reply"}}"#,
        ].join("\n");
        fs::write(&path, content).unwrap();

        let cutoff = OffsetDateTime::parse(
            "2026-04-18T00:30:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        let delta = extract_delta(&path, cutoff).unwrap();

        assert!(delta.contains("new"));
        assert!(delta.contains("new reply"));
        assert!(!delta.contains("old reply"));
    }

    #[test]
    fn test_extract_delta_empty_when_no_turns_after_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mobile.jsonl");
        fs::write(
            &path,
            r#"{"type":"user","timestamp":"2020-01-01T00:00:00Z","message":{"role":"user","content":"ancient"}}"#,
        ).unwrap();
        let cutoff = OffsetDateTime::now_utc();
        let delta = extract_delta(&path, cutoff).unwrap();
        assert!(delta.trim().is_empty() || !delta.contains("ancient"));
    }

    // ── rotate_archives ────────────────────────────────────────────────────

    #[test]
    fn test_rotate_archives_keeps_n_most_recent() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..15 {
            let path = dir
                .path()
                .join(format!("mobile-2026-04-18T00-0{:02}-00Z.jsonl", i));
            fs::write(&path, format!("archive {i}")).unwrap();
            // Space out mtimes
            let ts = std::time::SystemTime::now() - std::time::Duration::from_secs((14 - i) * 60);
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(ts)).ok();
        }
        let deleted = rotate_archives(dir.path(), 10).unwrap();
        assert_eq!(deleted.len(), 5);
        let remaining: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("mobile-"))
            .collect();
        assert_eq!(remaining.len(), 10);
    }

    #[test]
    fn test_rotate_archives_noop_when_below_limit() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..3 {
            let path = dir.path().join(format!("mobile-{i}.jsonl"));
            fs::write(&path, format!("archive {i}")).unwrap();
        }
        let deleted = rotate_archives(dir.path(), 10).unwrap();
        assert!(deleted.is_empty());
    }

    // ── append_fork_marker ─────────────────────────────────────────────────

    #[test]
    fn test_append_fork_marker_adds_sidechain_line() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = dir.path().join("desktop.jsonl");
        fs::write(
            &desktop,
            r#"{"type":"user","sessionId":"x","message":"first"}"#,
        )
        .unwrap();

        append_fork_marker(&desktop, MOBILE_UUID).unwrap();

        let content = fs::read_to_string(&desktop).unwrap();
        let last_line = content.lines().last().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(last_line).unwrap();
        assert_eq!(parsed["isSidechain"], serde_json::Value::Bool(true));
        assert!(parsed["message"].to_string().contains("Mobile session"));
    }

    #[test]
    #[ignore = "requires live ~/.claude/projects — run with --ignored"]
    fn real_sessions_smoke() {
        let home = std::env::var("HOME").unwrap();
        let dir = PathBuf::from(format!(
            "{}/.claude/projects/-home-john-chuong-Projects-SampleRepo",
            home
        ));
        if !dir.exists() {
            panic!("project dir missing: {}", dir.display());
        }

        let sessions = detect_active_sessions(&dir, MOBILE_UUID, 30 * 60).unwrap();
        println!("found {} active sessions", sessions.len());
        for s in &sessions {
            println!(
                "  {} turns={} mtime={} title={:?} cwd={}",
                s.uuid, s.turn_count, s.last_modified, s.ai_title, s.cwd
            );
        }

        let cards = build_session_cards(&sessions);
        println!("cards:");
        for row in &cards {
            for (text, data) in row {
                println!("  text={:?} data={:?}", text, data);
            }
        }

        assert_eq!(cards.len(), sessions.len());
        for (row, s) in cards.iter().zip(sessions.iter()) {
            let (_text, data) = &row[0];
            assert_eq!(data, &format!("sel_claude:{}", s.uuid));
        }
    }
}
