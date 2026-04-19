use agent_bus_core::auth_context::AuthContextsConfig;
use agent_bus_core::state::{
    AuthContextStatusKind, PendingRotation, PendingRotationStatus, StateHandle,
};
use crate::daemon::telegram::{BotClient, InlineKeyboard, MessageRef, TelegramError};

/// 4a.5: handle /quota <agent>
pub async fn handle_quota_command<B: BotClient + ?Sized>(
    bot: &B,
    state: StateHandle,
    cfg: &AuthContextsConfig,
    chat_id: i64,
    agent: &str,
) -> Result<(), TelegramError> {
    let contexts = cfg.contexts_for(agent);
    if contexts.is_empty() {
        bot.send_message(
            chat_id,
            format!("No auth contexts configured for {agent}"),
            None,
        )
        .await?;
        return Ok(());
    }

    let snapshot = state.snapshot().await;
    let active_id = snapshot.active_auth_context.get(agent);
    let agent_statuses = snapshot.auth_context_status.get(agent);

    let mut text = format!("/quota {agent}");
    for ctx in contexts {
        let status_str = if let Some(status) = agent_statuses.and_then(|m| m.get(&ctx.id)) {
            let mut s = match status.status {
                AuthContextStatusKind::Available => "available".to_string(),
                AuthContextStatusKind::QuotaExhausted => "quota_exhausted".to_string(),
                AuthContextStatusKind::RateLimited => "rate_limited".to_string(),
                AuthContextStatusKind::AuthExpired => "auth_expired".to_string(),
                AuthContextStatusKind::ManualReauthRequired => "reauth_required".to_string(),
                AuthContextStatusKind::Disabled => "disabled".to_string(),
                AuthContextStatusKind::UnknownFailure => "failed".to_string(),
            };
            if let Some(until) = &status.cooldown_until {
                s.push_str("  cooldown until ");
                s.push_str(until);
            }
            s
        } else {
            "available".to_string()
        };

        let marker = if active_id == Some(&ctx.id) {
            " (active)"
        } else {
            ""
        };

        text.push_str(&format!("\n{:<10} {} {}", ctx.id, status_str, marker));
    }

    bot.send_message(chat_id, text, None).await?;
    Ok(())
}

/// 4a.5: handle /auth_list
pub async fn handle_auth_list_command<B: BotClient + ?Sized>(
    bot: &B,
    cfg: &AuthContextsConfig,
    chat_id: i64,
) -> Result<(), TelegramError> {
    if cfg.agents.is_empty() {
        bot.send_message(chat_id, "No auth contexts configured".to_string(), None)
            .await?;
        return Ok(());
    }

    let mut text = String::new();
    for (agent, contexts) in &cfg.agents {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("{}:", agent));
        for ctx in contexts {
            text.push_str(&format!("\n  {:<8} {}", ctx.id, ctx.profile_dir.display()));
            if ctx.auto_rotate {
                text.push_str("  auto_rotate=true");
            }
            if ctx.require_owner_approval {
                text.push_str("  require_approval=true");
            }
        }
    }

    bot.send_message(chat_id, text, None).await?;
    Ok(())
}

