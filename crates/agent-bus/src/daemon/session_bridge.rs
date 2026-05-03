use agent_bus_core::auth_context::AgentKind;
use agent_bus_core::state::{BridgedSessionState, StateHandle};
use anyhow::{anyhow, Context};
use base64::Engine;
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
#[cfg(test)]
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeCommand {
    List(AgentKind),
    Chat(AgentKind, String),
    Flush(AgentKind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionInfo {
    pub id: String,
    pub path: PathBuf,
    pub cwd: String,
    pub title: Option<String>,
    pub updated_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeminiSessionInfo {
    pub id: String,
    pub title: Option<String>,
    pub repo_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AntigravitySessionRecord {
    id: String,
    title: Option<String>,
    repo_path: Option<String>,
    updated_secs: u64,
}

#[cfg(test)]
static ANTIGRAVITY_DB_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);
#[cfg(test)]
static ANTIGRAVITY_BRAIN_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);
#[cfg(test)]
pub(crate) static ANTIGRAVITY_OVERRIDE_TEST_LOCK: Mutex<()> = Mutex::new(());

pub fn parse_bridge_command(text: &str) -> Option<BridgeCommand> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    // AC-SB2: Generic Command Parser
    // /list_claude
    // /ls_cl_ses (legacy alias)
    // @flush_claude
    // @flush_mobile (legacy alias)
    // @claude hi

    if matches!(trimmed, "/list_claude" | "/ls_cl_ses") {
        return Some(BridgeCommand::List(AgentKind::Claude));
    }
    if trimmed == "/list_codex" {
        return Some(BridgeCommand::List(AgentKind::Codex));
    }
    if trimmed == "/list_gemini" {
        return Some(BridgeCommand::List(AgentKind::Gemini));
    }
    if trimmed == "/list_antigravity" {
        return Some(BridgeCommand::List(AgentKind::Antigravity));
    }
    if trimmed == "@flush_claude" || trimmed == "@flush_mobile" {
        return Some(BridgeCommand::Flush(AgentKind::Claude));
    }
    if trimmed == "@flush_codex" {
        return Some(BridgeCommand::Flush(AgentKind::Codex));
    }
    if trimmed == "@flush_gemini" {
        return Some(BridgeCommand::Flush(AgentKind::Gemini));
    }
    if trimmed == "@flush_antigravity" {
        return Some(BridgeCommand::Flush(AgentKind::Antigravity));
    }

    if let Some(rest) = trimmed.strip_prefix("@claude") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            let msg = rest.trim();
            if !msg.is_empty() {
                return Some(BridgeCommand::Chat(AgentKind::Claude, msg.to_string()));
            }
        }
    }

    if let Some(rest) = trimmed.strip_prefix("@codex") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            let msg = rest.trim();
            if !msg.is_empty() {
                return Some(BridgeCommand::Chat(AgentKind::Codex, msg.to_string()));
            }
        }
    }

    if let Some(rest) = trimmed.strip_prefix("@gemini") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            let msg = rest.trim();
            if !msg.is_empty() {
                return Some(BridgeCommand::Chat(AgentKind::Gemini, msg.to_string()));
            }
        }
    }

    if let Some(rest) = trimmed.strip_prefix("@antigravity") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            let msg = rest.trim();
            if !msg.is_empty() {
                return Some(BridgeCommand::Chat(AgentKind::Antigravity, msg.to_string()));
            }
        }
    }

    None
}

pub fn parse_callback_data(data: &str) -> Option<(AgentKind, String)> {
    if let Some(id) = data.strip_prefix("sel_claude:") {
        return Some((AgentKind::Claude, id.to_string()));
    }
    if let Some(id) = data.strip_prefix("sel_codex:") {
        return Some((AgentKind::Codex, id.to_string()));
    }
    if let Some(id) = data.strip_prefix("sel_gemini:") {
        return Some((AgentKind::Gemini, id.to_string()));
    }
    if let Some(id) = data.strip_prefix("sel_antigravity:") {
        return Some((AgentKind::Antigravity, id.to_string()));
    }
    None
}

/// Locate the gemini chats directory for a given repo. Gemini stores per-repo
/// chats under `~/.gemini/tmp/<repo_basename>/chats/`.
pub fn gemini_chats_dir_for(repo_path: &Path) -> Option<PathBuf> {
    let basename = repo_path.file_name().and_then(|name| name.to_str())?;
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(
        home.join(".gemini")
            .join("tmp")
            .join(basename)
            .join("chats"),
    )
}

/// Find the most recently modified gemini chat session for the given repo and
/// return its full sessionId. Used after a headless run so we can save the
/// just-created session to the bridge — `gemini --list-sessions` does not
/// guarantee newest-first ordering, so file mtime is more reliable.
pub fn find_latest_gemini_session_id(repo_path: &Path) -> Option<String> {
    let dir = gemini_chats_dir_for(repo_path)?;
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if newest.as_ref().is_none_or(|(t, _)| mtime > *t) {
            newest = Some((mtime, path));
        }
    }
    let (_, path) = newest?;
    let text = std::fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub fn parse_gemini_list_sessions(output: &str, limit: usize) -> Vec<GeminiSessionInfo> {
    let mut sessions = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((prefix, rest)) = trimmed.split_once(". ") else {
            continue;
        };
        if !prefix.chars().all(|ch| ch.is_ascii_digit()) {
            continue;
        }
        let Some(open_bracket) = rest.rfind('[') else {
            continue;
        };
        let Some(close_bracket) = rest.rfind(']') else {
            continue;
        };
        if close_bracket <= open_bracket + 1 {
            continue;
        }
        let id = rest[open_bracket + 1..close_bracket].trim();
        if id.is_empty() {
            continue;
        }
        let mut title_part = rest[..open_bracket].trim();
        if let Some(idx) = title_part.rfind(" (") {
            title_part = title_part[..idx].trim_end();
        }
        let title = if title_part.is_empty() {
            None
        } else {
            Some(title_part.to_string())
        };
        sessions.push(GeminiSessionInfo {
            id: id.to_string(),
            title,
            repo_path: None,
        });
        if sessions.len() >= limit {
            break;
        }
    }
    sessions
}

