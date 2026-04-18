//! Phase 3 — Mobile Claude session management.
//!
//! See docs/specs/phase3-mobile-claude-session.md.
//!
//! This module implements session forking, active-session detection, delta extraction,
//! and command parsing for the Telegram-controlled mobile Claude workflow.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use regex::Regex;
use time::OffsetDateTime;

/// Fixed mobile session uuid reused across all forks. See spec §Data Model.
pub const MOBILE_UUID: &str = "mobile-00000000-0000-0000-0000-000000000001";

/// Maximum number of mobile archive files retained (see AC-10).
pub const ARCHIVE_RETENTION: usize = 10;

/// One active Claude desktop session discovered in `~/.claude/projects/<hash>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub uuid: String,
    pub cwd: String,
    pub ai_title: Option<String>,
    pub last_modified: OffsetDateTime,
    pub turn_count: usize,
}

/// Outcome of a fork operation.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Rewrite the `sessionId` field in a single JSONL line to `new_uuid`.
///
/// - Lines that don't contain `sessionId` are returned unchanged.
/// - Other fields (cwd, gitBranch, uuid, parentUuid, promptId) are preserved.
/// - Returns error if the line is not valid JSON.
pub fn rewrite_session_id(_line: &str, _new_uuid: &str) -> Result<String> {
    unimplemented!("phase3: rewrite_session_id")
}

/// Fork a source JSONL session into a target file, rewriting sessionId to `new_uuid`.
///
/// If `target` already exists, it MUST be archived (caller responsibility or inside impl
/// depending on final design). Returns stats for logging/Telegram reply.
pub fn fork_session(_source: &Path, _target: &Path, _new_uuid: &str) -> Result<ForkStats> {
    unimplemented!("phase3: fork_session")
}

/// Discover active desktop sessions in `project_dir`, excluding `exclude_uuid` (mobile).
///
/// A session is considered "active" if any of:
///   - A running `claude` process has `--resume <uuid>` arg matching the file (via /proc)
///   - File mtime within `mtime_threshold` (default 30 min in caller)
///
/// Returns sessions sorted by last_modified DESC.
pub fn detect_active_sessions(
    _project_dir: &Path,
    _exclude_uuid: &str,
    _mtime_threshold_secs: u64,
) -> Result<Vec<SessionInfo>> {
    unimplemented!("phase3: detect_active_sessions")
}

/// Build the inline keyboard rows for the `@list_claude` reply card.
/// Each row is a Vec of (button_text, callback_data) tuples.
pub fn build_session_cards(sessions: &[SessionInfo]) -> Vec<Vec<(String, String)>> {
    let now = OffsetDateTime::now_utc();
    sessions
        .iter()
        .map(|s| {
            let title = match &s.ai_title {
                Some(t) if !t.trim().is_empty() => t.clone(),
                _ => {
                    let short = s.uuid.get(..8).unwrap_or(&s.uuid);
                    let base = Path::new(&s.cwd)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&s.cwd);
                    format!("{} ({})", short, base)
                }
            };
            let rel = relative_time(now, s.last_modified);
            let text = format!("{} · {} turns · {}", title, s.turn_count, rel);
            let data = format!("sel_claude:{}", s.uuid);
            vec![(text, data)]
        })
        .collect()
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

/// Extract mobile turns (user+assistant messages) added after `since` from the mobile JSONL.
/// Returns markdown-formatted delta content suitable for writing to pending-merge/.
pub fn extract_delta(_jsonl_path: &Path, _since: OffsetDateTime) -> Result<String> {
    unimplemented!("phase3: extract_delta")
}

/// Keep only the `keep` most-recent files in `archive_dir` (matching `mobile-*.jsonl`).
/// Returns the paths of deleted files.
pub fn rotate_archives(_archive_dir: &Path, _keep: usize) -> Result<Vec<PathBuf>> {
    unimplemented!("phase3: rotate_archives")
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
/// The line has `isSidechain: true` and does NOT affect the live process.
pub fn append_fork_marker(_desktop_jsonl: &Path, _mobile_uuid: &str) -> Result<()> {
    unimplemented!("phase3: append_fork_marker")
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
        fs::write(&active, r#"{"type":"user","sessionId":"active-uuid","timestamp":"2026-04-18T00:00:00Z"}"#).unwrap();
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

    // ── build_session_cards ────────────────────────────────────────────────

    #[test]
    fn test_build_session_cards_one_button_per_session() {
        let sessions = vec![
            SessionInfo {
                uuid: "11111111-1111-1111-1111-111111111111".to_string(),
                cwd: "/repo".to_string(),
                ai_title: Some("2FA implementation".to_string()),
                last_modified: OffsetDateTime::now_utc(),
                turn_count: 47,
            },
            SessionInfo {
                uuid: "22222222-2222-2222-2222-222222222222".to_string(),
                cwd: "/other".to_string(),
                ai_title: None,
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
        assert_eq!(parse_mobile_command("@list_claude"), Some(MobileCommand::ListClaude));
        assert_eq!(parse_mobile_command("@ls_cl_ses"), Some(MobileCommand::ListClaude));
        assert_eq!(parse_mobile_command("  @LIST_CLAUDE  "), Some(MobileCommand::ListClaude));
    }

    #[test]
    fn test_parse_mobile_command_flush() {
        assert_eq!(parse_mobile_command("@flush_mobile"), Some(MobileCommand::FlushMobile));
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
        ).unwrap();
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
            let path = dir.path().join(format!("mobile-2026-04-18T00-0{:02}-00Z.jsonl", i));
            fs::write(&path, format!("archive {i}")).unwrap();
            // Space out mtimes
            let ts = std::time::SystemTime::now() - std::time::Duration::from_secs((14 - i) * 60);
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(ts)).ok();
        }
        let deleted = rotate_archives(dir.path(), 10).unwrap();
        assert_eq!(deleted.len(), 5);
        let remaining: Vec<_> = fs::read_dir(dir.path()).unwrap()
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
        fs::write(&desktop, r#"{"type":"user","sessionId":"x","message":"first"}"#).unwrap();

        append_fork_marker(&desktop, MOBILE_UUID).unwrap();

        let content = fs::read_to_string(&desktop).unwrap();
        let last_line = content.lines().last().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(last_line).unwrap();
        assert_eq!(parsed["isSidechain"], serde_json::Value::Bool(true));
        assert!(parsed["message"].to_string().contains("Mobile session"));
    }
}
