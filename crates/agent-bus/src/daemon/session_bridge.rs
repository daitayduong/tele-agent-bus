use agent_bus_core::auth_context::AgentKind;
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::Write;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeCommand {
    List(AgentKind),
    Chat(AgentKind, String),
    Flush(AgentKind),
}

pub fn parse_bridge_command(text: &str) -> Option<BridgeCommand> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    // AC-SB2: Generic Command Parser
    // @list_claude
    // @ls_cl_ses (legacy alias)
    // @flush_claude
    // @flush_mobile (legacy alias)
    // @claude hi

    if trimmed == "@list_claude" || trimmed == "@ls_cl_ses" {
        return Some(BridgeCommand::List(AgentKind::Claude));
    }
    if trimmed == "@list_codex" {
        return Some(BridgeCommand::List(AgentKind::Codex));
    }
    if trimmed == "@flush_claude" || trimmed == "@flush_mobile" {
        return Some(BridgeCommand::Flush(AgentKind::Claude));
    }
    if trimmed == "@flush_codex" {
        return Some(BridgeCommand::Flush(AgentKind::Codex));
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

    None
}

pub fn parse_callback_data(data: &str) -> Option<(AgentKind, String)> {
    if let Some(id) = data.strip_prefix("sel_claude:") {
        return Some((AgentKind::Claude, id.to_string()));
    }
    if let Some(id) = data.strip_prefix("sel_codex:") {
        return Some((AgentKind::Codex, id.to_string()));
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDirection {
    DesktopToMobile,
    MobileToDesktop,
}

pub struct SyncStats {
    pub copied: usize,
    pub skipped: usize,
    pub errors: usize,
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
        let source_session_id = value
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
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
    }
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
    fn test_parse_list_commands() {
        assert_eq!(
            parse_bridge_command("@list_claude"),
            Some(BridgeCommand::List(AgentKind::Claude))
        );
        assert_eq!(
            parse_bridge_command("@ls_cl_ses"),
            Some(BridgeCommand::List(AgentKind::Claude))
        );
        assert_eq!(
            parse_bridge_command("@list_codex"),
            Some(BridgeCommand::List(AgentKind::Codex))
        );
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
    }

    #[test]
    fn test_parse_ignores_inbox_routing() {
        assert_eq!(parse_bridge_command("@codex:repo hello"), None);
        assert_eq!(parse_bridge_command("@claude:repo hello"), None);
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
        assert_eq!(parse_callback_data("other:data"), None);
    }
}
