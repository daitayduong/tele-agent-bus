use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;

use crate::daemon::claude_headless;
use crate::daemon::mobile_session::{
    self, MobileCommand, SessionInfo, ARCHIVE_RETENTION, MOBILE_UUID,
};
use crate::daemon::routing::{Routed, RoutingError, RoutingParser};
use crate::daemon::auth_cmds;
use agent_bus_core::auth_context::AuthContextsConfig;
use agent_bus_core::state::{MobileSessionState, PendingPermStatus, StateHandle};
use teloxide::payloads::{AnswerCallbackQuerySetters, SendMessageSetters};
use teloxide::prelude::{Requester, ResponseResult};
use teloxide::types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MessageId};
use thiserror::Error;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramConfig {
    pub allowed_chats: Vec<String>,
    pub repos: Vec<RepoEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct RepoEntry {
    pub id: String,
    pub display: String,
    pub path: String,
    #[serde(default)]
    pub agents: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineKeyboard {
    pub rows: Vec<Vec<(String, String)>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageRef {
    pub chat_id: i64,
    pub message_id: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
pub struct SentMessage {
    pub chat_id: i64,
    pub text: String,
    pub keyboard: Option<InlineKeyboard>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
pub struct EditedMessage {
    pub message: MessageRef,
    pub text: String,
}

#[derive(Debug, Error)]
pub enum TelegramError {
    #[error("telegram send failed: {0}")]
    Send(String),
    #[error("unknown repo: {0}")]
    UnknownRepo(String),
    #[error("invalid callback data: {0}")]
    InvalidCallback(String),
    #[error(transparent)]
    State(#[from] agent_bus_core::state::StateError),
    #[error(transparent)]
    Inbox(#[from] anyhow::Error),
}

pub trait BotClient: Send + Sync {
    fn send_message<'a>(
        &'a self,
        chat_id: i64,
        text: String,
        keyboard: Option<InlineKeyboard>,
    ) -> BoxFuture<'a, Result<MessageRef, TelegramError>>;

    fn edit_message_text<'a>(
        &'a self,
        message: MessageRef,
        text: String,
    ) -> BoxFuture<'a, Result<(), TelegramError>>;

    fn answer_callback<'a>(
        &'a self,
        callback_id: String,
        text: String,
    ) -> BoxFuture<'a, Result<(), TelegramError>>;
}

pub async fn handle_list_rp_command<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    chat_id: i64,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }

    let snapshot = state.snapshot().await;
    let current = snapshot.default_repo_by_chat.get(&chat_id.to_string());
    let mut text = match current.and_then(|id| repo_by_id(config, id)) {
        Some(repo) => format!("Registered repos (chat default = {})", repo.display),
        None => "Registered repos (chat default = none)".to_string(),
    };

    for repo in &config.repos {
        let marker = if current.is_some_and(|id| id == &repo.id) {
            "* "
        } else {
            "- "
        };
        text.push('\n');
        text.push_str(marker);
        text.push_str(&repo.display);
    }

    let keyboard = InlineKeyboard {
        rows: config
            .repos
            .iter()
            .map(|repo| {
                let label = if current.is_some_and(|id| id == &repo.id) {
                    format!("{} *", repo.display)
                } else {
                    repo.display.clone()
                };
                vec![(label, format!("switch:{}", repo.id))]
            })
            .collect(),
    };

    bot.send_message(chat_id, text, Some(keyboard)).await?;
    Ok(())
}

pub async fn handle_switch_rp_command<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    chat_id: i64,
    repo_id: String,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }

    let repo =
        repo_by_id(config, &repo_id).ok_or_else(|| TelegramError::UnknownRepo(repo_id.clone()))?;
    state
        .set_default_repo(chat_id.to_string(), repo.id.clone())
        .await?;
    bot.send_message(
        chat_id,
        format!("Default repo set to {}", repo.display),
        None,
    )
    .await?;
    Ok(())
}