pub fn detect_antigravity_sessions(
    repo_path: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<GeminiSessionInfo>> {
    let mut sessions = Vec::new();
    sessions.extend(read_antigravity_summary_sessions()?);
    sessions.extend(read_antigravity_brain_sessions()?);

    let repo_path = repo_path.map(normalize_path_text);
    if let Some(repo_path) = repo_path.as_ref() {
        let prefix = format!("{repo_path}/");
        sessions.retain_mut(|session| {
            let Some(path) = session.repo_path.as_deref() else {
                return false;
            };
            let normalized = normalize_path_text(path);
            if normalized == *repo_path || normalized.starts_with(&prefix) {
                // Normalize the session's repo_path to the actual repo root so
                // the display prefix matches the user's repo (not the file's
                // parent dir extracted from the antigravity overview).
                session.repo_path = Some(repo_path.clone());
                true
            } else {
                false
            }
        });
    }

    sessions.sort_by(|a, b| {
        b.updated_secs
            .cmp(&a.updated_secs)
            .then_with(|| b.id.cmp(&a.id))
    });

    Ok(dedupe_gemini_sessions(
        sessions
            .into_iter()
            .map(|session| GeminiSessionInfo {
                id: session.id,
                title: session.title,
                repo_path: session.repo_path,
            })
            .collect(),
        limit,
    ))
}

fn read_antigravity_summary_sessions() -> anyhow::Result<Vec<AntigravitySessionRecord>> {
    let db_path = antigravity_state_db_path();
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("open Antigravity state db")?;
    let blob = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            ["antigravityUnifiedStateSync.trajectorySummaries"],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .context("read trajectory summaries")?;
    let Some(blob) = blob else {
        return Ok(Vec::new());
    };

    let raw = base64::engine::general_purpose::STANDARD
        .decode(blob)
        .context("decode Antigravity trajectory summaries")?;
    let mut sessions = Vec::new();

    for entry in protobuf_len_fields(&raw) {
        let strings = collect_nested_strings(entry, 0);
        let Some(repo_uri) = strings
            .iter()
            .map(|s| s.trim())
            .find(|s| s.starts_with("file:///") && s.len() > "file:///".len())
        else {
            continue;
        };
        let Some(entry_repo_path) = file_uri_to_path_text(repo_uri) else {
            continue;
        };
        let Some(id) = strings.iter().find_map(|s| {
            let candidate = s.trim();
            if looks_like_uuid(candidate) {
                Some(candidate.to_string())
            } else {
                None
            }
        }) else {
            continue;
        };

        // Prefer the AI-generated title from the inner base64 protobuf; fall
        // back to the heuristic candidate scan when the structure changes.
        let title = extract_antigravity_summary_title(entry).or_else(|| {
            strings
                .iter()
                .find_map(|s| pick_antigravity_title_candidate(s))
                .map(|title| title.to_string())
        });

        sessions.push(AntigravitySessionRecord {
            id,
            title,
            repo_path: Some(entry_repo_path),
            updated_secs: 0,
        });
    }

    Ok(sessions)
}

fn read_antigravity_brain_sessions() -> anyhow::Result<Vec<AntigravitySessionRecord>> {
    let root = antigravity_brain_root();
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !looks_like_uuid(id) {
            continue;
        }
        let overview = path
            .join(".system_generated")
            .join("logs")
            .join("overview.txt");
        if !overview.exists() {
            continue;
        }
        let text = match std::fs::read_to_string(&overview) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let title = extract_antigravity_overview_title(&text);
        let repo_path = extract_antigravity_overview_repo(&text);
        let updated_secs = overview
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        sessions.push(AntigravitySessionRecord {
            id: id.to_string(),
            title,
            repo_path,
            updated_secs,
        });
    }
    Ok(sessions)
}

pub fn dedupe_gemini_sessions(
    sessions: Vec<GeminiSessionInfo>,
    limit: usize,
) -> Vec<GeminiSessionInfo> {
    let mut seen = HashMap::<String, GeminiSessionInfo>::new();
    let mut ordered = Vec::new();
    for session in sessions {
        if seen.contains_key(&session.id) {
            continue;
        }
        ordered.push(session.id.clone());
        seen.insert(session.id.clone(), session);
    }
    ordered
        .into_iter()
        .filter_map(|id| seen.remove(&id))
        .take(limit)
        .collect()
}

fn antigravity_state_db_path() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = ANTIGRAVITY_DB_OVERRIDE
        .lock()
        .expect("antigravity db override lock poisoned")
        .clone()
    {
        return path;
    }

    #[cfg(target_os = "macos")]
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Antigravity")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb");
    }

    #[cfg(target_os = "windows")]
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata)
            .join("Antigravity")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb");
    }

    std::env::var("HOME")
        .map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("Antigravity")
                .join("User")
                .join("globalStorage")
                .join("state.vscdb")
        })
        .unwrap_or_else(|_| PathBuf::from(".config/Antigravity/User/globalStorage/state.vscdb"))
}

fn antigravity_brain_root() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = ANTIGRAVITY_BRAIN_OVERRIDE
        .lock()
        .expect("antigravity brain override lock poisoned")
        .clone()
    {
        return path;
    }

    #[cfg(target_os = "macos")]
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".gemini")
            .join("antigravity")
            .join("brain");
    }

    #[cfg(target_os = "windows")]
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        return PathBuf::from(userprofile)
            .join(".gemini")
            .join("antigravity")
            .join("brain");
    }

    std::env::var("HOME")
        .map(|home| {
            PathBuf::from(home)
                .join(".gemini")
                .join("antigravity")
                .join("brain")
        })
        .unwrap_or_else(|_| PathBuf::from(".gemini/antigravity/brain"))
}

#[cfg(test)]
pub(crate) fn set_antigravity_state_db_override(path: Option<PathBuf>) {
    *ANTIGRAVITY_DB_OVERRIDE
        .lock()
        .expect("antigravity db override lock poisoned") = path;
}

#[cfg(test)]
pub(crate) fn set_antigravity_brain_root_override(path: Option<PathBuf>) {
    *ANTIGRAVITY_BRAIN_OVERRIDE
        .lock()
        .expect("antigravity brain override lock poisoned") = path;
}

/// Parse protobuf and return only length-delimited fields with their field
/// numbers. Skips other wire types so callers can index into specific fields.
fn protobuf_tagged_len_fields(bytes: &[u8]) -> Vec<(u64, &[u8])> {
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < bytes.len() {
        let Some(tag) = read_varint(bytes, &mut idx) else {
            break;
        };
        let field_num = tag >> 3;
        let wire_type = tag & 0x07;
        match wire_type {
            0 => {
                if read_varint(bytes, &mut idx).is_none() {
                    break;
                }
            }
            1 => {
                if idx + 8 > bytes.len() {
                    break;
                }
                idx += 8;
            }
            2 => {
                let Some(len) = read_varint(bytes, &mut idx) else {
                    break;
                };
                let len = len as usize;
                if idx + len > bytes.len() {
                    break;
                }
                out.push((field_num, &bytes[idx..idx + len]));
                idx += len;
            }
            5 => {
                if idx + 4 > bytes.len() {
                    break;
                }
                idx += 4;
            }
            _ => break,
        }
    }
    out
}