/// 4a.5: handle /auth_rotate <agent>
pub async fn handle_auth_rotate_command<B: BotClient + ?Sized>(
    bot: &B,
    state: StateHandle,
    cfg: &AuthContextsConfig,
    chat_id: i64,
    agent: &str,
) -> Result<(), TelegramError> {
    let contexts: Vec<_> = cfg
        .contexts_for(agent)
        .iter()
        .filter(|c| c.enabled)
        .collect();
    if contexts.is_empty() {
        bot.send_message(
            chat_id,
            format!("No enabled auth contexts for {agent}"),
            None,
        )
        .await?;
        return Ok(());
    }

    let snapshot = state.snapshot().await;
    let current_id = snapshot.active_auth_context.get(agent);

    let next_ctx = if let Some(cid) = current_id {
        let pos = contexts.iter().position(|c| &c.id == cid);
        match pos {
            Some(i) => contexts[(i + 1) % contexts.len()],
            None => contexts[0],
        }
    } else {
        contexts[0]
    };

    let old = current_id.map(|s| s.as_str()).unwrap_or("none");
    if next_ctx.require_owner_approval {
        let rotation_id = generate_rotation_id();
        let rot = PendingRotation {
            id: rotation_id.clone(),
            agent: agent.to_string(),
            from: old.to_string(),
            to: next_ctx.id.clone(),
            repo_id: snapshot
                .default_repo_by_chat
                .get(&chat_id.to_string())
                .cloned()
                .unwrap_or_else(|| "none".to_string()),
            request_id: format!("tg_{}", rotation_id),
            chat_id,
            expires_at: now_plus_hour_rfc3339(),
            status: PendingRotationStatus::Pending,
        };
        state.insert_pending_rotation(rot).await?;

        let text = format!(
            "Rotation requested for {agent}: {old} -> {new}\nRequires approval.",
            new = next_ctx.id
        );
        let keyboard = InlineKeyboard {
            rows: vec![vec![
                ("Approve".to_string(), format!("rot:approve:{}", rotation_id)),
                ("Deny".to_string(), format!("rot:deny:{}", rotation_id)),
            ]],
        };
        bot.send_message(chat_id, text, Some(keyboard)).await?;
    } else {
        state.set_active_auth_context(agent, &next_ctx.id).await?;
        bot.send_message(
            chat_id,
            format!("Rotated {agent}: {old} -> {new}", new = next_ctx.id),
            None,
        )
        .await?;
    }
    Ok(())
}

/// 4a.6: handle rot:approve:<id> and rot:deny:<id>
pub async fn handle_callback_rotation<B: BotClient + ?Sized>(
    bot: &B,
    state: StateHandle,
    message: MessageRef,
    callback_id: String,
    callback_data: String,
) -> Result<(), TelegramError> {
    let (action, rot_id) = callback_data
        .strip_prefix("rot:")
        .and_then(|rest| rest.split_once(':'))
        .ok_or_else(|| TelegramError::InvalidCallback(callback_data.clone()))?;

    let snapshot = state.snapshot().await;
    let rotation = snapshot.pending_rotations.get(rot_id).ok_or_else(|| {
        TelegramError::InvalidCallback(format!("unknown rotation {}", rot_id))
    })?;

    if rotation.status != PendingRotationStatus::Pending {
        bot.answer_callback(callback_id, "Rotation already resolved".to_string())
            .await?;
        return Ok(());
    }

    match action {
        "approve" => {
            state
                .resolve_pending_rotation(rot_id, PendingRotationStatus::Approved)
                .await?;
            state
                .set_active_auth_context(&rotation.agent, &rotation.to)
                .await?;
            bot.edit_message_text(
                message,
                format!("Approved rotation: {} -> {}", rotation.from, rotation.to),
            )
            .await?;
            bot.answer_callback(callback_id, "Approved".to_string())
                .await?;
        }
        "deny" => {
            state
                .resolve_pending_rotation(rot_id, PendingRotationStatus::Denied)
                .await?;
            bot.edit_message_text(message, format!("Denied rotation to {}", rotation.to))
                .await?;
            bot.answer_callback(callback_id, "Denied".to_string())
                .await?;
        }
        _ => return Err(TelegramError::InvalidCallback(callback_data)),
    }
    Ok(())
}

fn generate_rotation_id() -> String {
    use rand::{distributions::Alphanumeric, Rng};
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(8)
        .map(char::from)
        .collect::<String>()
        .to_lowercase()
}