pub async fn handle_callback_switch<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    chat_id: i64,
    message: MessageRef,
    callback_id: String,
    callback_data: String,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }

    let repo_id = callback_data
        .strip_prefix("switch:")
        .ok_or_else(|| TelegramError::InvalidCallback(callback_data.clone()))?;
    let repo = repo_by_id(config, repo_id)
        .ok_or_else(|| TelegramError::UnknownRepo(repo_id.to_string()))?;

    state
        .set_default_repo(chat_id.to_string(), repo.id.clone())
        .await?;
    bot.edit_message_text(message, format!("Default -> {}", repo.display))
        .await?;
    bot.answer_callback(callback_id, format!("Switched to {}", repo.display))
        .await
}

pub async fn handle_current_command<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    chat_id: i64,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }

    let snapshot = state.snapshot().await;
    let text = match snapshot
        .default_repo_by_chat
        .get(&chat_id.to_string())
        .and_then(|id| repo_by_id(config, id))
    {
        Some(repo) => format!("Current default repo: {}", repo.display),
        None => "Current default repo: none".to_string(),
    };
    bot.send_message(chat_id, text, None).await?;
    Ok(())
}

pub async fn handle_text_command<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    auth_contexts: &Option<AuthContextsConfig>,
    chat_id: i64,
    username: Option<&str>,
    text: &str,
) -> Result<(), TelegramError> {
    if let Some(cmd) = mobile_session::parse_mobile_command(text) {
        return handle_mobile_command(bot, config, state, chat_id, cmd).await;
    }

    if text.trim_start().starts_with('@') {
        return handle_routed_message(bot, config, state, chat_id, username, text).await;
    }

    let mut parts = text.split_whitespace();
    match parts.next() {
        Some("/list_rp") => handle_list_rp_command(bot, config, state, chat_id).await,
        Some("/current") => handle_current_command(bot, config, state, chat_id).await,
        Some("/switch_rp") => {
            let Some(repo_id) = parts.next() else {
                if is_allowed(config, chat_id) {
                    bot.send_message(chat_id, "Usage: /switch_rp <repo_id>".to_string(), None)
                        .await?;
                }
                return Ok(());
            };
            handle_switch_rp_command(bot, config, state, chat_id, repo_id.to_string()).await
        }
        Some("/auth_list") => {
            if let Some(cfg) = auth_contexts {
                auth_cmds::handle_auth_list_command(bot, state, cfg, chat_id).await
            } else {
                bot.send_message(chat_id, "Auth contexts not configured (legacy mode)".to_string(), None).await?;
                Ok(())
            }
        }
        Some("/quota") => {
            let Some(agent) = parts.next() else {
                bot.send_message(chat_id, "Usage: /quota <agent>".to_string(), None).await?;
                return Ok(());
            };
            if let Some(cfg) = auth_contexts {
                auth_cmds::handle_quota_command(bot, state, cfg, chat_id, agent).await
            } else {
                bot.send_message(chat_id, "Auth contexts not configured (legacy mode)".to_string(), None).await?;
                Ok(())
            }
        }
        Some("/auth_rotate") => {
            let Some(agent) = parts.next() else {
                bot.send_message(chat_id, "Usage: /auth_rotate <agent>".to_string(), None).await?;
                return Ok(());
            };
            if let Some(cfg) = auth_contexts {
                auth_cmds::handle_auth_rotate_command(bot, state, cfg, chat_id, agent).await
            } else {
                bot.send_message(chat_id, "Auth contexts not configured (legacy mode)".to_string(), None).await?;
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

async fn handle_mobile_command<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    chat_id: i64,
    cmd: MobileCommand,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }
    match cmd {
        MobileCommand::ListClaude => handle_list_claude_command(bot, config, state, chat_id).await,
        MobileCommand::ClaudeMsg(body) => {
            handle_claude_mobile_msg(bot, config, state, chat_id, body).await
        }
        MobileCommand::FlushMobile => {
            bot.send_message(
                chat_id,
                "@flush_mobile — not implemented yet (Phase 3.5).".to_string(),
                None,
            )
            .await?;
            Ok(())
        }
    }
}

pub async fn handle_list_claude_command<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    chat_id: i64,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }

    let snapshot = state.snapshot().await;
    let Some(repo_id) = snapshot.default_repo_by_chat.get(&chat_id.to_string()) else {
        bot.send_message(
            chat_id,
            "No default repo. Use /switch_rp <id> first.".to_string(),
            None,
        )
        .await?;
        return Ok(());
    };
    let Some(repo) = repo_by_id(config, repo_id) else {
        return Err(TelegramError::UnknownRepo(repo_id.clone()));
    };

    let project_dir = claude_project_dir(&repo.path);
    let sessions = match mobile_session::detect_active_sessions(
        &project_dir,
        MOBILE_UUID,
        DEFAULT_MTIME_THRESHOLD_SECS,
    ) {
        Ok(s) => s,
        Err(err) => {
            bot.send_message(chat_id, format!("Failed to scan sessions: {err}"), None)
                .await?;
            return Ok(());
        }
    };

    if sessions.is_empty() {
        bot.send_message(
            chat_id,
            format!(
                "No active desktop sessions for {} (scanned {}).",
                repo.display,
                project_dir.display()
            ),
            None,
        )
        .await?;
        return Ok(());
    }

    let rows = mobile_session::build_session_cards(&sessions);
    let text = format!("Active desktop sessions ({}):", repo.display);
    bot.send_message(chat_id, text, Some(InlineKeyboard { rows }))
        .await?;
    Ok(())
}