/// Extract the AI-generated title from an Antigravity trajectorySummaries
/// entry. The structure is:
///   Entry { 1: session_id, 2: Wrapper { 1: base64-encoded protobuf } }
///   Decoded inner protobuf: { 1: title (utf-8) }
fn extract_antigravity_summary_title(entry: &[u8]) -> Option<String> {
    let wrapper = protobuf_tagged_len_fields(entry)
        .into_iter()
        .find_map(|(num, bytes)| if num == 2 { Some(bytes) } else { None })?;
    let b64 = protobuf_tagged_len_fields(wrapper)
        .into_iter()
        .find_map(|(num, bytes)| if num == 1 { Some(bytes) } else { None })?;
    let b64_text = std::str::from_utf8(b64).ok()?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64_text.trim())
        .ok()?;
    let title_bytes = protobuf_tagged_len_fields(&decoded)
        .into_iter()
        .find_map(|(num, bytes)| if num == 1 { Some(bytes) } else { None })?;
    let title = std::str::from_utf8(title_bytes).ok()?.trim();
    if title.is_empty() {
        return None;
    }
    Some(title.to_string())
}

fn protobuf_len_fields(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < bytes.len() {
        let Some(tag) = read_varint(bytes, &mut idx) else {
            break;
        };
        let wire_type = tag & 0x07;
        match wire_type {
            0 => {
                if read_varint(bytes, &mut idx).is_none() {
                    break;
                }
            }
            1 => {
                if idx + 8 > bytes.len() {
                    break;
                }
                idx += 8;
            }
            2 => {
                let Some(len) = read_varint(bytes, &mut idx) else {
                    break;
                };
                let len = len as usize;
                if idx + len > bytes.len() {
                    break;
                }
                out.push(&bytes[idx..idx + len]);
                idx += len;
            }
            5 => {
                if idx + 4 > bytes.len() {
                    break;
                }
                idx += 4;
            }
            _ => break,
        }
    }
    out
}

fn collect_nested_strings(bytes: &[u8], depth: usize) -> Vec<String> {
    if depth > 5 {
        return Vec::new();
    }

    let mut out = Vec::new();
    for field in protobuf_len_fields(bytes) {
        if let Ok(text) = std::str::from_utf8(field) {
            let trimmed = text.trim_matches(char::from(0)).trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
                if looks_like_base64(trimmed) {
                    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(trimmed) {
                        out.extend(collect_nested_strings(&decoded, depth + 1));
                    }
                }
            }
        }
        out.extend(collect_nested_strings(field, depth + 1));
    }
    out
}

fn read_varint(bytes: &[u8], idx: &mut usize) -> Option<u64> {
    let mut shift = 0u32;
    let mut value = 0u64;
    while *idx < bytes.len() {
        let byte = bytes[*idx];
        *idx += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
    None
}

fn looks_like_base64(text: &str) -> bool {
    if text.len() < 8 || text.len() % 4 != 0 {
        return false;
    }
    text.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
}

fn looks_like_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (idx, byte) in bytes.iter().enumerate() {
        let is_dash = matches!(idx, 8 | 13 | 18 | 23);
        if is_dash {
            if *byte != b'-' {
                return false;
            }
        } else if !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

fn pick_antigravity_title_candidate(text: &str) -> Option<Cow<'_, str>> {
    let title = text.trim();
    if title.is_empty()
        || title.contains('\n')
        || title.starts_with("file:///")
        || title.starts_with("http://")
        || title.starts_with("https://")
        || title.starts_with("cci:")
        || looks_like_uuid(title)
        || looks_like_base64(title)
        || title.len() > 120
    {
        return None;
    }
    if title.contains('/') && !title.contains(' ') {
        return None;
    }
    Some(Cow::Borrowed(title))
}

fn file_uri_to_path_text(uri: &str) -> Option<String> {
    let path = uri.strip_prefix("file://")?;
    Some(percent_decode(path))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(input.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' && idx + 2 < bytes.len() {
            let hex = &input[idx + 1..idx + 3];
            if let Ok(value) = u8::from_str_radix(hex, 16) {
                out.push(value);
                idx += 3;
                continue;
            }
        }
        out.push(bytes[idx]);
        idx += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn extract_antigravity_overview_title(text: &str) -> Option<String> {
    let content = extract_antigravity_overview_content(text)?;
    let (_, after_open) = content.split_once("<USER_REQUEST>")?;
    let (body, _) = after_open.split_once("</USER_REQUEST>")?;
    clean_codex_title(body.trim())
}

fn extract_antigravity_overview_repo(text: &str) -> Option<String> {
    let content = extract_antigravity_overview_content(text)?;
    let mut candidates = Vec::new();
    for line in content.lines() {
        if let Some(path) = line
            .trim()
            .strip_prefix("Active Document: ")
            .and_then(extract_repo_root_from_doc_path)
        {
            candidates.push(path);
        } else if let Some(path) = line
            .trim()
            .strip_prefix("- ")
            .and_then(extract_repo_root_from_doc_path)
        {
            candidates.push(path);
        }
    }
    candidates.into_iter().next()
}

fn extract_antigravity_overview_content(text: &str) -> Option<String> {
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(content) = value.get("content").and_then(Value::as_str) else {
            continue;
        };
        if content.contains("<USER_REQUEST>") {
            return Some(content.to_string());
        }
    }
    None
}

fn extract_repo_root_from_doc_path(line: &str) -> Option<String> {
    let path = line.split(" (").next()?.trim();
    if let Some(repo) = path.split("/.git/worktrees/").next() {
        if repo != path {
            return Some(repo.to_string());
        }
    }
    if let Some(repo) = path.split("/.git/").next() {
        if repo != path {
            return Some(repo.to_string());
        }
    }
    Path::new(path)
        .parent()
        .map(|parent| parent.display().to_string())
}

pub fn detect_codex_sessions(
    codex_home: &Path,
    repo_path: &str,
    limit: usize,
) -> anyhow::Result<Vec<CodexSessionInfo>> {
    let root = codex_home.join("sessions");
    if !root.exists() {
        return Ok(Vec::new());
    }

    let repo_path = normalize_path_text(repo_path);
    let mut files = Vec::new();
    collect_codex_rollouts(&root, &mut files)?;
    let titles = load_codex_session_titles(codex_home);

    let mut sessions = Vec::new();
    for path in files {
        let Some(mut session) = read_codex_session_meta(&path, &titles)? else {
            continue;
        };
        if normalize_path_text(&session.cwd) != repo_path {
            continue;
        }
        session.updated_secs = path
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(session.updated_secs);
        sessions.push(session);
    }
    sessions.sort_by(|a, b| {
        b.updated_secs
            .cmp(&a.updated_secs)
            .then_with(|| b.id.cmp(&a.id))
    });
    sessions.truncate(limit);
    Ok(sessions)
}

fn collect_codex_rollouts(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_codex_rollouts(&path, out)?;
        } else if file_type.is_file()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
        {
            out.push(path);
        }
    }
    Ok(())
}

fn load_codex_session_titles(codex_home: &Path) -> HashMap<String, String> {
    let path = codex_home.join("session_index.jsonl");
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };

    let mut titles = HashMap::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(title) = value
            .get("thread_name")
            .and_then(Value::as_str)
            .and_then(clean_codex_title)
        else {
            continue;
        };
        titles.insert(id.to_string(), title);
    }
    titles
}