fn now_plus_hour_rfc3339() -> String {
    let now = time::OffsetDateTime::now_utc();
    let later = now + time::Duration::hours(1);
    later
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "2026-04-18T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::telegram::MockBot;
    use agent_bus_core::state::{spawn_state_actor, AuthContextStatus};
    use std::path::Path;
    use tempfile::tempdir;

    fn test_cfg() -> AuthContextsConfig {
        let yaml = r#"
version: 1
agents:
  claude:
    contexts:
      - id: john
        profile_dir: ~/.agent-bus/auth/claude/john
      - id: partner
        profile_dir: ~/.agent-bus/auth/claude/partner
        require_owner_approval: true
  codex:
    contexts:
      - id: main
        profile_dir: ~/.agent-bus/auth/codex/main
"#;
        AuthContextsConfig::parse(yaml, Path::new("/home/user")).unwrap()
    }

    #[tokio::test]
    async fn quota_shows_status_table() {
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let cfg = test_cfg();

        state
            .set_auth_context_status(
                "claude",
                "partner",
                AuthContextStatus {
                    status: AuthContextStatusKind::QuotaExhausted,
                    cooldown_until: Some("2026-04-18T10:30:00Z".to_string()),
                    last_event_id: None,
                    updated_at: "now".to_string(),
                },
            )
            .await
            .unwrap();
        state
            .set_active_auth_context("claude", "john")
            .await
            .unwrap();

        handle_quota_command(&bot, state, &cfg, 100, "claude")
            .await
            .unwrap();

        let sent = bot.sent_messages();
        assert_eq!(sent.len(), 1);
        let text = &sent[0].text;
        assert!(text.contains("john"));
        assert!(text.contains("available"));
        assert!(text.contains("(active)"));
        assert!(text.contains("partner"));
        assert!(text.contains("quota_exhausted"));
        assert!(text.contains("cooldown until 2026-04-18T10:30:00Z"));
    }

    #[tokio::test]
    async fn auth_list_formats_all_agents() {
        let bot = MockBot::default();
        let cfg = test_cfg();

        handle_auth_list_command(&bot, &cfg, 100).await.unwrap();

        let sent = bot.sent_messages();
        assert_eq!(sent.len(), 1);
        let text = &sent[0].text;
        assert!(text.contains("claude:"));
        assert!(text.contains("john"));
        assert!(text.contains("partner"));
        assert!(text.contains("codex:"));
        assert!(text.contains("main"));
        assert!(text.contains("require_approval=true"));
    }

    #[tokio::test]
    async fn rotate_switches_active_context() {
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let cfg = test_cfg();

        // Rotate codex: none -> main
        handle_auth_rotate_command(&bot, state.clone(), &cfg, 100, "codex")
            .await
            .unwrap();
        let snap = state.snapshot().await;
        // Debugging
        if snap.active_auth_context.get("codex") != Some(&"main".to_string()) {
             println!("DEBUG: active_auth_context: {:?}", snap.active_auth_context);
        }
        assert_eq!(snap.active_auth_context.get("codex").map(|s| s.as_str()), Some("main"));

        // Rotate claude: none -> john (no approval req for john)
        handle_auth_rotate_command(&bot, state.clone(), &cfg, 100, "claude")
            .await
            .unwrap();
        let snap = state.snapshot().await;
        assert_eq!(snap.active_auth_context.get("claude").map(|s| s.as_str()), Some("john"));

        // Rotate claude: john -> partner (approval required)
        handle_auth_rotate_command(&bot, state.clone(), &cfg, 100, "claude")
            .await
            .unwrap();
        let snap = state.snapshot().await;
        assert_eq!(snap.active_auth_context.get("claude").map(|s| s.as_str()), Some("john")); // Still john
        assert_eq!(snap.pending_rotations.len(), 1);
        let rot = snap.pending_rotations.values().next().unwrap();
        assert_eq!(rot.to, "partner");
    }

    #[tokio::test]
    async fn callback_rotation_approve_updates_state() {
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();

        let rot = PendingRotation {
            id: "rot123".to_string(),
            agent: "claude".to_string(),
            from: "john".to_string(),
            to: "partner".to_string(),
            repo_id: "rallyup".to_string(),
            request_id: "req1".to_string(),
            chat_id: 100,
            expires_at: "future".to_string(),
            status: PendingRotationStatus::Pending,
        };
        state.insert_pending_rotation(rot).await.unwrap();

        handle_callback_rotation(
            &bot,
            state.clone(),
            MessageRef {
                chat_id: 100,
                message_id: 1,
            },
            "cb1".to_string(),
            "rot:approve:rot123".to_string(),
        )
        .await
        .unwrap();

        let snap = state.snapshot().await;
        assert_eq!(
            snap.pending_rotations["rot123"].status,
            PendingRotationStatus::Approved
        );
        assert_eq!(snap.active_auth_context.get("claude").map(|s| s.as_str()), Some("partner"));
        assert_eq!(bot.edited_messages().len(), 1);
        assert!(bot.edited_messages()[0].text.contains("Approved"));
    }
}