pub async fn handle_callback_sel_claude<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    chat_id: i64,
    message: MessageRef,
    callback_id: String,
    callback_data: String,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }

    let Some(desktop_uuid) = mobile_session::parse_callback_data(&callback_data) else {
        return Err(TelegramError::InvalidCallback(callback_data));
    };

    let snapshot = state.snapshot().await;
    let Some(repo_id) = snapshot.default_repo_by_chat.get(&chat_id.to_string()) else {
        bot.answer_callback(callback_id, "No default repo".to_string())
            .await?;
        return Ok(());
    };
    let Some(repo) = repo_by_id(config, repo_id) else {
        return Err(TelegramError::UnknownRepo(repo_id.clone()));
    };

    let project_dir = claude_project_dir(&repo.path);
    let source_path = project_dir.join(format!("{}.jsonl", desktop_uuid));
    let target_path = project_dir.join(format!("{}.jsonl", MOBILE_UUID));

    let sessions = match mobile_session::detect_active_sessions(
        &project_dir,
        MOBILE_UUID,
        DEFAULT_MTIME_THRESHOLD_SECS,
    ) {
        Ok(s) => s,
        Err(err) => {
            bot.answer_callback(callback_id, short_err("Scan failed", &err.to_string()))
                .await?;
            return Ok(());
        }
    };
    let Some(session) = sessions.iter().find(|s| s.uuid == desktop_uuid) else {
        bot.answer_callback(callback_id, "Session not found".to_string())
            .await?;
        return Ok(());
    };

    if !cwd_matches_repo(&session.cwd, &repo.path) {
        bot.edit_message_text(
            message,
            format!(
                "⚠️ Session cwd {} does not match repo {}. Cannot fork across projects.",
                session.cwd, repo.path
            ),
        )
        .await?;
        bot.answer_callback(callback_id, "cwd mismatch".to_string())
            .await?;
        return Ok(());
    }

    let archive_dir = agents_archive_dir(&repo.path);
    if let Err(err) = std::fs::create_dir_all(&archive_dir) {
        bot.answer_callback(
            callback_id,
            short_err("archive dir error", &err.to_string()),
        )
        .await?;
        return Ok(());
    }
    if target_path.exists() {
        let ts = timestamp_tag();
        let archive_target = archive_dir.join(format!("mobile-{}.jsonl", ts));
        if let Err(err) = std::fs::rename(&target_path, &archive_target) {
            bot.answer_callback(
                callback_id,
                short_err("archive rename failed", &err.to_string()),
            )
            .await?;
            return Ok(());
        }
    }

    if let Some(dir) = target_path.parent() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            let prefix = format!(
                "{}.tmp.",
                target_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
            );
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with(&prefix) {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }

    let fork_stats = match mobile_session::fork_session(&source_path, &target_path, MOBILE_UUID) {
        Ok(s) => s,
        Err(err) => {
            bot.answer_callback(callback_id, short_err("fork failed", &err.to_string()))
                .await?;
            return Ok(());
        }
    };

    if let Err(err) = mobile_session::append_fork_marker(&source_path, MOBILE_UUID) {
        tracing::warn!(target: "agent_bus::mobile", error = %err, "append_fork_marker failed");
    }

    if let Err(err) = mobile_session::rotate_archives(&archive_dir, ARCHIVE_RETENTION) {
        tracing::warn!(target: "agent_bus::mobile", error = %err, "rotate_archives failed");
    }

    let now = time::OffsetDateTime::now_utc();
    let forked_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| now.unix_timestamp().to_string());
    let project_hash = project_hash_for_repo(&repo.path);
    let mobile = MobileSessionState {
        mobile_uuid: MOBILE_UUID.to_string(),
        mobile_fork_source: desktop_uuid.clone(),
        mobile_forked_at: forked_at,
        project_hash,
        repo_id: repo.id.clone(),
    };
    state
        .set_mobile_session(chat_id.to_string(), mobile)
        .await?;

    let title = session
        .ai_title
        .clone()
        .unwrap_or_else(|| desktop_uuid.get(..8).unwrap_or(&desktop_uuid).to_string());
    let confirm = format!(
        "✅ Mobile forked from \"{}\" ({} lines rewritten). Send @claude <msg> to continue.",
        title, fork_stats.lines_rewritten
    );
    bot.edit_message_text(message, confirm).await?;
    bot.answer_callback(callback_id, "Forked".to_string())
        .await?;
    Ok(())
}