fn read_codex_session_meta(
    path: &Path,
    titles: &HashMap<String, String>,
) -> anyhow::Result<Option<CodexSessionInfo>> {
    let text = std::fs::read_to_string(path)?;
    for line in text.lines().take(20) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let Some(id) = payload.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(cwd) = payload.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        let updated_secs = payload
            .get("timestamp")
            .or_else(|| value.get("timestamp"))
            .and_then(Value::as_str)
            .and_then(|ts| {
                time::OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339).ok()
            })
            .map(|ts| ts.unix_timestamp().max(0) as u64)
            .unwrap_or(0);
        return Ok(Some(CodexSessionInfo {
            id: id.to_string(),
            path: path.to_path_buf(),
            cwd: cwd.to_string(),
            title: payload
                .get("title")
                .and_then(Value::as_str)
                .and_then(clean_codex_title)
                .or_else(|| titles.get(id).cloned())
                .or_else(|| infer_codex_title(&text)),
            updated_secs,
        }));
    }
    Ok(None)
}

fn infer_codex_title(text: &str) -> Option<String> {
    for line in text.lines().take(80) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(Value::as_str) != Some("message")
            || payload.get("role").and_then(Value::as_str) != Some("user")
        {
            continue;
        }
        let Some(content) = payload.get("content").and_then(Value::as_array) else {
            continue;
        };
        for item in content {
            if item.get("type").and_then(Value::as_str) != Some("input_text") {
                continue;
            }
            if let Some(candidate) = item
                .get("text")
                .and_then(Value::as_str)
                .and_then(extract_codex_title_candidate)
            {
                return Some(candidate);
            }
        }
    }
    None
}

fn extract_codex_title_candidate(text: &str) -> Option<String> {
    if text.starts_with("# AGENTS.md instructions") || text.starts_with("<environment_context>") {
        return None;
    }
    if let Some((_, after)) = text.rsplit_once("My request for Codex:") {
        return clean_codex_title(after);
    }
    if let Some((_, after)) = text.rsplit_once("from @") {
        if let Some((_, body)) = after.split_once('\n') {
            return clean_codex_title(body);
        }
    }
    if text.starts_with("## Context from shared memory") {
        return None;
    }
    clean_codex_title(text)
}

fn clean_codex_title(text: &str) -> Option<String> {
    let mut title = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return None;
    }
    const MAX_CHARS: usize = 48;
    if title.chars().count() > MAX_CHARS {
        title = title.chars().take(MAX_CHARS - 1).collect::<String>();
        title.push('…');
    }
    Some(title)
}

fn normalize_path_text(path: &str) -> String {
    path.trim_end_matches('/').to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDirection {
    DesktopToMobile,
    MobileToDesktop,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub copied: usize,
    pub skipped: usize,
    pub errors: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BridgeSyncStats {
    pub desktop_to_mobile: SyncStats,
    pub mobile_to_desktop: SyncStats,
}

impl BridgeSyncStats {
    pub fn copied(self) -> usize {
        self.desktop_to_mobile.copied + self.mobile_to_desktop.copied
    }

    pub fn skipped(self) -> usize {
        self.desktop_to_mobile.skipped + self.mobile_to_desktop.skipped
    }

    pub fn errors(self) -> usize {
        self.desktop_to_mobile.errors + self.mobile_to_desktop.errors
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BridgePollStats {
    pub sessions: usize,
    pub copied: usize,
    pub skipped: usize,
    pub errors: usize,
}

#[derive(Debug)]
struct BridgeLock {
    _file: std::fs::File,
}

pub async fn sync_bridged_session_locked(
    chat_id: &str,
    agent: &str,
    bridge: &mut BridgedSessionState,
) -> anyhow::Result<BridgeSyncStats> {
    let lock_path = bridge_lock_path(chat_id, agent)?;
    let _lock = tokio::task::spawn_blocking(move || acquire_bridge_lock(&lock_path))
        .await
        .context("session bridge lock task failed")??;
    sync_bridged_session(bridge)
}

fn bridge_lock_path(chat_id: &str, agent: &str) -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("AGENT_BUS_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent-bus")))
        .ok_or_else(|| anyhow!("HOME is not set"))?;
    let lock_name = format!(
        "session-bridge-{}-{}.lock",
        sanitize_lock_component(chat_id),
        sanitize_lock_component(agent)
    );
    Ok(home.join("locks").join(lock_name))
}

fn sanitize_lock_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn acquire_bridge_lock(path: &Path) -> anyhow::Result<BridgeLock> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create lock dir {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to open session bridge lock {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("failed to lock session bridge {}", path.display()))?;
    Ok(BridgeLock { _file: file })
}

/// Generic JSONL sync cycle with loop prevention and offset advancement.
/// To be implemented in Code phase.
pub fn sync_cycle(
    agent: AgentKind,
    direction: SyncDirection,
    source_path: &std::path::Path,
    target_path: &std::path::Path,
    source_offset: &mut u64,
    target_session_id: &str,
) -> anyhow::Result<SyncStats> {
    let bytes = std::fs::read(source_path)?;
    let start = (*source_offset as usize).min(bytes.len());
    let tail = &bytes[start..];
    let Some(last_newline) = tail.iter().rposition(|b| *b == b'\n') else {
        return Ok(SyncStats {
            copied: 0,
            skipped: 0,
            errors: 0,
        });
    };
    let complete_len = last_newline + 1;
    let complete = &tail[..complete_len];
    let mut stats = SyncStats {
        copied: 0,
        skipped: 0,
        errors: 0,
    };
    let mut rendered = Vec::new();

    for line in complete.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(mut value) = serde_json::from_slice::<Value>(line) else {
            stats.errors += 1;
            continue;
        };
        if should_skip_synced_line(&value, direction) {
            stats.skipped += 1;
            continue;
        }
        let source_session_id = jsonl_session_id(&value).unwrap_or_default().to_string();
        rewrite_jsonl_session(&mut value, target_session_id);
        add_sync_metadata(&mut value, agent, direction, &source_session_id);
        rendered.push(serde_json::to_vec(&value)?);
        stats.copied += 1;
    }

    if !rendered.is_empty() {
        if let Some(parent) = target_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut target = OpenOptions::new()
            .create(true)
            .append(true)
            .open(target_path)?;
        for line in rendered {
            target.write_all(&line)?;
            target.write_all(b"\n")?;
        }
        target.sync_all()?;
    }

    *source_offset += complete_len as u64;
    Ok(stats)
}