pub async fn handle_claude_mobile_msg<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    chat_id: i64,
    body: String,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }

    let snapshot = state.snapshot().await;
    let Some(mobile) = snapshot.mobile_sessions.get(&chat_id.to_string()) else {
        bot.send_message(
            chat_id,
            "⚠️ No mobile session. Send @list_claude first to pick a desktop session.".to_string(),
            None,
        )
        .await?;
        return Ok(());
    };
    let Some(repo) = repo_by_id(config, &mobile.repo_id) else {
        return Err(TelegramError::UnknownRepo(mobile.repo_id.clone()));
    };

    let claude_bin = claude_bin_path();
    let cwd = PathBuf::from(&repo.path);
    let timeout_secs = claude_headless::resolved_timeout_secs();

    bot.send_message(
        chat_id,
        format!("⏳ thinking... (timeout {}s)", timeout_secs),
        None,
    )
    .await
    .ok();

    let reply = match claude_headless::spawn_claude_resume(
        &claude_bin,
        &cwd,
        &mobile.mobile_uuid,
        &body,
        timeout_secs,
    )
    .await
    {
        Ok(out) => out,
        Err(err) => {
            bot.send_message(chat_id, format!("❌ claude failed: {err}"), None)
                .await?;
            return Ok(());
        }
    };

    let trimmed = reply.trim();
    if trimmed.is_empty() {
        bot.send_message(chat_id, "(empty reply from claude)".to_string(), None)
            .await?;
        return Ok(());
    }

    const MAX_CHUNK: usize = 4000;
    for chunk in claude_headless::chunk_for_telegram(trimmed, MAX_CHUNK) {
        bot.send_message(chat_id, chunk, None).await?;
    }
    Ok(())
}

const DEFAULT_MTIME_THRESHOLD_SECS: u64 = 30 * 60;

fn claude_project_dir(repo_path: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(project_hash_for_repo(repo_path))
}

fn project_hash_for_repo(repo_path: &str) -> String {
    repo_path.replace('/', "-")
}

fn agents_archive_dir(repo_path: &str) -> PathBuf {
    PathBuf::from(repo_path).join(".agents").join("archive")
}

fn cwd_matches_repo(session_cwd: &str, repo_path: &str) -> bool {
    let normalize = |s: &str| s.trim_end_matches('/').to_string();
    normalize(session_cwd) == normalize(repo_path)
}

fn timestamp_tag() -> String {
    let now = time::OffsetDateTime::now_utc();
    now.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| now.unix_timestamp().to_string())
        .replace(':', "-")
}

fn claude_bin_path() -> String {
    std::env::var("AGENT_BUS_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string())
}

fn short_err(label: &str, err: &str) -> String {
    const MAX: usize = 180;
    let sanitized: String = err
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let head: String = sanitized.chars().take(MAX).collect();
    format!("{}: {}", label, head)
}

#[allow(dead_code)]
fn _session_info_dummy() -> SessionInfo {
    SessionInfo {
        uuid: String::new(),
        cwd: String::new(),
        ai_title: None,
        first_prompt: None,
        last_modified: time::OffsetDateTime::UNIX_EPOCH,
        turn_count: 0,
    }
}

pub async fn handle_routed_message<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    chat_id: i64,
    username: Option<&str>,
    text: &str,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }

    let snapshot = state.snapshot().await;
    let default_repo = snapshot
        .default_repo_by_chat
        .get(&chat_id.to_string())
        .map(String::as_str);
    let routed = match RoutingParser::parse(text, default_repo) {
        Ok(routed) => routed,
        Err(err) => {
            log_routing_rejected(&err, text);
            if err != RoutingError::NoMatch {
                bot.send_message(chat_id, routing_error_message(&err).to_string(), None)
                    .await?;
            }
            return Ok(());
        }
    };

    let Some(repo) = repo_by_id(config, &routed.repo) else {
        log_routing_rejected_reason("unknown_repo", text);
        bot.send_message(
            chat_id,
            format!("Unknown agent or repo: {}", text.trim_start()),
            None,
        )
        .await?;
        return Ok(());
    };
    if !repo.agents.iter().any(|agent| agent == &routed.agent) {
        log_routing_rejected_reason("unknown_agent", text);
        bot.send_message(
            chat_id,
            format!("Unknown agent or repo: {}", text.trim_start()),
            None,
        )
        .await?;
        return Ok(());
    }

    write_routed(repo, &routed, username.unwrap_or("unknown"))?;
    bot.send_message(
        chat_id,
        format!("✓ routed to {}@{}", routed.agent, routed.repo),
        None,
    )
    .await?;
    Ok(())
}

fn write_routed(repo: &RepoEntry, routed: &Routed, username: &str) -> Result<(), TelegramError> {
    crate::daemon::inbox::append_inbox(
        Path::new(&repo.path),
        &routed.agent,
        username,
        &routed.body,
    )?;
    Ok(())
}

fn routing_error_message(err: &RoutingError) -> &'static str {
    match err {
        RoutingError::NoMatch => "",
        RoutingError::NoDefaultRepo => {
            "No default repo for this chat. Use /switch_rp <id> or @agent:repo msg"
        }
        RoutingError::InvalidAgentName => "Invalid agent name",
        RoutingError::InvalidRepoName => "Invalid repo name",
        RoutingError::EmptyBody => "Empty message body",
        RoutingError::MessageTooLong => "Message too long",
    }
}

fn log_routing_rejected(err: &RoutingError, raw: &str) {
    log_routing_rejected_reason(err.reason(), raw);
}

fn log_routing_rejected_reason(reason: &'static str, raw: &str) {
    let raw_snippet = raw.chars().take(80).collect::<String>();
    tracing::warn!(
        target: "agent_bus::routing",
        reason = %reason,
        raw_snippet,
        "routing_rejected"
    );
}