pub fn sync_bridged_session(bridge: &mut BridgedSessionState) -> anyhow::Result<BridgeSyncStats> {
    let agent = AgentKind::from_str(&bridge.agent)?;
    if matches!(agent, AgentKind::Gemini | AgentKind::Antigravity) {
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc().unix_timestamp().to_string());
        bridge.sync.last_synced_at = Some(now);
        bridge.sync.last_error = None;
        return Ok(BridgeSyncStats::default());
    }
    let desktop_path = validate_bridge_path(Path::new(&bridge.desktop_path), true)?;
    let mobile_path = validate_bridge_path(Path::new(&bridge.mobile_path), false)?;
    if desktop_path == mobile_path {
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc().unix_timestamp().to_string());
        bridge.sync.last_synced_at = Some(now);
        bridge.sync.last_error = None;
        return Ok(BridgeSyncStats::default());
    }
    let desktop_to_mobile = sync_cycle(
        agent,
        SyncDirection::DesktopToMobile,
        &desktop_path,
        &mobile_path,
        &mut bridge.sync.desktop_offset,
        &bridge.mobile_session_id,
    )?;
    let mobile_to_desktop = sync_cycle(
        agent,
        SyncDirection::MobileToDesktop,
        &mobile_path,
        &desktop_path,
        &mut bridge.sync.mobile_offset,
        &bridge.desktop_session_id,
    )?;
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc().unix_timestamp().to_string());
    bridge.sync.last_synced_at = Some(now);
    bridge.sync.last_error = None;
    Ok(BridgeSyncStats {
        desktop_to_mobile,
        mobile_to_desktop,
    })
}

pub async fn wait_for_codex_desktop_reply(
    desktop_path: PathBuf,
    start_offset: u64,
    timeout: std::time::Duration,
) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut offset = start_offset;
    let mut last_agent_message = None;

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!("timed out waiting for Codex desktop reply"));
        }

        if let Some(reply) =
            read_codex_reply_delta(&desktop_path, &mut offset, &mut last_agent_message)?
        {
            return Ok(reply);
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

fn read_codex_reply_delta(
    path: &Path,
    offset: &mut u64,
    last_agent_message: &mut Option<String>,
) -> anyhow::Result<Option<String>> {
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let len = file.metadata()?.len();
    if len <= *offset {
        return Ok(None);
    }

    file.seek(SeekFrom::Start(*offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let Some(last_newline) = bytes.iter().rposition(|b| *b == b'\n') else {
        return Ok(None);
    };
    let complete_len = last_newline + 1;
    let complete = &bytes[..complete_len];

    for line in complete.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if let Some(message) = codex_agent_message(&value) {
            *last_agent_message = Some(message);
        }
        if is_codex_task_complete(&value) {
            if let Some(message) =
                codex_task_complete_message(&value).or_else(|| last_agent_message.clone())
            {
                *offset += complete_len as u64;
                return Ok(Some(message));
            }
        }
    }

    *offset += complete_len as u64;
    Ok(None)
}

fn codex_agent_message(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) == Some("event_msg") {
        let payload = value.get("payload")?;
        if payload.get("type").and_then(Value::as_str) == Some("agent_message") {
            return payload
                .get("message")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }
    }

    if value.get("type").and_then(Value::as_str) == Some("response_item") {
        let payload = value.get("payload")?;
        if payload.get("type").and_then(Value::as_str) == Some("message")
            && payload.get("role").and_then(Value::as_str) == Some("assistant")
        {
            let mut parts = Vec::new();
            for item in payload.get("content")?.as_array()? {
                if item.get("type").and_then(Value::as_str) == Some("output_text") {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        parts.push(text.to_string());
                    }
                }
            }
            if !parts.is_empty() {
                return Some(parts.join("\n"));
            }
        }
    }

    None
}

fn is_codex_task_complete(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("event_msg")
        && value
            .get("payload")
            .and_then(|payload| payload.get("type"))
            .and_then(Value::as_str)
            == Some("task_complete")
}

fn codex_task_complete_message(value: &Value) -> Option<String> {
    value
        .get("payload")
        .and_then(|payload| payload.get("last_agent_message"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn validate_bridge_path(path: &Path, must_exist: bool) -> anyhow::Result<PathBuf> {
    reject_symlink_components(path)?;
    if path.exists() {
        return path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize bridge path {}", path.display()));
    }
    if must_exist {
        return Err(anyhow!(
            "session bridge path does not exist: {}",
            path.display()
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("session bridge path has no parent: {}", path.display()))?;
    reject_symlink_components(parent)?;
    let parent = parent.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize bridge path parent {}",
            parent.display()
        )
    })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("session bridge path has no file name: {}", path.display()))?;
    Ok(parent.join(file_name))
}

fn reject_symlink_components(path: &Path) -> anyhow::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let Ok(meta) = std::fs::symlink_metadata(&current) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            return Err(anyhow!(
                "session bridge path contains symlink: {}",
                current.display()
            ));
        }
    }
    Ok(())
}

pub async fn sync_all_bridged_sessions_once(state: StateHandle) -> BridgePollStats {
    let snapshot = state.snapshot().await;
    let mut poll = BridgePollStats::default();

    for (chat_id, by_agent) in snapshot.bridged_sessions {
        for (agent, mut bridge) in by_agent {
            poll.sessions += 1;
            match sync_bridged_session_locked(&chat_id, &agent, &mut bridge).await {
                Ok(stats) => {
                    poll.copied += stats.copied();
                    poll.skipped += stats.skipped();
                    poll.errors += stats.errors();
                }
                Err(err) => {
                    poll.errors += 1;
                    bridge.sync.last_error = Some(err.to_string());
                }
            }
            if let Err(err) = state
                .set_bridged_session(chat_id.clone(), agent.clone(), bridge)
                .await
            {
                tracing::warn!(
                    target: "agent_bus::session_bridge",
                    chat_id = %chat_id,
                    agent = %agent,
                    error = %err,
                    "failed to persist bridge sync state"
                );
                poll.errors += 1;
            }
        }
    }

    poll
}

pub fn spawn_session_bridge_sync(
    state: StateHandle,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            let stats = sync_all_bridged_sessions_once(state.clone()).await;
            if stats.sessions > 0 {
                tracing::debug!(
                    target: "agent_bus::session_bridge",
                    sessions = stats.sessions,
                    copied = stats.copied,
                    skipped = stats.skipped,
                    errors = stats.errors,
                    "session bridge sync tick complete"
                );
            }
        }
    })
}

fn should_skip_synced_line(value: &Value, direction: SyncDirection) -> bool {
    let skip_from = match direction {
        SyncDirection::DesktopToMobile => "mobile",
        SyncDirection::MobileToDesktop => "desktop",
    };
    value
        .get("agentBusSync")
        .and_then(|sync| sync.get("from"))
        .and_then(Value::as_str)
        == Some(skip_from)
}

fn rewrite_jsonl_session(value: &mut Value, target_session_id: &str) {
    if let Some(obj) = value.as_object_mut() {
        if obj.contains_key("sessionId") {
            obj.insert("sessionId".to_string(), json!(target_session_id));
        }
        if obj.get("type").and_then(Value::as_str) == Some("session_meta") {
            if let Some(payload) = obj.get_mut("payload").and_then(Value::as_object_mut) {
                if payload.contains_key("id") {
                    payload.insert("id".to_string(), json!(target_session_id));
                }
            }
        }
    }
}