pub async fn send_perm_prompt<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    repo_id: &str,
    perm_id: &str,
    command_hash: &str,
    matched_pattern: &str,
) -> Result<Option<MessageRef>, TelegramError> {
    let Some(chat_id) = config
        .allowed_chats
        .first()
        .and_then(|chat| chat.parse::<i64>().ok())
    else {
        return Ok(None);
    };

    let repo = repo_by_id(config, repo_id)
        .map(|repo| repo.display.as_str())
        .unwrap_or(repo_id);
    let text = format!(
        "Permission requested\nRepo: {repo}\nCommand: {command_hash}\nMatched: {matched_pattern}"
    );
    let keyboard = InlineKeyboard {
        rows: vec![vec![
            ("Approve".to_string(), format!("perm:approve:{perm_id}")),
            ("Deny".to_string(), format!("perm:deny:{perm_id}")),
        ]],
    };

    bot.send_message(chat_id, text, Some(keyboard))
        .await
        .map(Some)
}

pub async fn handle_callback_perm<B: BotClient + ?Sized>(
    bot: &B,
    state: StateHandle,
    registry: crate::daemon::perm::PendingPermRegistry,
    message: MessageRef,
    callback_id: String,
    callback_data: String,
    user_name: Option<String>,
) -> Result<(), TelegramError> {
    let (action, perm_id) = callback_data
        .strip_prefix("perm:")
        .and_then(|rest| rest.split_once(':'))
        .ok_or_else(|| TelegramError::InvalidCallback(callback_data.clone()))?;

    let (status, verdict, text) = match action {
        "approve" => (
            PendingPermStatus::Approved,
            crate::daemon::perm::PermVerdict::Approve,
            "Approved",
        ),
        "deny" => (
            PendingPermStatus::Denied,
            crate::daemon::perm::PermVerdict::Deny,
            "Denied",
        ),
        _ => return Err(TelegramError::InvalidCallback(callback_data)),
    };

    registry.resolve(perm_id, verdict).await;
    state.resolve_pending(perm_id.to_string(), status).await?;

    let snapshot = state.snapshot().await;
    if snapshot
        .pending_perms
        .get(perm_id)
        .and_then(|perm| perm.message_id)
        == Some(message.message_id)
    {
        let actor = user_name.unwrap_or_else(|| "user".to_string());
        bot.edit_message_text(message, format!("{text} by @{actor}"))
            .await?;
    }

    bot.answer_callback(callback_id, text.to_string()).await
}

fn is_allowed(config: &TelegramConfig, chat_id: i64) -> bool {
    config
        .allowed_chats
        .iter()
        .any(|allowed| allowed == &chat_id.to_string())
}

fn repo_by_id<'a>(config: &'a TelegramConfig, repo_id: &str) -> Option<&'a RepoEntry> {
    config.repos.iter().find(|repo| repo.id == repo_id)
}

#[derive(Debug, Clone, Default)]
#[cfg(test)]
pub struct MockBot {
    sent: Arc<Mutex<Vec<SentMessage>>>,
    edited: Arc<Mutex<Vec<EditedMessage>>>,
    callbacks: Arc<Mutex<Vec<String>>>,
}

#[cfg(test)]
impl MockBot {
    pub fn sent_messages(&self) -> Vec<SentMessage> {
        self.sent.lock().expect("mock bot lock poisoned").clone()
    }

    pub fn edited_messages(&self) -> Vec<EditedMessage> {
        self.edited.lock().expect("mock bot lock poisoned").clone()
    }
}

#[cfg(test)]
impl BotClient for MockBot {
    fn send_message<'a>(
        &'a self,
        chat_id: i64,
        text: String,
        keyboard: Option<InlineKeyboard>,
    ) -> BoxFuture<'a, Result<MessageRef, TelegramError>> {
        Box::pin(async move {
            self.sent
                .lock()
                .expect("mock bot lock poisoned")
                .push(SentMessage {
                    chat_id,
                    text,
                    keyboard,
                });
            Ok(MessageRef {
                chat_id,
                message_id: self.sent.lock().expect("mock bot lock poisoned").len() as i32,
            })
        })
    }

    fn edit_message_text<'a>(
        &'a self,
        message: MessageRef,
        text: String,
    ) -> BoxFuture<'a, Result<(), TelegramError>> {
        Box::pin(async move {
            self.edited
                .lock()
                .expect("mock bot lock poisoned")
                .push(EditedMessage { message, text });
            Ok(())
        })
    }

    fn answer_callback<'a>(
        &'a self,
        callback_id: String,
        _text: String,
    ) -> BoxFuture<'a, Result<(), TelegramError>> {
        Box::pin(async move {
            self.callbacks
                .lock()
                .expect("mock bot lock poisoned")
                .push(callback_id);
            Ok(())
        })
    }
}