fn jsonl_session_id(value: &Value) -> Option<&str> {
    value.get("sessionId").and_then(Value::as_str).or_else(|| {
        if value.get("type").and_then(Value::as_str) == Some("session_meta") {
            value
                .get("payload")
                .and_then(|payload| payload.get("id"))
                .and_then(Value::as_str)
        } else {
            None
        }
    })
}

fn add_sync_metadata(
    value: &mut Value,
    agent: AgentKind,
    direction: SyncDirection,
    source_session_id: &str,
) {
    let from = match direction {
        SyncDirection::DesktopToMobile => "desktop",
        SyncDirection::MobileToDesktop => "mobile",
    };
    let synced_at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc().unix_timestamp().to_string());
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "agentBusSync".to_string(),
            json!({
                "agent": agent.as_str(),
                "from": from,
                "sourceSessionId": source_session_id,
                "syncedAt": synced_at,
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_sync_desktop_to_mobile_advances_offset() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = dir.path().join("desktop.jsonl");
        let mobile = dir.path().join("mobile.jsonl");

        let mut f = std::fs::File::create(&desktop).unwrap();
        writeln!(f, r#"{{"sessionId":"old-id","text":"hello"}}"#).unwrap();

        let mut offset = 0;
        let stats = sync_cycle(
            AgentKind::Claude,
            SyncDirection::DesktopToMobile,
            &desktop,
            &mobile,
            &mut offset,
            "new-id",
        )
        .unwrap();

        assert_eq!(stats.copied, 1);
        assert!(offset > 0);

        let mobile_content = std::fs::read_to_string(&mobile).unwrap();
        assert!(mobile_content.contains(r#""sessionId":"new-id""#));
        assert!(mobile_content.contains(r#""agentBusSync""#));
    }

    #[test]
    fn test_sync_skips_already_synced_lines_to_prevent_loop() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = dir.path().join("desktop.jsonl");
        let mobile = dir.path().join("mobile.jsonl");

        let mut f = std::fs::File::create(&desktop).unwrap();
        // Line that was originally synced FROM mobile -> desktop
        writeln!(
            f,
            r#"{{"sessionId":"desk","text":"hi","agentBusSync":{{"from":"mobile"}}}}"#
        )
        .unwrap();

        let mut offset = 0;
        let stats = sync_cycle(
            AgentKind::Claude,
            SyncDirection::DesktopToMobile,
            &desktop,
            &mobile,
            &mut offset,
            "mob",
        )
        .unwrap();

        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.copied, 0);
    }

    #[test]
    fn test_sync_holds_back_partial_trailing_line() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = dir.path().join("desktop.jsonl");
        let mobile = dir.path().join("mobile.jsonl");

        let mut f = std::fs::File::create(&desktop).unwrap();
        write!(f, r#"{{"sessionId":"id","text":"incomplete..."#).unwrap();

        let mut offset = 0;
        let stats = sync_cycle(
            AgentKind::Claude,
            SyncDirection::DesktopToMobile,
            &desktop,
            &mobile,
            &mut offset,
            "id",
        )
        .unwrap();

        assert_eq!(stats.copied, 0);
        assert_eq!(offset, 0);
    }

    #[test]
    fn test_sync_mobile_to_desktop_advances_offset() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = dir.path().join("desktop.jsonl");
        let mobile = dir.path().join("mobile.jsonl");

        let mut f = std::fs::File::create(&mobile).unwrap();
        writeln!(f, r#"{{"sessionId":"mobile-id","text":"hello desktop"}}"#).unwrap();

        let mut offset = 0;
        let stats = sync_cycle(
            AgentKind::Claude,
            SyncDirection::MobileToDesktop,
            &mobile,
            &desktop,
            &mut offset,
            "desktop-id",
        )
        .unwrap();

        assert_eq!(stats.copied, 1);
        assert!(offset > 0);

        let desktop_content = std::fs::read_to_string(&desktop).unwrap();
        assert!(desktop_content.contains(r#""sessionId":"desktop-id""#));
        assert!(desktop_content.contains(r#""from":"mobile""#));
        assert!(desktop_content.contains(r#""sourceSessionId":"mobile-id""#));
    }

    #[test]
    fn test_sync_rewrites_codex_session_meta_payload_id() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = dir.path().join("desktop.jsonl");
        let mobile = dir.path().join("mobile.jsonl");

        let mut f = std::fs::File::create(&desktop).unwrap();
        writeln!(
            f,
            r#"{{"type":"session_meta","payload":{{"id":"desktop-id","cwd":"/repo"}}}}"#
        )
        .unwrap();

        let mut offset = 0;
        let stats = sync_cycle(
            AgentKind::Codex,
            SyncDirection::DesktopToMobile,
            &desktop,
            &mobile,
            &mut offset,
            "mobile-id",
        )
        .unwrap();

        assert_eq!(stats.copied, 1);
        let mobile_content = std::fs::read_to_string(&mobile).unwrap();
        assert!(mobile_content.contains(r#""id":"mobile-id""#));
        assert!(mobile_content.contains(r#""sourceSessionId":"desktop-id""#));
    }

    #[tokio::test]
    async fn wait_for_codex_desktop_reply_returns_last_agent_message_on_task_complete() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = dir.path().join("desktop.jsonl");
        {
            let mut f = std::fs::File::create(&desktop).unwrap();
            writeln!(f, r#"{{"type":"session_meta","payload":{{"id":"s1"}}}}"#).unwrap();
        }
        let offset = std::fs::metadata(&desktop).unwrap().len();
        {
            let mut f = OpenOptions::new().append(true).open(&desktop).unwrap();
            writeln!(
                f,
                r#"{{"type":"event_msg","payload":{{"type":"agent_message","message":"first reply"}}}}"#
            )
            .unwrap();
            writeln!(
                f,
                r#"{{"type":"event_msg","payload":{{"type":"agent_message","message":"final reply"}}}}"#
            )
            .unwrap();
            writeln!(
                f,
                r#"{{"type":"event_msg","payload":{{"type":"task_complete"}}}}"#
            )
            .unwrap();
        }

        let reply =
            wait_for_codex_desktop_reply(desktop, offset, std::time::Duration::from_secs(1))
                .await
                .unwrap();
        assert_eq!(reply, "final reply");
    }

    #[tokio::test]
    async fn wait_for_codex_desktop_reply_reads_task_complete_last_agent_message() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = dir.path().join("desktop.jsonl");
        let offset = 0;
        {
            let mut f = std::fs::File::create(&desktop).unwrap();
            writeln!(
                f,
                r#"{{"type":"event_msg","payload":{{"type":"task_complete","last_agent_message":"done from payload"}}}}"#
            )
            .unwrap();
        }

        let reply =
            wait_for_codex_desktop_reply(desktop, offset, std::time::Duration::from_secs(1))
                .await
                .unwrap();
        assert_eq!(reply, "done from payload");
    }

    #[cfg(unix)]
    #[test]
    fn test_sync_bridged_session_rejects_symlink_path() {
        let dir = tempfile::tempdir().unwrap();
        let real_desktop = dir.path().join("desktop-real.jsonl");
        let desktop_link = dir.path().join("desktop-link.jsonl");
        let mobile = dir.path().join("mobile.jsonl");

        let mut f = std::fs::File::create(&real_desktop).unwrap();
        writeln!(f, r#"{{"sessionId":"desktop-id","text":"hello"}}"#).unwrap();
        std::os::unix::fs::symlink(&real_desktop, &desktop_link).unwrap();
        std::fs::File::create(&mobile).unwrap();

        let mut bridge = BridgedSessionState {
            agent: AgentKind::Claude.to_string(),
            repo_id: "repo".to_string(),
            desktop_session_id: "desktop-id".to_string(),
            desktop_path: desktop_link.display().to_string(),
            mobile_session_id: "mobile-id".to_string(),
            mobile_path: mobile.display().to_string(),
            selected_at: "2026-04-19T00:00:00Z".to_string(),
            sync: agent_bus_core::state::SessionSyncCursor {
                desktop_offset: 0,
                mobile_offset: 0,
                last_synced_at: None,
                last_error: None,
            },
            display_name: None,
        };

        let err = sync_bridged_session(&mut bridge).unwrap_err();
        assert!(
            err.to_string().contains("symlink"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_sync_all_bridged_sessions_once_persists_offsets_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let state = agent_bus_core::state::spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();

        let good_desktop = dir.path().join("good-desktop.jsonl");
        let good_mobile = dir.path().join("good-mobile.jsonl");
        {
            let mut f = std::fs::File::create(&good_desktop).unwrap();
            writeln!(f, r#"{{"sessionId":"desk","text":"from desktop"}}"#).unwrap();
        }
        std::fs::File::create(&good_mobile).unwrap();

        state
            .set_bridged_session(
                "chat-good",
                AgentKind::Claude.to_string(),
                BridgedSessionState {
                    agent: AgentKind::Claude.to_string(),
                    repo_id: "repo".to_string(),
                    desktop_session_id: "desk".to_string(),
                    desktop_path: good_desktop.display().to_string(),
                    mobile_session_id: "mob".to_string(),
                    mobile_path: good_mobile.display().to_string(),
                    selected_at: "2026-04-19T00:00:00Z".to_string(),
                    sync: agent_bus_core::state::SessionSyncCursor {
                        desktop_offset: 0,
                        mobile_offset: 0,
                        last_synced_at: None,
                        last_error: None,
                    },
                    display_name: None,
                },
            )
            .await
            .unwrap();
        state
            .set_bridged_session(
                "chat-bad",
                AgentKind::Claude.to_string(),
                BridgedSessionState {
                    agent: AgentKind::Claude.to_string(),
                    repo_id: "repo".to_string(),
                    desktop_session_id: "missing-desk".to_string(),
                    desktop_path: dir
                        .path()
                        .join("missing-desktop.jsonl")
                        .display()
                        .to_string(),
                    mobile_session_id: "missing-mob".to_string(),
                    mobile_path: dir
                        .path()
                        .join("missing-mobile.jsonl")
                        .display()
                        .to_string(),
                    selected_at: "2026-04-19T00:00:00Z".to_string(),
                    sync: agent_bus_core::state::SessionSyncCursor {
                        desktop_offset: 0,
                        mobile_offset: 0,
                        last_synced_at: None,
                        last_error: None,
                    },
                    display_name: None,
                },
            )
            .await
            .unwrap();

        let stats = sync_all_bridged_sessions_once(state.clone()).await;

        assert_eq!(stats.sessions, 2);
        assert_eq!(stats.copied, 1);
        assert_eq!(stats.errors, 1);
        let snapshot = state.snapshot().await;
        let good = &snapshot.bridged_sessions["chat-good"]["claude"];
        let bad = &snapshot.bridged_sessions["chat-bad"]["claude"];
        assert!(good.sync.desktop_offset > 0);
        assert!(good.sync.last_synced_at.is_some());
        assert_eq!(good.sync.last_error, None);
        assert!(bad.sync.last_error.is_some());
    }

    #[test]
    fn test_sync_bridged_session_gemini_is_noop() {
        let mut bridge = BridgedSessionState {
            agent: AgentKind::Gemini.to_string(),
            repo_id: "repo".to_string(),
            desktop_session_id: "g-1".to_string(),
            desktop_path: String::new(),
            mobile_session_id: "g-1".to_string(),
            mobile_path: String::new(),
            selected_at: "2026-04-19T00:00:00Z".to_string(),
            sync: agent_bus_core::state::SessionSyncCursor {
                desktop_offset: 0,
                mobile_offset: 0,
                last_synced_at: None,
                last_error: None,
            },
            display_name: None,
        };

        let stats = sync_bridged_session(&mut bridge).unwrap();
        assert_eq!(stats, BridgeSyncStats::default());
        assert!(bridge.sync.last_synced_at.is_some());
        assert_eq!(bridge.sync.last_error, None);
    }

    #[test]
    fn test_detect_codex_sessions_filters_by_repo_and_sorts_recent_first() {
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join("codex");
        let sessions_dir = codex_home.join("sessions/2026/04/19");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let older = sessions_dir.join("rollout-old.jsonl");
        let newer = sessions_dir.join("rollout-new.jsonl");
        let other_repo = sessions_dir.join("rollout-other.jsonl");
        std::fs::write(
            &older,
            r#"{"type":"session_meta","payload":{"id":"old","cwd":"/repo","timestamp":"2026-04-19T10:00:00Z"}}"#,
        )
        .unwrap();
        std::fs::write(
            &newer,
            r#"{"type":"session_meta","payload":{"id":"new","cwd":"/repo/","timestamp":"2026-04-19T11:00:00Z"}}"#,
        )
        .unwrap();
        filetime::set_file_mtime(&older, filetime::FileTime::from_unix_time(1_700_000_000, 0))
            .unwrap();
        filetime::set_file_mtime(&newer, filetime::FileTime::from_unix_time(1_700_000_100, 0))
            .unwrap();
        std::fs::write(
            &other_repo,
            r#"{"type":"session_meta","payload":{"id":"other","cwd":"/elsewhere"}}"#,
        )
        .unwrap();

        let sessions = detect_codex_sessions(&codex_home, "/repo", 10).unwrap();

        assert_eq!(
            sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["new", "old"]
        );
    }

    #[test]
    fn test_detect_codex_sessions_uses_session_index_title() {
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join("codex");
        let sessions_dir = codex_home.join("sessions/2026/04/19");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            codex_home.join("session_index.jsonl"),
            r#"{"id":"session-a","thread_name":"Huong dan repo Telegram","updated_at":"2026-04-19T10:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(
            sessions_dir.join("rollout-session-a.jsonl"),
            concat!(
                r#"{"type":"session_meta","payload":{"id":"session-a","cwd":"/repo","timestamp":"2026-04-19T10:00:00Z"}}"#,
                "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fallback prompt"}]}}"#
            ),
        )
        .unwrap();

        let sessions = detect_codex_sessions(&codex_home, "/repo", 10).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].title.as_deref(),
            Some("Huong dan repo Telegram")
        );
    }

    #[test]
    fn test_parse_list_commands() {
        assert_eq!(
            parse_bridge_command("/list_claude"),
            Some(BridgeCommand::List(AgentKind::Claude))
        );
        assert_eq!(
            parse_bridge_command("/ls_cl_ses"),
            Some(BridgeCommand::List(AgentKind::Claude))
        );
        assert_eq!(
            parse_bridge_command("/list_codex"),
            Some(BridgeCommand::List(AgentKind::Codex))
        );
        assert_eq!(
            parse_bridge_command("/list_gemini"),
            Some(BridgeCommand::List(AgentKind::Gemini))
        );
        assert_eq!(
            parse_bridge_command("/list_antigravity"),
            Some(BridgeCommand::List(AgentKind::Antigravity))
        );
        assert_eq!(parse_bridge_command("/list_antigravity_all"), None);
        assert_eq!(parse_bridge_command("@list_codex"), None);
    }

    #[test]
    fn test_parse_flush_commands() {
        assert_eq!(
            parse_bridge_command("@flush_claude"),
            Some(BridgeCommand::Flush(AgentKind::Claude))
        );
        assert_eq!(
            parse_bridge_command("@flush_mobile"),
            Some(BridgeCommand::Flush(AgentKind::Claude))
        );
        assert_eq!(
            parse_bridge_command("@flush_codex"),
            Some(BridgeCommand::Flush(AgentKind::Codex))
        );
        assert_eq!(
            parse_bridge_command("@flush_gemini"),
            Some(BridgeCommand::Flush(AgentKind::Gemini))
        );
        assert_eq!(
            parse_bridge_command("@flush_antigravity"),
            Some(BridgeCommand::Flush(AgentKind::Antigravity))
        );
    }

    #[test]
    fn test_parse_chat_commands() {
        assert_eq!(
            parse_bridge_command("@claude hello world"),
            Some(BridgeCommand::Chat(
                AgentKind::Claude,
                "hello world".to_string()
            ))
        );
        assert_eq!(
            parse_bridge_command("@codex list files"),
            Some(BridgeCommand::Chat(
                AgentKind::Codex,
                "list files".to_string()
            ))
        );
        assert_eq!(
            parse_bridge_command("@gemini explain this"),
            Some(BridgeCommand::Chat(
                AgentKind::Gemini,
                "explain this".to_string()
            ))
        );
        assert_eq!(
            parse_bridge_command("@antigravity explain this"),
            Some(BridgeCommand::Chat(
                AgentKind::Antigravity,
                "explain this".to_string()
            ))
        );
    }

    #[test]
    fn test_parse_ignores_inbox_routing() {
        assert_eq!(parse_bridge_command("@codex:repo hello"), None);
        assert_eq!(parse_bridge_command("@claude:repo hello"), None);
        assert_eq!(parse_bridge_command("@gemini:repo hello"), None);
        assert_eq!(parse_bridge_command("@antigravity:repo hello"), None);
    }

    #[test]
    fn test_parse_callback_data() {
        assert_eq!(
            parse_callback_data("sel_claude:uuid123"),
            Some((AgentKind::Claude, "uuid123".to_string()))
        );
        assert_eq!(
            parse_callback_data("sel_codex:hash456"),
            Some((AgentKind::Codex, "hash456".to_string()))
        );
        assert_eq!(
            parse_callback_data("sel_gemini:uuid789"),
            Some((AgentKind::Gemini, "uuid789".to_string()))
        );
        assert_eq!(
            parse_callback_data("sel_antigravity:uuid999"),
            Some((AgentKind::Antigravity, "uuid999".to_string()))
        );
        assert_eq!(parse_callback_data("other:data"), None);
    }

    #[test]
    fn test_parse_gemini_list_sessions() {
        let sessions = parse_gemini_list_sessions(
            "Available sessions for this project (2):\n  1. Hi there (3 minutes ago) [abc-123]\n  2. Another task (2 hours ago) [def-456]\n",
            10,
        );
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "abc-123");
        assert_eq!(sessions[0].title.as_deref(), Some("Hi there"));
        assert_eq!(sessions[0].repo_path, None);
        assert_eq!(sessions[1].id, "def-456");
        assert_eq!(sessions[1].title.as_deref(), Some("Another task"));
    }

    #[test]
    fn test_detect_antigravity_gemini_sessions_filters_repo_and_decodes_title() {
        let _guard = ANTIGRAVITY_OVERRIDE_TEST_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.vscdb");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();

        let entry = proto_field(
            1,
            &proto_message([
                proto_field(1, b"abc12345-1234-1234-1234-1234567890ab"),
                proto_field(
                    2,
                    base64::engine::general_purpose::STANDARD
                        .encode(proto_message([proto_field(
                            1,
                            b"Testing SparkUp Extension Workflow",
                        )]))
                        .as_bytes(),
                ),
                proto_field(
                    5,
                    &proto_message([proto_field(
                        1,
                        b"file:///home/john-chuong/Projects/tele-agent-bus",
                    )]),
                ),
            ]),
        );
        let other_entry = proto_field(
            1,
            &proto_message([
                proto_field(1, b"def12345-1234-1234-1234-1234567890ab"),
                proto_field(2, b"Different Repo Session"),
                proto_field(
                    5,
                    &proto_message([proto_field(1, b"file:///tmp/other-repo")]),
                ),
            ]),
        );
        let blob = [entry, other_entry].concat();
        let encoded = base64::engine::general_purpose::STANDARD.encode(blob);
        conn.execute(
            "INSERT INTO ItemTable(key, value) VALUES (?1, ?2)",
            ("antigravityUnifiedStateSync.trajectorySummaries", encoded),
        )
        .unwrap();

        set_antigravity_state_db_override(Some(db_path));
        set_antigravity_brain_root_override(Some(dir.path().join("brain-empty")));
        let sessions =
            detect_antigravity_sessions(Some("/home/john-chuong/Projects/tele-agent-bus"), 10)
                .unwrap();
        set_antigravity_state_db_override(None);
        set_antigravity_brain_root_override(None);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "abc12345-1234-1234-1234-1234567890ab");
        assert_eq!(
            sessions[0].title.as_deref(),
            Some("Testing SparkUp Extension Workflow")
        );
        assert_eq!(
            sessions[0].repo_path.as_deref(),
            Some("/home/john-chuong/Projects/tele-agent-bus")
        );
    }

    fn proto_field(field: u64, data: &[u8]) -> Vec<u8> {
        let mut out = encode_varint((field << 3) | 2);
        out.extend(encode_varint(data.len() as u64));
        out.extend_from_slice(data);
        out
    }

    fn proto_message<const N: usize>(fields: [Vec<u8>; N]) -> Vec<u8> {
        fields.into_iter().flatten().collect()
    }

    fn encode_varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
        out
    }
}