#[derive(Clone)]
pub struct TeloxideBotClient {
    bot: teloxide::Bot,
}

impl TeloxideBotClient {
    pub fn new(bot: teloxide::Bot) -> Self {
        Self { bot }
    }
}

impl BotClient for TeloxideBotClient {
    fn send_message<'a>(
        &'a self,
        chat_id: i64,
        text: String,
        keyboard: Option<InlineKeyboard>,
    ) -> BoxFuture<'a, Result<MessageRef, TelegramError>> {
        Box::pin(async move {
            let mut request = self.bot.send_message(ChatId(chat_id), text);
            if let Some(keyboard) = keyboard {
                request = request.reply_markup(to_teloxide_keyboard(keyboard));
            }
            request
                .await
                .map(|message| MessageRef {
                    chat_id,
                    message_id: message.id.0,
                })
                .map_err(|err| TelegramError::Send(err.to_string()))
        })
    }

    fn edit_message_text<'a>(
        &'a self,
        message: MessageRef,
        text: String,
    ) -> BoxFuture<'a, Result<(), TelegramError>> {
        Box::pin(async move {
            self.bot
                .edit_message_text(ChatId(message.chat_id), MessageId(message.message_id), text)
                .await
                .map(|_| ())
                .map_err(|err| TelegramError::Send(err.to_string()))
        })
    }

    fn answer_callback<'a>(
        &'a self,
        callback_id: String,
        text: String,
    ) -> BoxFuture<'a, Result<(), TelegramError>> {
        Box::pin(async move {
            self.bot
                .answer_callback_query(callback_id)
                .text(text)
                .await
                .map(|_| ())
                .map_err(|err| TelegramError::Send(err.to_string()))
        })
    }
}

pub async fn teloxide_message_handler(
    bot: teloxide::Bot,
    msg: teloxide::types::Message,
    config: Arc<TelegramConfig>,
    state: StateHandle,
    auth_contexts: Arc<Option<AuthContextsConfig>>,
) -> ResponseResult<()> {
    let client = TeloxideBotClient::new(bot);
    if let Some(text) = msg.text() {
        let username = msg.from.as_ref().and_then(|user| user.username.as_deref());
        handle_text_command(&client, &config, state, &auth_contexts, msg.chat.id.0, username, text)
            .await
            .map_err(to_teloxide_error)?;
    }
    Ok(())
}

pub async fn teloxide_callback_handler(
    bot: teloxide::Bot,
    query: teloxide::types::CallbackQuery,
    config: Arc<TelegramConfig>,
    state: StateHandle,
    auth_contexts: Arc<Option<AuthContextsConfig>>,
    registry: crate::daemon::perm::PendingPermRegistry,
) -> ResponseResult<()> {
    let client = TeloxideBotClient::new(bot);
    let Some(data) = query.data else {
        return Ok(());
    };
    let Some(message) = query.message else {
        return Ok(());
    };
    let chat = message.chat();
    let message_id = message.id();

    if data.starts_with("switch:") {
        handle_callback_switch(
            &client,
            &config,
            state,
            chat.id.0,
            MessageRef {
                chat_id: chat.id.0,
                message_id: message_id.0,
            },
            query.id,
            data,
        )
        .await
        .map_err(to_teloxide_error)?;
    } else if data.starts_with("sel_claude:") {
        handle_callback_sel_claude(
            &client,
            &config,
            state,
            chat.id.0,
            MessageRef {
                chat_id: chat.id.0,
                message_id: message_id.0,
            },
            query.id,
            data,
        )
        .await
        .map_err(to_teloxide_error)?;
    } else if data.starts_with("perm:") {
        let user_name = query
            .from
            .username
            .clone()
            .or_else(|| Some(query.from.id.0.to_string()));
        handle_callback_perm(
            &client,
            state,
            registry,
            MessageRef {
                chat_id: chat.id.0,
                message_id: message_id.0,
            },
            query.id,
            data,
            user_name,
        )
        .await
        .map_err(to_teloxide_error)?;
    } else if data.starts_with("rot:") {
        if let Some(_cfg) = auth_contexts.as_ref() {
            auth_cmds::handle_callback_rotation(
                &client,
                state,
                MessageRef {
                    chat_id: chat.id.0,
                    message_id: message_id.0,
                },
                query.id,
                data,
            )
            .await
            .map_err(to_teloxide_error)?;
        }
    }
    Ok(())
}

fn to_teloxide_keyboard(keyboard: InlineKeyboard) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(keyboard.rows.into_iter().map(|row| {
        row.into_iter()
            .map(|(label, data)| InlineKeyboardButton::callback(label, data))
            .collect::<Vec<_>>()
    }))
}

fn to_teloxide_error(err: TelegramError) -> teloxide::RequestError {
    teloxide::RequestError::Io(std::io::Error::other(err.to_string()))
}

#[cfg(test)]
mod mobile_tests {
    use super::*;
    use agent_bus_core::state::spawn_state_actor;
    use tempfile::tempdir;

    fn test_config() -> TelegramConfig {
        TelegramConfig {
            allowed_chats: vec!["100".to_string()],
            repos: vec![RepoEntry {
                id: "rallyup".to_string(),
                display: "RallyUp".to_string(),
                path: "/tmp/rallyup-test".to_string(),
                agents: vec!["claude".to_string()],
            }],
        }
    }

    #[tokio::test]
    async fn list_claude_with_no_default_repo_warns() {
        let bot = MockBot::default();
        let config = test_config();
        let dir = tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();

        handle_text_command(&bot, &config, state, &None, 100, None, "@list_claude")
            .await
            .unwrap();

        let sent = bot.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].text.to_lowercase().contains("no default repo"));
    }

    #[tokio::test]
    async fn claude_msg_without_mobile_session_sends_guard_warning() {
        let bot = MockBot::default();
        let config = test_config();
        let dir = tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();

        handle_text_command(&bot, &config, state, &None, 100, None, "@claude hello world")
            .await
            .unwrap();

        let sent = bot.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(
            sent[0].text.contains("No mobile session"),
            "expected guard message, got: {:?}",
            sent[0].text
        );
    }

    #[tokio::test]
    async fn unauthorized_chat_is_ignored_for_mobile_commands() {
        let bot = MockBot::default();
        let config = test_config();
        let dir = tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();

        handle_text_command(&bot, &config, state, &None, 999, None, "@list_claude")
            .await
            .unwrap();

        assert!(bot.sent_messages().is_empty());
    }

    #[tokio::test]
    async fn claude_mobile_msg_uses_stored_repo_and_uuid() {
        let bot = MockBot::default();
        let config = test_config();
        let dir = tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "rallyup").await.unwrap();

        handle_text_command(&bot, &config, state, &None, 100, None, "@claude hi")
            .await
            .unwrap();

        let sent = bot.sent_messages();
        assert!(sent[0].text.contains("No mobile session"));
    }

    #[tokio::test]
    async fn cwd_matches_repo_normalizes_trailing_slash() {
        assert!(cwd_matches_repo("/a/b", "/a/b"));
        assert!(cwd_matches_repo("/a/b/", "/a/b"));
        assert!(cwd_matches_repo("/a/b", "/a/b/"));
        assert!(!cwd_matches_repo("/a/b/c", "/a/b"));
    }

    #[tokio::test]
    async fn project_hash_replaces_slashes() {
        assert_eq!(
            project_hash_for_repo("/home/user/Projects/RallyUp"),
            "-home-user-Projects-RallyUp"
        );
    }
}
