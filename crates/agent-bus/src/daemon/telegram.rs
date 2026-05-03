use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
use std::time::Duration;

use crate::daemon::auth_cmds;
use crate::daemon::claude_headless;
use crate::daemon::codex_app_server;
use crate::daemon::codex_ipc::{CodexIpcClient, CodexIpcError};
use crate::daemon::mobile_session::{self, MobileCommand, SessionInfo, MOBILE_UUID};
use crate::daemon::routing::{Routed, RoutingError, RoutingParser};
use crate::daemon::runner::{AgentRunMode, AgentRunRequest, RunnerError, SharedAgentRunner};
use crate::daemon::session_bridge::{self, BridgeCommand, BridgeSyncStats};
use agent_bus_core::auth_context::{AgentKind, AuthContextsConfig, LeadSource};
use agent_bus_core::state::{
    BridgedSessionState, PendingPermStatus, ResolvePendingOutcome, SessionSyncCursor, StateHandle,
};
use teloxide::payloads::{AnswerCallbackQuerySetters, SendMessageSetters};
use teloxide::prelude::{Requester, ResponseResult};
use teloxide::types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MessageId};
use thiserror::Error;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[cfg(test)]
static CODEX_HOME_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

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
    #[serde(default)]
    pub codex_mode: CodexMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CodexMode {
    #[default]
    LiveBridge,
    AppServer,
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

pub async fn handle_switch_rp_picker<B: BotClient + ?Sized>(
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

async fn handle_help_command<B: BotClient + ?Sized>(
    bot: &B,
    chat_id: i64,
) -> Result<(), TelegramError> {
    let keyboard = InlineKeyboard {
        rows: vec![
            vec![
                ("Current Repo".to_string(), "help:current".to_string()),
                ("Switch Repo".to_string(), "help:switch_rp".to_string()),
            ],
            vec![
                ("Auth List".to_string(), "help:auth_list".to_string()),
                ("Quota".to_string(), "help:quota".to_string()),
            ],
            vec![
                ("Auth Rotate".to_string(), "help:auth_rotate".to_string()),
                ("Lead".to_string(), "help:lead".to_string()),
            ],
            vec![
                ("Lead Default".to_string(), "help:lead_default".to_string()),
                ("Lead Clear".to_string(), "help:lead_clear".to_string()),
            ],
            vec![
                ("List Claude".to_string(), "help:list_claude".to_string()),
                ("List Codex".to_string(), "help:list_codex".to_string()),
            ],
            vec![
                ("List Gemini".to_string(), "help:list_gemini".to_string()),
                (
                    "List Antigravity".to_string(),
                    "help:list_antigravity".to_string(),
                ),
            ],
            vec![
                (
                    "Antigravity Models".to_string(),
                    "help:models_antigravity".to_string(),
                ),
                (
                    "Set Antigravity Model".to_string(),
                    "help:set_model_antigravity".to_string(),
                ),
            ],
        ],
    };
    bot.send_message(chat_id, "Available commands:".to_string(), Some(keyboard))
        .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_text_command<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    auth_contexts: &Option<AuthContextsConfig>,
    chat_id: i64,
    username: Option<&str>,
    text: &str,
    agent_runner: Option<
        &Arc<crate::daemon::runner::AgentRunner<crate::daemon::cli_spawner::CliSpawner>>,
    >,
) -> Result<(), TelegramError> {
    if let Some(cmd) = mobile_session::parse_mobile_command(text) {
        return handle_mobile_command(bot, config, state, chat_id, cmd, agent_runner).await;
    }

    if let Some(cmd) = session_bridge::parse_bridge_command(text) {
        return handle_bridge_command(bot, config, state, chat_id, cmd, agent_runner).await;
    }

    if text.trim_start().starts_with('@') {
        return handle_routed_message(bot, config, state, chat_id, username, text).await;
    }

    let mut parts = text.split_whitespace();
    match parts.next() {
        Some("/current") => handle_current_command(bot, config, state, chat_id).await,
        Some("/switch_rp") => {
            let Some(repo_id) = parts.next() else {
                return handle_switch_rp_picker(bot, config, state, chat_id).await;
            };
            handle_switch_rp_command(bot, config, state, chat_id, repo_id.to_string()).await
        }
        Some("/auth_list") => {
            if let Some(cfg) = auth_contexts {
                auth_cmds::handle_auth_list_command(bot, state, cfg, chat_id).await
            } else {
                bot.send_message(
                    chat_id,
                    "Auth contexts not configured (legacy mode)".to_string(),
                    None,
                )
                .await?;
                Ok(())
            }
        }
        Some("/quota") => {
            let Some(agent) = parts.next() else {
                bot.send_message(chat_id, "Usage: /quota <agent>".to_string(), None)
                    .await?;
                return Ok(());
            };
            if let Some(cfg) = auth_contexts {
                auth_cmds::handle_quota_command(bot, state, cfg, chat_id, agent).await
            } else {
                bot.send_message(
                    chat_id,
                    "Auth contexts not configured (legacy mode)".to_string(),
                    None,
                )
                .await?;
                Ok(())
            }
        }
        Some("/auth_rotate") => {
            let Some(agent) = parts.next() else {
                bot.send_message(chat_id, "Usage: /auth_rotate <agent>".to_string(), None)
                    .await?;
                return Ok(());
            };
            if let Some(cfg) = auth_contexts {
                auth_cmds::handle_auth_rotate_command(bot, state, cfg, chat_id, agent).await
            } else {
                bot.send_message(
                    chat_id,
                    "Auth contexts not configured (legacy mode)".to_string(),
                    None,
                )
                .await?;
                Ok(())
            }
        }
        Some("/lead") => {
            if !is_allowed(config, chat_id) {
                return Ok(());
            }
            if let Some(cfg) = auth_contexts {
                auth_cmds::handle_lead_command(bot, state, cfg, chat_id, parts.next()).await
            } else {
                handle_legacy_lead_command(bot, state, chat_id, parts.next()).await
            }
        }
        Some("/lead_default") => {
            if !is_allowed(config, chat_id) {
                return Ok(());
            }
            let Some(agent) = parts.next() else {
                bot.send_message(chat_id, "Usage: /lead_default <agent>".to_string(), None)
                    .await?;
                return Ok(());
            };
            auth_cmds::handle_lead_default_command(bot, state, chat_id, agent).await
        }
        Some("/lead_clear") => {
            if !is_allowed(config, chat_id) {
                return Ok(());
            }
            auth_cmds::handle_lead_clear_command(bot, state, chat_id).await
        }
        Some("/help") => handle_help_command(bot, chat_id).await,
        Some("/set_model_antigravity") => {
            handle_set_model_antigravity_command(bot, state, chat_id, parts.next()).await
        }
        Some("/models_antigravity") => {
            handle_list_antigravity_models_command(bot, config, state, chat_id).await
        }
        Some(cmd) if cmd.starts_with('/') => Ok(()),
        _ => {
            handle_unaddressed_lead_message(
                bot,
                config,
                state,
                auth_contexts,
                chat_id,
                text,
                agent_runner,
            )
            .await
        }
    }
}

struct AgentRunInput {
    repo_id: String,
    repo_path: PathBuf,
    prompt: String,
    mode: AgentRunMode,
    timeout: Duration,
    chat_id: i64,
}

async fn handle_legacy_lead_command<B: BotClient + ?Sized>(
    bot: &B,
    state: StateHandle,
    chat_id: i64,
    agent: Option<&str>,
) -> Result<(), TelegramError> {
    if let Some(agent) = agent {
        if !matches!(agent, "claude" | "codex" | "gemini" | "antigravity") {
            bot.send_message(
                chat_id,
                "Usage: agent must be one of claude, codex, gemini, antigravity".to_string(),
                None,
            )
            .await?;
            return Ok(());
        }
        state.set_lead_for_chat(chat_id.to_string(), agent).await?;
        bot.send_message(chat_id, format!("Lead for this chat set to {agent}"), None)
            .await?;
        return Ok(());
    }

    let snapshot = state.snapshot().await;
    let (agent, source) = snapshot
        .lead_overrides
        .per_chat
        .get(&chat_id.to_string())
        .map(|agent| (agent.as_str(), "per_chat"))
        .or_else(|| {
            snapshot
                .lead_overrides
                .default
                .as_deref()
                .map(|agent| (agent, "default"))
        })
        .unwrap_or(("claude", "default"));
    bot.send_message(chat_id, format!("Lead: {agent}\nsource: {source}"), None)
        .await?;
    Ok(())
}

async fn handle_unaddressed_lead_message<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    auth_contexts: &Option<AuthContextsConfig>,
    chat_id: i64,
    text: &str,
    agent_runner: Option<
        &Arc<crate::daemon::runner::AgentRunner<crate::daemon::cli_spawner::CliSpawner>>,
    >,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }

    let (agent, source) = resolve_lead_for_chat(state.clone(), auth_contexts, chat_id).await;
    tracing::info!(
        target: "agent_bus::lead",
        chat_id = %chat_id,
        source = ?source,
        agent = %agent,
        "lead_resolved"
    );

    let snapshot = state.snapshot().await;
    let mobile = snapshot.mobile_sessions.get(&chat_id.to_string()).cloned();
    if lead_has_selected_session(&snapshot, chat_id, agent) {
        let addressed = format!("@{} {}", agent, text.trim());
        if let Some(cmd) = mobile_session::parse_mobile_command(&addressed) {
            return handle_mobile_command(bot, config, state, chat_id, cmd, agent_runner).await;
        }
        if let Some(cmd) = session_bridge::parse_bridge_command(&addressed) {
            return handle_bridge_command(bot, config, state, chat_id, cmd, agent_runner).await;
        }
    }

    let Some(runner) = agent_runner else {
        bot.send_message(
            chat_id,
            "Auth contexts not configured (legacy mode)".to_string(),
            None,
        )
        .await?;
        return Ok(());
    };

    let repo_id = mobile.as_ref().map(|m| m.repo_id.as_str()).or_else(|| {
        snapshot
            .default_repo_by_chat
            .get(&chat_id.to_string())
            .map(String::as_str)
    });
    let Some(repo_id) = repo_id else {
        bot.send_message(
            chat_id,
            "No default repo for this chat. Use /switch_rp first.".to_string(),
            None,
        )
        .await?;
        return Ok(());
    };
    let Some(repo) = repo_by_id(config, repo_id) else {
        return Err(TelegramError::UnknownRepo(repo_id.to_string()));
    };

    if !repo.agents.iter().any(|allowed| allowed == agent.as_str()) {
        bot.send_message(
            chat_id,
            format!("Agent {} is not enabled for repo {}", agent, repo.id),
            None,
        )
        .await?;
        return Ok(());
    }

    let timeout_secs = claude_headless::resolved_timeout_secs();
    bot.send_message(
        chat_id,
        format!("⏳ thinking... (timeout {}s)", timeout_secs),
        None,
    )
    .await
    .ok();

    let (mode, runner_session_label) = match mobile {
        Some(mobile) => {
            let label = mobile.mobile_uuid[..mobile.mobile_uuid.len().min(8)].to_string();
            (
                AgentRunMode::WithMobileContext {
                    mobile_uuid: mobile.mobile_uuid,
                },
                label,
            )
        }
        None => (AgentRunMode::Fresh, "new".to_string()),
    };
    let reply = run_agent_via_runner(
        runner,
        agent,
        AgentRunInput {
            repo_id: repo.id.clone(),
            repo_path: PathBuf::from(&repo.path),
            prompt: text.trim().to_string(),
            mode,
            timeout: Duration::from_secs(timeout_secs),
            chat_id,
        },
    )
    .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(msg) => {
            bot.send_message(chat_id, msg, None).await?;
            return Ok(());
        }
    };
    let trimmed = reply.trim();
    if trimmed.is_empty() {
        bot.send_message(chat_id, format!("(empty reply from {agent})"), None)
            .await?;
        return Ok(());
    }
    let agent_display = match agent {
        AgentKind::Claude => "Claude",
        AgentKind::Codex => "Codex",
        AgentKind::Gemini => "Gemini",
        AgentKind::Antigravity => "Antigravity",
    };
    let header = agent_header(agent_display, &runner_session_label);
    for chunk in claude_headless::chunk_for_telegram(&format!("{header}{trimmed}"), 4000) {
        bot.send_message(chat_id, chunk, None).await?;
    }
    Ok(())
}

fn lead_has_selected_session(
    snapshot: &agent_bus_core::state::StateSnapshot,
    chat_id: i64,
    agent: AgentKind,
) -> bool {
    let chat_key = chat_id.to_string();
    match agent {
        AgentKind::Claude => {
            snapshot
                .bridged_sessions
                .get(&chat_key)
                .and_then(|by_agent| by_agent.get("claude"))
                .is_some()
                || snapshot.mobile_sessions.contains_key(&chat_key)
        }
        AgentKind::Codex => snapshot
            .bridged_sessions
            .get(&chat_key)
            .and_then(|by_agent| by_agent.get("codex"))
            .is_some(),
        AgentKind::Gemini => snapshot
            .bridged_sessions
            .get(&chat_key)
            .and_then(|by_agent| by_agent.get("gemini"))
            .is_some(),
        AgentKind::Antigravity => snapshot
            .bridged_sessions
            .get(&chat_key)
            .and_then(|by_agent| by_agent.get("antigravity"))
            .is_some(),
    }
}

async fn resolve_lead_for_chat(
    state: StateHandle,
    auth_contexts: &Option<AuthContextsConfig>,
    chat_id: i64,
) -> (AgentKind, LeadSource) {
    let snapshot = state.snapshot().await;
    if let Some(agent) = snapshot
        .lead_overrides
        .per_chat
        .get(&chat_id.to_string())
        .and_then(|agent| agent.parse::<AgentKind>().ok())
    {
        return (agent, LeadSource::OverridePerChat);
    }
    if let Some(agent) = snapshot
        .lead_overrides
        .default
        .as_deref()
        .and_then(|agent| agent.parse::<AgentKind>().ok())
    {
        return (agent, LeadSource::OverrideDefault);
    }
    auth_contexts
        .as_ref()
        .map(|cfg| cfg.resolve_lead(Some(&chat_id.to_string())))
        .unwrap_or((AgentKind::Claude, LeadSource::Legacy))
}

async fn handle_mobile_command<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    chat_id: i64,
    cmd: MobileCommand,
    agent_runner: Option<
        &Arc<crate::daemon::runner::AgentRunner<crate::daemon::cli_spawner::CliSpawner>>,
    >,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }
    match cmd {
        MobileCommand::ListClaude => handle_list_claude_command(bot, config, state, chat_id).await,
        MobileCommand::ClaudeMsg(body) => {
            handle_claude_mobile_msg(bot, config, state, chat_id, body, agent_runner).await
        }
        MobileCommand::FlushMobile => {
            flush_bridge_session(bot, state, chat_id, AgentKind::Claude).await
        }
    }
}

async fn handle_bridge_command<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    chat_id: i64,
    cmd: BridgeCommand,
    agent_runner: Option<
        &Arc<crate::daemon::runner::AgentRunner<crate::daemon::cli_spawner::CliSpawner>>,
    >,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }
    match cmd {
        BridgeCommand::List(AgentKind::Claude) => {
            handle_list_claude_command(bot, config, state, chat_id).await
        }
        BridgeCommand::List(AgentKind::Codex) => {
            handle_list_codex_command(bot, config, state, chat_id).await
        }
        BridgeCommand::List(AgentKind::Gemini) => {
            handle_list_gemini_command(bot, config, state, chat_id).await
        }
        BridgeCommand::List(AgentKind::Antigravity) => {
            handle_list_antigravity_command(bot, config, state, chat_id).await
        }
        BridgeCommand::Chat(AgentKind::Claude, body) => {
            handle_claude_mobile_msg(bot, config, state, chat_id, body, agent_runner).await
        }
        BridgeCommand::Chat(AgentKind::Codex, body) => {
            handle_codex_bridge_msg(bot, config, state, chat_id, body, agent_runner).await
        }
        BridgeCommand::Chat(AgentKind::Gemini, body) => {
            handle_gemini_bridge_msg(bot, config, state, chat_id, body).await
        }
        BridgeCommand::Chat(AgentKind::Antigravity, body) => {
            handle_antigravity_bridge_msg(bot, config, state, chat_id, body).await
        }
        BridgeCommand::Flush(AgentKind::Claude) => {
            flush_bridge_session(bot, state, chat_id, AgentKind::Claude).await
        }
        BridgeCommand::Flush(AgentKind::Gemini) => {
            flush_bridge_session(bot, state, chat_id, AgentKind::Gemini).await
        }
        BridgeCommand::Flush(AgentKind::Codex) => {
            flush_bridge_session(bot, state, chat_id, AgentKind::Codex).await
        }
        BridgeCommand::Flush(AgentKind::Antigravity) => {
            flush_bridge_session(bot, state, chat_id, AgentKind::Antigravity).await
        }
    }
}

async fn handle_gemini_bridge_msg<B: BotClient + ?Sized>(
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
    let selected_session = snapshot
        .bridged_sessions
        .get(&chat_id.to_string())
        .and_then(|by_agent| by_agent.get("gemini"))
        .cloned();
    let Some(repo_id) = selected_session
        .as_ref()
        .map(|bridge| bridge.repo_id.as_str())
        .or_else(|| {
            snapshot
                .default_repo_by_chat
                .get(&chat_id.to_string())
                .map(String::as_str)
        })
    else {
        bot.send_message(
            chat_id,
            "No default repo for this chat. Use /switch_rp first.".to_string(),
            None,
        )
        .await?;
        return Ok(());
    };
    let Some(repo) = repo_by_id(config, repo_id) else {
        return Err(TelegramError::UnknownRepo(repo_id.to_string()));
    };
    if !repo.agents.iter().any(|allowed| allowed == "gemini") {
        bot.send_message(
            chat_id,
            format!("Agent gemini is not enabled for repo {}", repo.id),
            None,
        )
        .await?;
        return Ok(());
    }

    let timeout_secs = claude_headless::resolved_timeout_secs();
    let approval_mode = gemini_approval_mode();
    bot.send_message(
        chat_id,
        format!("⏳ gemini thinking... (timeout {timeout_secs}s, approval={approval_mode})"),
        None,
    )
    .await
    .ok();

    let repo_path = PathBuf::from(&repo.path);

    let initial_label = selected_session
        .as_ref()
        .map(session_label_for)
        .unwrap_or_else(|| "new".to_string());

    let resume_result = if let Some(ref bridge) = selected_session {
        Some(
            run_gemini_resume(
                &repo_path,
                &bridge.desktop_session_id,
                &body,
                timeout_secs,
                &approval_mode,
            )
            .await,
        )
    } else {
        None
    };

    let (reply, session_label, auto_save_bridge) = match resume_result {
        Some(Ok(reply)) => (reply, initial_label, None),
        Some(Err(msg)) if !is_stale_gemini_session_error(&msg) => {
            bot.send_message(chat_id, msg, None).await?;
            return Ok(());
        }
        other => {
            if matches!(other, Some(Err(_))) {
                tracing::warn!(
                    target: "agent_bus::session_bridge",
                    "gemini bridge session stale; falling back to headless"
                );
                bot.send_message(
                    chat_id,
                    "⚠️ Stale gemini session — starting fresh via gemini headless.".to_string(),
                    None,
                )
                .await
                .ok();
            }
            match run_gemini_headless(&repo_path, &body, timeout_secs, &approval_mode).await {
                Ok(reply) => {
                    let new_id = session_bridge::find_latest_gemini_session_id(&repo_path);
                    let new_title = if let Some(ref id) = new_id {
                        list_gemini_sessions(&repo_path, 50)
                            .await
                            .ok()
                            .and_then(|list| list.into_iter().find(|s| s.id == *id))
                            .and_then(|s| s.title)
                    } else {
                        None
                    };
                    let (label, new_bridge) = match new_id {
                        Some(id) => {
                            // Preserve the original selected session's title so the
                            // chat header stays meaningful after fallback. Only use
                            // the new session's auto-generated title when no session
                            // was previously selected (initial_label == "new").
                            let label = if initial_label != "new" {
                                initial_label.clone()
                            } else {
                                new_title
                                    .clone()
                                    .unwrap_or_else(|| id[..id.len().min(8)].to_string())
                            };
                            let now = time::OffsetDateTime::now_utc();
                            let selected_at = now
                                .format(&time::format_description::well_known::Rfc3339)
                                .unwrap_or_else(|_| now.unix_timestamp().to_string());
                            let new_bridge = BridgedSessionState {
                                agent: AgentKind::Gemini.to_string(),
                                repo_id: repo.id.clone(),
                                desktop_session_id: id.clone(),
                                desktop_path: String::new(),
                                mobile_session_id: id,
                                mobile_path: String::new(),
                                selected_at,
                                sync: SessionSyncCursor {
                                    desktop_offset: 0,
                                    mobile_offset: 0,
                                    last_synced_at: None,
                                    last_error: None,
                                },
                                display_name: Some(label.clone()),
                            };
                            (label, Some(new_bridge))
                        }
                        None => ("new".to_string(), None),
                    };
                    (reply, label, new_bridge)
                }
                Err(msg) => {
                    bot.send_message(chat_id, msg, None).await?;
                    return Ok(());
                }
            }
        }
    };

    if let Some(bridge) = auto_save_bridge {
        let _ = state
            .set_bridged_session(chat_id.to_string(), "gemini".to_string(), bridge)
            .await;
    }

    let trimmed = reply.trim();
    if trimmed.is_empty() {
        bot.send_message(chat_id, "(empty reply from gemini)".to_string(), None)
            .await?;
        return Ok(());
    }
    let header = agent_header("Gemini", &session_label);
    for chunk in claude_headless::chunk_for_telegram(&format!("{header}{trimmed}"), 4000) {
        bot.send_message(chat_id, chunk, None).await?;
    }
    Ok(())
}

async fn handle_codex_bridge_msg<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    chat_id: i64,
    body: String,
    agent_runner: Option<
        &Arc<crate::daemon::runner::AgentRunner<crate::daemon::cli_spawner::CliSpawner>>,
    >,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }

    let snapshot = state.snapshot().await;
    let Some(mut bridge) = snapshot
        .bridged_sessions
        .get(&chat_id.to_string())
        .and_then(|by_agent| by_agent.get("codex"))
        .cloned()
    else {
        bot.send_message(
            chat_id,
            "No codex session selected. Send /list_codex first.".to_string(),
            None,
        )
        .await?;
        return Ok(());
    };
    let Some(repo) = repo_by_id(config, &bridge.repo_id) else {
        return Err(TelegramError::UnknownRepo(bridge.repo_id.clone()));
    };
    if let Err(err) =
        session_bridge::sync_bridged_session_locked(&chat_id.to_string(), "codex", &mut bridge)
            .await
    {
        bridge.sync.last_error = Some(err.to_string());
        state
            .set_bridged_session(chat_id.to_string(), "codex".to_string(), bridge.clone())
            .await?;
        bot.send_message(chat_id, format!("@codex sync failed: {err}"), None)
            .await?;
        return Ok(());
    }
    state
        .set_bridged_session(chat_id.to_string(), "codex".to_string(), bridge.clone())
        .await?;

    let codex_session_label = session_label_for(&bridge);

    bot.send_message(chat_id, "⏳ codex thinking...".to_string(), None)
        .await
        .ok();

    let fallback_prompt = codex_bridge_prompt(&bridge, repo, &body);
    let timeout = Duration::from_secs(claude_headless::resolved_timeout_secs());
    let reply = match repo.codex_mode {
        CodexMode::LiveBridge => {
            match run_codex_bridge_via_desktop_ipc(&bridge, &body, timeout).await {
                Ok(reply) => Ok(reply),
                Err(CodexBridgeIpcRunError::Fallback(reason)) => {
                    tracing::warn!(
                        target: "agent_bus::session_bridge",
                        desktop_session_id = %bridge.desktop_session_id,
                        reason = %reason,
                        "falling back to Codex CLI resume because desktop IPC is unavailable"
                    );
                    let Some(runner) = agent_runner else {
                        bot.send_message(
                        chat_id,
                        "Codex session bridge requires a live desktop Codex session or auth-contexts.yaml / AgentRunner.".to_string(),
                        None,
                    )
                    .await?;
                        return Ok(());
                    };
                    run_agent_via_runner(
                        runner,
                        AgentKind::Codex,
                        AgentRunInput {
                            repo_id: bridge.repo_id.clone(),
                            repo_path: PathBuf::from(&repo.path),
                            prompt: fallback_prompt,
                            mode: AgentRunMode::CodexResume {
                                session_id: bridge.desktop_session_id.clone(),
                                transcript_path: Some(PathBuf::from(&bridge.desktop_path)),
                            },
                            timeout,
                            chat_id,
                        },
                    )
                    .await
                }
                Err(CodexBridgeIpcRunError::StartedButNoReply(message)) => Err(message),
            }
        }
        CodexMode::AppServer => {
            codex_app_server::run_codex_turn_via_app_server(
                bot,
                config,
                state.clone(),
                repo,
                &bridge,
                &body,
                timeout,
                chat_id,
            )
            .await
        }
    };

    let reply = match reply {
        Ok(out) => out,
        Err(msg) => {
            bot.send_message(chat_id, msg, None).await?;
            return Ok(());
        }
    };
    if let Ok(stats) =
        session_bridge::sync_bridged_session_locked(&chat_id.to_string(), "codex", &mut bridge)
            .await
    {
        let _ = state
            .set_bridged_session(chat_id.to_string(), "codex".to_string(), bridge)
            .await;
        tracing::debug!(
            target: "agent_bus::session_bridge",
            copied = stats.copied(),
            skipped = stats.skipped(),
            errors = stats.errors(),
            "synced codex bridge after mobile reply"
        );
    }
    let trimmed = reply.trim();
    if trimmed.is_empty() {
        bot.send_message(chat_id, "(empty reply from codex)".to_string(), None)
            .await?;
        return Ok(());
    }
    const MAX_CHUNK: usize = 4000;
    let header = agent_header("Codex", &codex_session_label);
    for chunk in claude_headless::chunk_for_telegram(&format!("{header}{trimmed}"), MAX_CHUNK) {
        bot.send_message(chat_id, chunk, None).await?;
    }
    Ok(())
}

async fn handle_antigravity_bridge_msg<B: BotClient + ?Sized>(
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
    let selected_session = snapshot
        .bridged_sessions
        .get(&chat_id.to_string())
        .and_then(|by_agent| by_agent.get("antigravity"))
        .cloned();
    let Some(bridge) = selected_session else {
        bot.send_message(
            chat_id,
            "No antigravity session selected. Send /list_antigravity first.".to_string(),
            None,
        )
        .await?;
        return Ok(());
    };

    let Some(repo) = repo_by_id(config, &bridge.repo_id) else {
        return Err(TelegramError::UnknownRepo(bridge.repo_id.clone()));
    };
    let antigravity_enabled = repo
        .agents
        .iter()
        .any(|allowed| allowed == "antigravity" || allowed == "gemini");
    if !antigravity_enabled {
        bot.send_message(
            chat_id,
            format!("Agent antigravity is not enabled for repo {}", repo.id),
            None,
        )
        .await?;
        return Ok(());
    }

    let initial_label = session_label_for(&bridge);
    if is_session_name_query(&body) {
        let full_name = bridge
            .display_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&initial_label);
        bot.send_message(
            chat_id,
            format!(
                "Antigravity session hiện tại: {full_name}\nID: {}",
                bridge.desktop_session_id
            ),
            None,
        )
        .await?;
        return Ok(());
    }

    let selected_model = snapshot
        .selected_model_by_chat
        .get(&chat_id.to_string())
        .and_then(|by_agent| by_agent.get("antigravity"))
        .cloned();
    if is_model_query(&body) {
        let text = match selected_model.as_deref() {
            Some(model) => {
                format!("Antigravity model hiện tại: {model}\nĐổi bằng /set_model_antigravity <model>")
            }
            None => "Antigravity model hiện tại: default (server chọn).\nĐổi bằng /set_model_antigravity <model>".to_string(),
        };
        bot.send_message(chat_id, text, None).await?;
        return Ok(());
    }

    let timeout_secs = claude_headless::resolved_timeout_secs();
    let model_label = selected_model
        .as_deref()
        .map(|m| format!(", model={m}"))
        .unwrap_or_default();
    bot.send_message(
        chat_id,
        format!("⏳ antigravity thinking... (timeout {timeout_secs}s, brain resume{model_label})"),
        None,
    )
    .await
    .ok();

    let resume_result = run_antigravity_brain_resume(
        bot,
        state.clone(),
        chat_id,
        &repo.id,
        &repo.path,
        &bridge.desktop_session_id,
        &body,
        timeout_secs,
        selected_model.as_deref(),
    )
    .await;

    let (reply, antigravity_session_label) = match resume_result {
        Ok(reply) => (reply, initial_label),
        Err(msg) => {
            tracing::warn!(
                target: "agent_bus::session_bridge",
                session_id = %bridge.desktop_session_id,
                "antigravity bridge session resume failed"
            );
            bot.send_message(
                chat_id,
                format!(
                    "⚠️ Could not resume selected Antigravity brain session {}. Selection was kept.\n{}",
                    bridge.desktop_session_id,
                    msg
                ),
                None,
            )
            .await?;
            return Ok(());
        }
    };

    let trimmed = reply.trim();
    if trimmed.is_empty() {
        bot.send_message(chat_id, "(empty reply from antigravity)".to_string(), None)
            .await?;
        return Ok(());
    }
    let header = agent_header("Antigravity", &antigravity_session_label);
    for chunk in claude_headless::chunk_for_telegram(&format!("{header}{trimmed}"), 4000) {
        bot.send_message(chat_id, chunk, None).await?;
    }
    Ok(())
}

enum CodexBridgeIpcRunError {
    Fallback(String),
    StartedButNoReply(String),
}

async fn run_codex_bridge_via_desktop_ipc(
    bridge: &BridgedSessionState,
    prompt: &str,
    timeout: Duration,
) -> Result<String, CodexBridgeIpcRunError> {
    let desktop_path = PathBuf::from(&bridge.desktop_path);
    let start_offset = std::fs::metadata(&desktop_path)
        .map(|meta| meta.len())
        .unwrap_or(0);

    CodexIpcClient::default()
        .start_thread_follower_turn(
            &bridge.desktop_session_id,
            prompt,
            Duration::from_secs(8),
        )
        .await
        .map_err(|err| match err {
            CodexIpcError::Unavailable(reason)
            | CodexIpcError::Protocol(reason)
            | CodexIpcError::RequestFailed(reason) => CodexBridgeIpcRunError::Fallback(reason),
            CodexIpcError::NoLiveOwner => {
                CodexBridgeIpcRunError::Fallback("desktop session is not open".to_string())
            }
            CodexIpcError::Timeout => CodexBridgeIpcRunError::StartedButNoReply(
                "Codex desktop IPC timed out before confirming the turn. Check the desktop Codex session before retrying to avoid duplicate work.".to_string(),
            ),
        })?;

    session_bridge::wait_for_codex_desktop_reply(desktop_path, start_offset, timeout)
        .await
        .map_err(|err| {
            CodexBridgeIpcRunError::StartedButNoReply(format!(
                "Codex desktop accepted the Telegram turn, but agent-bus could not observe a completed reply in the desktop transcript: {err}"
            ))
        })
}

fn codex_bridge_prompt(
    bridge: &BridgedSessionState,
    repo: &RepoEntry,
    user_message: &str,
) -> String {
    format!(
        "[agent-bus desktop session bridge]\n\
You are continuing the selected desktop Codex session directly.\n\
Use the selected desktop session transcript as the primary context for this Telegram message.\n\
Repo id: {repo_id}\n\
Repo path: {repo_path}\n\
Desktop session id: {desktop_session_id}\n\
\n\
User message from Telegram:\n{user_message}",
        repo_id = bridge.repo_id,
        repo_path = repo.path,
        desktop_session_id = bridge.desktop_session_id,
    )
}

async fn flush_bridge_session<B: BotClient + ?Sized>(
    bot: &B,
    state: StateHandle,
    chat_id: i64,
    agent: AgentKind,
) -> Result<(), TelegramError> {
    let chat_key = chat_id.to_string();
    let agent_key = agent.to_string();
    let snapshot = state.snapshot().await;
    let Some(mut bridge) = snapshot
        .bridged_sessions
        .get(&chat_key)
        .and_then(|by_agent| by_agent.get(&agent_key))
        .cloned()
    else {
        bot.send_message(
            chat_id,
            format!("No {agent} session selected. Send /list_{agent} first."),
            None,
        )
        .await?;
        return Ok(());
    };

    let result =
        session_bridge::sync_bridged_session_locked(&chat_key, &agent_key, &mut bridge).await;
    match result {
        Ok(stats) => {
            state
                .set_bridged_session(chat_key, agent_key, bridge)
                .await?;
            bot.send_message(chat_id, format_bridge_sync_stats(agent, stats), None)
                .await?;
            Ok(())
        }
        Err(err) => {
            bridge.sync.last_error = Some(err.to_string());
            state
                .set_bridged_session(chat_key, agent_key, bridge)
                .await?;
            bot.send_message(chat_id, format!("@flush_{agent} failed: {err}"), None)
                .await?;
            Ok(())
        }
    }
}

fn format_bridge_sync_stats(agent: AgentKind, stats: BridgeSyncStats) -> String {
    if agent == AgentKind::Gemini {
        return "@flush_gemini: Gemini bridge is resume-based; no transcript sync is needed."
            .to_string();
    }
    if agent == AgentKind::Antigravity {
        return "@flush_antigravity: Antigravity bridge is resume-based; no transcript sync is needed."
            .to_string();
    }
    format!(
        "@flush_{agent} synced\n\
desktop -> mobile: copied {}, skipped {}, errors {}\n\
mobile -> desktop: copied {}, skipped {}, errors {}\n\
total: copied {}, skipped {}, errors {}",
        stats.desktop_to_mobile.copied,
        stats.desktop_to_mobile.skipped,
        stats.desktop_to_mobile.errors,
        stats.mobile_to_desktop.copied,
        stats.mobile_to_desktop.skipped,
        stats.mobile_to_desktop.errors,
        stats.copied(),
        stats.skipped(),
        stats.errors()
    )
}

pub async fn handle_list_claude_command<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    _state: StateHandle,
    chat_id: i64,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }

    let sessions =
        match mobile_session::detect_all_sessions(&claude_projects_root(), MOBILE_UUID, 10) {
            Ok(s) => s,
            Err(err) => {
                bot.send_message(chat_id, format!("Failed to scan sessions: {err}"), None)
                    .await?;
                return Ok(());
            }
        };

    if sessions.is_empty() {
        bot.send_message(chat_id, "No Claude sessions found.".to_string(), None)
            .await?;
        return Ok(());
    }

    let now = time::OffsetDateTime::now_utc();
    let rows = sessions
        .iter()
        .map(|session| {
            let title = mobile_session::pick_session_label(session);
            let rel = mobile_session::relative_time(now, session.last_modified);
            let prefix = repo_short_label_from_source(&session.cwd);
            let text = format!(
                "{} · {} · {} turns · {}",
                prefix, title, session.turn_count, rel
            );
            vec![(text, format!("sel_claude:{}", session.uuid))]
        })
        .collect();
    let text = "Active Claude sessions:".to_string();
    bot.send_message(chat_id, text, Some(InlineKeyboard { rows }))
        .await?;
    Ok(())
}

pub async fn handle_list_codex_command<B: BotClient + ?Sized>(
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
            "No default repo for this chat. Use /switch_rp first.".to_string(),
            None,
        )
        .await?;
        return Ok(());
    };
    let Some(repo) = repo_by_id(config, repo_id) else {
        return Err(TelegramError::UnknownRepo(repo_id.clone()));
    };

    let codex_home = codex_home_dir();
    let sessions = match session_bridge::detect_codex_sessions(&codex_home, &repo.path, 10) {
        Ok(sessions) => sessions,
        Err(err) => {
            bot.send_message(
                chat_id,
                format!("Failed to scan Codex sessions: {err}"),
                None,
            )
            .await?;
            return Ok(());
        }
    };

    if sessions.is_empty() {
        bot.send_message(
            chat_id,
            format!(
                "No Codex sessions for {} (scanned {}).",
                repo.display,
                codex_home.join("sessions").display()
            ),
            None,
        )
        .await?;
        return Ok(());
    }

    let rows = sessions
        .iter()
        .map(|session| {
            let short = session.id.get(..8).unwrap_or(&session.id);
            vec![(
                codex_session_button_label(
                    Some(&repo_short_label(&repo.display)),
                    session.title.as_deref(),
                    short,
                ),
                format!("sel_codex:{}", session.id),
            )]
        })
        .collect();
    bot.send_message(
        chat_id,
        format!("Active Codex sessions ({}):", repo.display),
        Some(InlineKeyboard { rows }),
    )
    .await?;
    Ok(())
}

pub async fn handle_list_gemini_command<B: BotClient + ?Sized>(
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
            "No default repo for this chat. Use /switch_rp first.".to_string(),
            None,
        )
        .await?;
        return Ok(());
    };
    let Some(repo) = repo_by_id(config, repo_id) else {
        return Err(TelegramError::UnknownRepo(repo_id.clone()));
    };

    let sessions = match list_gemini_sessions(Path::new(&repo.path), 10).await {
        Ok(sessions) => sessions,
        Err(err) => {
            bot.send_message(chat_id, err, None).await?;
            return Ok(());
        }
    };

    if sessions.is_empty() {
        bot.send_message(
            chat_id,
            format!("No Gemini sessions for {}.", repo.display),
            None,
        )
        .await?;
        return Ok(());
    }

    let rows = sessions
        .iter()
        .map(|session| {
            let short = session.id.get(..8).unwrap_or(&session.id);
            vec![(
                gemini_session_button_label(
                    Some(&repo_short_label(&repo.display)),
                    session.title.as_deref(),
                    short,
                ),
                format!("sel_gemini:{}", session.id),
            )]
        })
        .collect();
    bot.send_message(
        chat_id,
        format!("Active Gemini sessions ({}):", repo.display),
        Some(InlineKeyboard { rows }),
    )
    .await?;
    Ok(())
}

pub async fn handle_set_model_antigravity_command<B: BotClient + ?Sized>(
    bot: &B,
    state: StateHandle,
    chat_id: i64,
    arg: Option<&str>,
) -> Result<(), TelegramError> {
    let snapshot = state.snapshot().await;
    let current = snapshot
        .selected_model_by_chat
        .get(&chat_id.to_string())
        .and_then(|by_agent| by_agent.get("antigravity"))
        .cloned();
    match arg.map(str::trim).filter(|s| !s.is_empty()) {
        None => {
            let msg = match current {
                Some(model) => format!(
                    "Antigravity model: {model}\nUsage: /set_model_antigravity <model> | clear\nList available: /models_antigravity"
                ),
                None => "Antigravity model: (default — recommended by server)\nUsage: /set_model_antigravity <model>\nList available: /models_antigravity".to_string(),
            };
            bot.send_message(chat_id, msg, None).await?;
        }
        Some("clear") | Some("none") | Some("default") => {
            state
                .set_selected_model(chat_id.to_string(), "antigravity", None)
                .await?;
            bot.send_message(
                chat_id,
                "Cleared antigravity model selection (will use server default).".to_string(),
                None,
            )
            .await?;
        }
        Some(model) => {
            state
                .set_selected_model(chat_id.to_string(), "antigravity", Some(model.to_string()))
                .await?;
            bot.send_message(
                chat_id,
                format!("Antigravity model set to `{model}` for this chat."),
                None,
            )
            .await?;
        }
    }
    Ok(())
}

pub async fn handle_list_antigravity_models_command<B: BotClient + ?Sized>(
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
            "No default repo for this chat. Use /switch_rp first.".to_string(),
            None,
        )
        .await?;
        return Ok(());
    };
    let Some(repo) = repo_by_id(config, repo_id) else {
        return Err(TelegramError::UnknownRepo(repo_id.clone()));
    };

    // We need a cascade id for discovery; use any antigravity session for this
    // repo. If none, fall back to the first running language server process.
    let sessions =
        session_bridge::detect_antigravity_sessions(Some(&repo.path), 1).unwrap_or_default();
    let cascade_id = sessions.into_iter().next().map(|s| s.id);

    let server = match cascade_id.as_deref() {
        Some(id) => discover_antigravity_language_server(&repo.path, id).await,
        None => Err(
            "No Antigravity session found for this repo — open one in the desktop app first."
                .to_string(),
        ),
    };
    let server = match server {
        Ok(s) => s,
        Err(err) => {
            bot.send_message(chat_id, err, None).await?;
            return Ok(());
        }
    };

    let client = match antigravity_http_client(Duration::from_secs(15)) {
        Ok(c) => c,
        Err(err) => {
            bot.send_message(chat_id, err, None).await?;
            return Ok(());
        }
    };
    let configs = match fetch_antigravity_model_configs(&client, &server).await {
        Ok(c) => c,
        Err(err) => {
            bot.send_message(chat_id, err, None).await?;
            return Ok(());
        }
    };
    if configs.is_empty() {
        bot.send_message(
            chat_id,
            "No models reported by Antigravity.".to_string(),
            None,
        )
        .await?;
        return Ok(());
    }
    let current_input = snapshot
        .selected_model_by_chat
        .get(&chat_id.to_string())
        .and_then(|by_agent| by_agent.get("antigravity"))
        .cloned();
    let current_resolved = current_input
        .as_deref()
        .and_then(|input| resolve_antigravity_model(input, &configs));
    let mut lines = vec!["Available Antigravity models:".to_string()];
    for c in &configs {
        let quota = match c.remaining_fraction {
            Some(f) => format!(" ({}% quota)", (f * 100.0).round() as i64),
            None => String::new(),
        };
        let marker = if Some(&c.model_id) == current_resolved.as_ref() {
            " ◀"
        } else {
            ""
        };
        lines.push(format!("• {}{quota}{marker}", c.label));
    }
    lines.push(String::new());
    lines.push(
        "Use /set_model_antigravity <name> — accepts a fragment of the label (e.g. `flash`, `claude sonnet`)."
            .to_string(),
    );
    if let Some(input) = current_input.as_deref() {
        if current_resolved.is_none() {
            lines.push(format!(
                "⚠️ Current setting `{input}` does not match any available model."
            ));
        }
    }
    bot.send_message(chat_id, lines.join("\n"), None).await?;
    Ok(())
}

pub async fn handle_list_antigravity_command<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    chat_id: i64,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }

    let _snapshot = state.snapshot().await;
    let mut sessions = match session_bridge::detect_antigravity_sessions(None, 50) {
        Ok(sessions) => sessions,
        Err(err) => {
            bot.send_message(chat_id, short_err("scan failed", &err.to_string()), None)
                .await?;
            return Ok(());
        }
    };

    // For each session, attach the matching registered repo (by prefix match)
    // so the display prefix shows the repo name and the callback can resolve it.
    for session in sessions.iter_mut() {
        if let Some(matched) = session
            .repo_path
            .as_deref()
            .and_then(|path| repo_by_path_prefix(config, path))
        {
            session.repo_path = Some(matched.path.clone());
        }
    }

    if sessions.is_empty() {
        bot.send_message(chat_id, "No Antigravity sessions found.".to_string(), None)
            .await?;
        return Ok(());
    }

    let rows = sessions
        .iter()
        .map(|session| {
            let short = session.id.get(..8).unwrap_or(&session.id);
            vec![(
                antigravity_session_button_label(
                    session
                        .repo_path
                        .as_deref()
                        .map(repo_short_label_from_source)
                        .as_deref(),
                    session.title.as_deref(),
                    short,
                ),
                format!("sel_antigravity:{}", session.id),
            )]
        })
        .collect();
    bot.send_message(
        chat_id,
        "Active Antigravity sessions:".to_string(),
        Some(InlineKeyboard { rows }),
    )
    .await?;
    Ok(())
}

fn codex_session_button_label(
    repo_prefix: Option<&str>,
    title: Option<&str>,
    short_id: &str,
) -> String {
    session_button_label(repo_prefix, title, short_id, "Codex")
}

fn gemini_session_button_label(
    repo_prefix: Option<&str>,
    title: Option<&str>,
    short_id: &str,
) -> String {
    session_button_label(repo_prefix, title, short_id, "Gemini")
}

fn antigravity_session_button_label(
    repo_prefix: Option<&str>,
    title: Option<&str>,
    short_id: &str,
) -> String {
    session_button_label(repo_prefix, title, short_id, "Antigravity")
}

fn session_button_label(
    repo_prefix: Option<&str>,
    title: Option<&str>,
    short_id: &str,
    fallback_name: &str,
) -> String {
    let base = match title {
        Some(title) => format!("{title} ({short_id})"),
        None => format!("{fallback_name} {short_id}"),
    };
    match repo_prefix {
        Some(prefix) if !prefix.is_empty() => format!("{prefix} · {base}"),
        _ => base,
    }
}

fn repo_short_label(display: &str) -> String {
    truncate_chars(display.trim(), 10)
}

fn repo_short_label_from_source(source: &str) -> String {
    let trimmed = source.trim_end_matches('/');
    let base = Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(trimmed);
    truncate_chars(base, 10)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out = text
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

fn codex_home_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(home) = CODEX_HOME_OVERRIDE
        .lock()
        .expect("codex home override lock poisoned")
        .clone()
    {
        return home;
    }
    if let Ok(home) = std::env::var("CODEX_HOME") {
        return PathBuf::from(home);
    }
    std::env::var("HOME")
        .map(|home| PathBuf::from(home).join(".codex"))
        .unwrap_or_else(|_| PathBuf::from(".codex"))
}

fn claude_projects_root() -> PathBuf {
    std::env::var("HOME")
        .map(|home| PathBuf::from(home).join(".claude").join("projects"))
        .unwrap_or_else(|_| PathBuf::from(".claude/projects"))
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

    let sessions =
        match mobile_session::detect_all_sessions(&claude_projects_root(), MOBILE_UUID, 200) {
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
    let Some(repo) = repo_by_path(config, &session.cwd) else {
        bot.answer_callback(callback_id, "Repo not registered".to_string())
            .await?;
        return Ok(());
    };

    let now = time::OffsetDateTime::now_utc();
    let forked_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| now.unix_timestamp().to_string());
    let desktop_offset = session.path.metadata().map(|m| m.len()).unwrap_or(0);
    let title = mobile_session::pick_session_label(session);
    let bridge = BridgedSessionState {
        agent: AgentKind::Claude.to_string(),
        repo_id: repo.id.clone(),
        desktop_session_id: desktop_uuid.clone(),
        desktop_path: session.path.display().to_string(),
        mobile_session_id: desktop_uuid.clone(),
        mobile_path: session.path.display().to_string(),
        selected_at: forked_at.clone(),
        sync: SessionSyncCursor {
            desktop_offset,
            mobile_offset: desktop_offset,
            last_synced_at: None,
            last_error: None,
        },
        display_name: Some(title.clone()),
    };
    state
        .set_bridged_session(chat_id.to_string(), AgentKind::Claude.to_string(), bridge)
        .await?;
    let confirm = format!(
        "✅ Claude session \"{}\" selected. Send @claude <msg> to continue.",
        title
    );
    bot.edit_message_text(message, confirm).await?;
    bot.answer_callback(callback_id, "Selected".to_string())
        .await?;
    Ok(())
}

pub async fn handle_callback_sel_codex<B: BotClient + ?Sized>(
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

    let Some((AgentKind::Codex, codex_id)) = session_bridge::parse_callback_data(&callback_data)
    else {
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

    let sessions = match session_bridge::detect_codex_sessions(&codex_home_dir(), &repo.path, 50) {
        Ok(sessions) => sessions,
        Err(err) => {
            bot.answer_callback(callback_id, short_err("scan failed", &err.to_string()))
                .await?;
            return Ok(());
        }
    };
    let Some(session) = sessions.into_iter().find(|session| session.id == codex_id) else {
        bot.answer_callback(callback_id, "Session not found".to_string())
            .await?;
        return Ok(());
    };

    let now = time::OffsetDateTime::now_utc();
    let selected_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| now.unix_timestamp().to_string());

    let desktop_offset = session.path.metadata().map(|m| m.len()).unwrap_or(0);
    let short = session.id.get(..8).unwrap_or(&session.id);
    let label = session.title.as_deref().unwrap_or(short);
    let bridge = BridgedSessionState {
        agent: AgentKind::Codex.to_string(),
        repo_id: repo.id.clone(),
        desktop_session_id: session.id.clone(),
        desktop_path: session.path.display().to_string(),
        mobile_session_id: session.id.clone(),
        mobile_path: session.path.display().to_string(),
        selected_at,
        sync: SessionSyncCursor {
            desktop_offset,
            mobile_offset: desktop_offset,
            last_synced_at: None,
            last_error: None,
        },
        display_name: Some(label.to_string()),
    };
    state
        .set_bridged_session(chat_id.to_string(), AgentKind::Codex.to_string(), bridge)
        .await?;
    bot.edit_message_text(
        message,
        format!("{label} selected. Send @codex <msg> to continue."),
    )
    .await?;
    bot.answer_callback(callback_id, "Selected".to_string())
        .await?;
    Ok(())
}

pub async fn handle_callback_sel_gemini<B: BotClient + ?Sized>(
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

    let Some((AgentKind::Gemini, gemini_id)) = session_bridge::parse_callback_data(&callback_data)
    else {
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

    let sessions = match list_gemini_sessions(Path::new(&repo.path), 50).await {
        Ok(sessions) => sessions,
        Err(err) => {
            bot.answer_callback(callback_id, short_err("scan failed", &err))
                .await?;
            return Ok(());
        }
    };
    let Some(session) = sessions.into_iter().find(|session| session.id == gemini_id) else {
        bot.answer_callback(callback_id, "Session not found".to_string())
            .await?;
        return Ok(());
    };

    let now = time::OffsetDateTime::now_utc();
    let selected_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| now.unix_timestamp().to_string());

    let short = session.id.get(..8).unwrap_or(&session.id);
    let label = session.title.as_deref().unwrap_or(short);
    let bridge = BridgedSessionState {
        agent: AgentKind::Gemini.to_string(),
        repo_id: repo.id.clone(),
        desktop_session_id: session.id.clone(),
        desktop_path: String::new(),
        mobile_session_id: session.id.clone(),
        mobile_path: String::new(),
        selected_at,
        sync: SessionSyncCursor {
            desktop_offset: 0,
            mobile_offset: 0,
            last_synced_at: None,
            last_error: None,
        },
        display_name: Some(label.to_string()),
    };
    state
        .set_bridged_session(chat_id.to_string(), AgentKind::Gemini.to_string(), bridge)
        .await?;

    bot.edit_message_text(
        message,
        format!("{label} selected. Send @gemini <msg> to continue."),
    )
    .await?;
    bot.answer_callback(callback_id, "Selected".to_string())
        .await?;
    Ok(())
}

pub async fn handle_callback_sel_antigravity<B: BotClient + ?Sized>(
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

    let Some((AgentKind::Antigravity, antigravity_id)) =
        session_bridge::parse_callback_data(&callback_data)
    else {
        return Err(TelegramError::InvalidCallback(callback_data));
    };

    let sessions = match session_bridge::detect_antigravity_sessions(None, 50) {
        Ok(sessions) => sessions,
        Err(err) => {
            bot.answer_callback(callback_id, short_err("scan failed", &err.to_string()))
                .await?;
            return Ok(());
        }
    };
    let Some(session) = sessions
        .into_iter()
        .find(|session| session.id == antigravity_id)
    else {
        bot.answer_callback(callback_id, "Session not found".to_string())
            .await?;
        return Ok(());
    };
    let Some(repo_path) = session.repo_path.as_deref() else {
        bot.answer_callback(callback_id, "Session repo not found".to_string())
            .await?;
        return Ok(());
    };
    let Some(repo) = repo_by_path_prefix(config, repo_path) else {
        bot.answer_callback(callback_id, "Repo not registered".to_string())
            .await?;
        return Ok(());
    };

    let now = time::OffsetDateTime::now_utc();
    let selected_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| now.unix_timestamp().to_string());

    let short = session.id.get(..8).unwrap_or(&session.id);
    let label = session.title.as_deref().unwrap_or(short);
    let bridge = BridgedSessionState {
        agent: AgentKind::Antigravity.to_string(),
        repo_id: repo.id.clone(),
        desktop_session_id: session.id.clone(),
        desktop_path: String::new(),
        mobile_session_id: session.id.clone(),
        mobile_path: String::new(),
        selected_at,
        sync: SessionSyncCursor {
            desktop_offset: 0,
            mobile_offset: 0,
            last_synced_at: None,
            last_error: None,
        },
        display_name: Some(label.to_string()),
    };
    state
        .set_bridged_session(
            chat_id.to_string(),
            AgentKind::Antigravity.to_string(),
            bridge,
        )
        .await?;
    bot.edit_message_text(
        message,
        format!("{label} selected. Send @antigravity <msg> to continue."),
    )
    .await?;
    bot.answer_callback(callback_id, "Selected".to_string())
        .await?;
    Ok(())
}

/// Handle `ant_appr:<approval_id>:<y|n>` — user clicked Approve/Deny on a
/// pending Antigravity tool approval prompt.
pub async fn handle_callback_antigravity_approval<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    message: MessageRef,
    callback_id: String,
    callback_data: String,
) -> Result<(), TelegramError> {
    let parts: Vec<&str> = callback_data
        .strip_prefix("ant_appr:")
        .ok_or_else(|| TelegramError::InvalidCallback(callback_data.clone()))?
        .split(':')
        .collect();
    let [approval_id, decision] = parts.as_slice() else {
        return Err(TelegramError::InvalidCallback(callback_data));
    };
    let allow = match *decision {
        "y" => true,
        "n" => false,
        _ => return Err(TelegramError::InvalidCallback(callback_data.clone())),
    };

    let snapshot = state.snapshot().await;
    let Some(entry) = snapshot
        .pending_antigravity_approvals
        .get(*approval_id)
        .cloned()
    else {
        bot.answer_callback(callback_id, "Approval no longer pending".to_string())
            .await?;
        let _ = bot
            .edit_message_text(message, "(approval expired or already handled)".to_string())
            .await;
        return Ok(());
    };

    let Some(repo) = repo_by_id(config, &entry.repo_id) else {
        bot.answer_callback(callback_id, "Repo not registered".to_string())
            .await?;
        return Ok(());
    };
    let server = match discover_antigravity_language_server(&repo.path, &entry.cascade_id).await {
        Ok(s) => s,
        Err(err) => {
            bot.answer_callback(callback_id, short_err("LS discovery", &err))
                .await?;
            return Ok(());
        }
    };
    let client = match antigravity_http_client(Duration::from_secs(15)) {
        Ok(c) => c,
        Err(err) => {
            bot.answer_callback(callback_id, short_err("client", &err))
                .await?;
            return Ok(());
        }
    };

    let pending = PendingAntigravityInteraction {
        trajectory_id: entry.trajectory_id.clone(),
        step_index: entry.step_index,
        kind: entry.kind.clone(),
        summary: entry.summary.clone(),
    };

    match respond_to_antigravity_interaction(&client, &server, &entry.cascade_id, &pending, allow)
        .await
    {
        Ok(()) => {
            let _ = state
                .remove_pending_antigravity_approval(approval_id.to_string())
                .await;
            let verdict = if allow { "✅ Approved" } else { "❌ Denied" };
            let _ = bot
                .edit_message_text(
                    message,
                    format!("{verdict}\n• kind: {}\n• {}", entry.kind, entry.summary),
                )
                .await;
            bot.answer_callback(callback_id, verdict.to_string())
                .await?;
        }
        Err(err) => {
            bot.answer_callback(callback_id, short_err("RPC", &err))
                .await?;
            tracing::warn!(
                target: "agent_bus::antigravity_approval",
                approval_id = %approval_id,
                allow = allow,
                error = %err,
                "Antigravity approval RPC failed"
            );
        }
    }
    Ok(())
}

/// AC-3 + AC-4 + Phase 4a.8: Handle `@claude <msg>` — when an `AgentRunner`
/// is present, dispatch through it for quota tracking + rotation; otherwise
/// fall back to the legacy `spawn_claude_resume` path (AC-Q9).
pub async fn handle_claude_mobile_msg<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    chat_id: i64,
    body: String,
    agent_runner: Option<
        &Arc<crate::daemon::runner::AgentRunner<crate::daemon::cli_spawner::CliSpawner>>,
    >,
) -> Result<(), TelegramError> {
    if !is_allowed(config, chat_id) {
        return Ok(());
    }

    let snapshot = state.snapshot().await;
    let chat_key = chat_id.to_string();
    let bridge = snapshot
        .bridged_sessions
        .get(&chat_key)
        .and_then(|by_agent| by_agent.get("claude"))
        .cloned();
    let mobile = snapshot.mobile_sessions.get(&chat_key).cloned();
    let Some((repo_id, resume_uuid)) = bridge
        .as_ref()
        .map(|bridge| (bridge.repo_id.as_str(), bridge.desktop_session_id.as_str()))
        .or_else(|| {
            mobile
                .as_ref()
                .map(|mobile| (mobile.repo_id.as_str(), mobile.mobile_uuid.as_str()))
        })
    else {
        bot.send_message(
            chat_id,
            "⚠️ No Claude session selected. Send /list_claude first to pick a desktop session."
                .to_string(),
            None,
        )
        .await?;
        return Ok(());
    };
    let Some(repo) = repo_by_id(config, repo_id) else {
        return Err(TelegramError::UnknownRepo(repo_id.to_string()));
    };

    let cwd = PathBuf::from(&repo.path);
    let timeout_secs = claude_headless::resolved_timeout_secs();
    let claude_session_label = bridge
        .as_ref()
        .map(session_label_for)
        .unwrap_or_else(|| make_session_label(resume_uuid, ""));

    bot.send_message(
        chat_id,
        format!("⏳ thinking... (timeout {}s)", timeout_secs),
        None,
    )
    .await
    .ok();

    let reply = if let Some(runner) = agent_runner {
        run_claude_via_runner(
            runner,
            repo_id,
            cwd.clone(),
            resume_uuid.to_string(),
            body.clone(),
            Duration::from_secs(timeout_secs),
            chat_id,
        )
        .await
    } else {
        run_claude_legacy(&cwd, resume_uuid, &body, timeout_secs).await
    };

    let reply = match reply {
        Ok(out) => out,
        Err(msg) => {
            bot.send_message(chat_id, msg, None).await?;
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
    let header = agent_header("Claude", &claude_session_label);
    for chunk in claude_headless::chunk_for_telegram(&format!("{header}{trimmed}"), MAX_CHUNK) {
        bot.send_message(chat_id, chunk, None).await?;
    }
    Ok(())
}

async fn run_claude_legacy(
    cwd: &Path,
    mobile_uuid: &str,
    body: &str,
    timeout_secs: u64,
) -> Result<String, String> {
    let claude_bin = claude_bin_path();
    claude_headless::spawn_claude_resume(&claude_bin, cwd, mobile_uuid, body, timeout_secs)
        .await
        .map_err(|err| format!("❌ claude failed: {err}"))
}

async fn run_gemini_headless(
    cwd: &Path,
    body: &str,
    timeout_secs: u64,
    approval_mode: &str,
) -> Result<String, String> {
    let output = run_gemini_command(
        cwd,
        &[
            "--prompt",
            body,
            "--output-format",
            "text",
            "--approval-mode",
            approval_mode,
        ],
        timeout_secs,
    )
    .await?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if output.status.success() {
        return Ok(stdout);
    }
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    Err(format!(
        "❌ gemini failed (exit {:?}): {}",
        output.status.code(),
        short_err("stderr", detail)
    ))
}

async fn run_gemini_resume(
    cwd: &Path,
    session_id: &str,
    body: &str,
    timeout_secs: u64,
    approval_mode: &str,
) -> Result<String, String> {
    let output = run_gemini_command(
        cwd,
        &[
            "--resume",
            session_id,
            "--prompt",
            body,
            "--output-format",
            "text",
            "--approval-mode",
            approval_mode,
        ],
        timeout_secs,
    )
    .await?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if output.status.success() {
        return Ok(stdout);
    }
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    Err(format!(
        "❌ gemini resume failed (exit {:?}): {}",
        output.status.code(),
        short_err("stderr", detail)
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AntigravityLanguageServer {
    base_url: String,
    csrf_token: String,
}

async fn run_antigravity_brain_resume<B: BotClient + ?Sized>(
    bot: &B,
    state: StateHandle,
    chat_id: i64,
    repo_id: &str,
    repo_path: &str,
    cascade_id: &str,
    body: &str,
    timeout_secs: u64,
    requested_model: Option<&str>,
) -> Result<String, String> {
    let overview_path = antigravity_overview_path(cascade_id);
    let start_offset = std::fs::metadata(&overview_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    let server = discover_antigravity_language_server(repo_path, cascade_id).await?;
    send_antigravity_user_message(&server, cascade_id, body, requested_model).await?;
    wait_for_antigravity_reply(
        bot,
        state,
        chat_id,
        repo_id,
        &server,
        cascade_id,
        &overview_path,
        start_offset,
        Duration::from_secs(timeout_secs),
    )
    .await
}

fn antigravity_overview_path(cascade_id: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".gemini")
        .join("antigravity")
        .join("brain")
        .join(cascade_id)
        .join(".system_generated")
        .join("logs")
        .join("overview.txt")
}

async fn discover_antigravity_language_server(
    repo_path: &str,
    cascade_id: &str,
) -> Result<AntigravityLanguageServer, String> {
    if let (Ok(base_url), Ok(csrf_token)) = (
        std::env::var("AGENT_BUS_ANTIGRAVITY_LS_URL"),
        std::env::var("AGENT_BUS_ANTIGRAVITY_CSRF_TOKEN"),
    ) {
        return Ok(AntigravityLanguageServer {
            base_url: base_url.trim_end_matches('/').to_string(),
            csrf_token,
        });
    }

    let processes = antigravity_language_server_processes()?;
    let workspace_id = antigravity_workspace_id(repo_path);
    let Some(process) = processes
        .iter()
        .find(|process| process.workspace_id.as_deref() == Some(workspace_id.as_str()))
        .or_else(|| processes.iter().find(|process| process.enable_lsp))
    else {
        return Err("Antigravity language server is not running for this workspace.".to_string());
    };

    let ports = listening_ports_for_pid(process.pid)?;
    if ports.is_empty() {
        return Err(
            "Antigravity language server is running, but no local RPC port was found.".to_string(),
        );
    }

    for port in ports {
        let server = AntigravityLanguageServer {
            base_url: format!("https://127.0.0.1:{port}"),
            csrf_token: process.csrf_token.clone(),
        };
        if antigravity_server_has_cascade(&server, cascade_id).await {
            return Ok(server);
        }
    }

    Err("Could not find an Antigravity RPC port that owns the selected brain session.".to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AntigravityProcess {
    pid: u32,
    csrf_token: String,
    workspace_id: Option<String>,
    enable_lsp: bool,
}

fn antigravity_language_server_processes() -> Result<Vec<AntigravityProcess>, String> {
    let output = std::process::Command::new("ps")
        .args(["-eo", "pid=,args="])
        .output()
        .map_err(|err| format!("Failed to inspect Antigravity processes: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(parse_antigravity_process_line)
        .collect())
}

fn parse_antigravity_process_line(line: &str) -> Option<AntigravityProcess> {
    if !line.contains("language_server_linux_x64") {
        return None;
    }
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let args = parts.collect::<Vec<_>>();
    let csrf_token = arg_value(&args, "--csrf_token")?.to_string();
    Some(AntigravityProcess {
        pid,
        csrf_token,
        workspace_id: arg_value(&args, "--workspace_id").map(ToString::to_string),
        enable_lsp: args.contains(&"--enable_lsp"),
    })
}

fn arg_value<'a>(args: &'a [&str], name: &str) -> Option<&'a str> {
    args.windows(2)
        .find_map(|pair| (pair[0] == name).then_some(pair[1]))
}

fn antigravity_workspace_id(repo_path: &str) -> String {
    format!(
        "file_{}",
        repo_path
            .trim_start_matches('/')
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
            .collect::<String>()
    )
}

fn listening_ports_for_pid(pid: u32) -> Result<Vec<u16>, String> {
    let output = std::process::Command::new("ss")
        .args(["-lptn"])
        .output()
        .map_err(|err| format!("Failed to inspect Antigravity RPC ports: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(|line| parse_listening_port_for_pid(line, pid))
        .collect())
}

fn parse_listening_port_for_pid(line: &str, pid: u32) -> Option<u16> {
    if !line.contains(&format!("pid={pid},")) {
        return None;
    }
    let local_addr = line.split_whitespace().nth(3)?;
    local_addr.rsplit_once(':')?.1.parse::<u16>().ok()
}

async fn antigravity_server_has_cascade(
    server: &AntigravityLanguageServer,
    cascade_id: &str,
) -> bool {
    let client = match antigravity_http_client(Duration::from_secs(3)) {
        Ok(client) => client,
        Err(_) => return false,
    };
    let body = serde_json::json!({ "cascadeId": cascade_id }).to_string();
    let url = format!(
        "{}/exa.language_server_pb.LanguageServerService/GetCascadeTrajectory",
        server.base_url
    );
    client
        .post(url)
        .header("x-codeium-csrf-token", &server.csrf_token)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map(|resp| resp.status().is_success())
        .unwrap_or(false)
}

async fn send_antigravity_user_message(
    server: &AntigravityLanguageServer,
    cascade_id: &str,
    body: &str,
    requested_model: Option<&str>,
) -> Result<(), String> {
    let client = antigravity_http_client(Duration::from_secs(30))?;
    let model_candidates = antigravity_model_candidates(&client, server, requested_model).await?;
    let url = format!(
        "{}/exa.language_server_pb.LanguageServerService/SendUserCascadeMessage",
        server.base_url
    );
    let mut exhausted = Vec::new();
    let mut last_err = None;
    for model in model_candidates {
        let payload = serde_json::json!({
            "cascadeId": cascade_id,
            "items": [{ "text": body }],
            "cascadeConfig": cascade_config_for_requested_model(&model.model_id),
            "blocking": false,
        })
        .to_string();
        let response = client
            .post(&url)
            .header("x-codeium-csrf-token", &server.csrf_token)
            .header("content-type", "application/json")
            .body(payload)
            .send()
            .await
            .map_err(|err| format!("Failed to send message to Antigravity brain RPC: {err}"))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let detail = response.text().await.unwrap_or_default();
        if is_antigravity_quota_exhausted(&detail) {
            exhausted.push(model.label);
            last_err = Some(detail);
            continue;
        }
        return Err(format!(
            "Antigravity brain RPC rejected the message ({status}): {}",
            short_err("body", detail.trim())
        ));
    }
    Err(format!(
        "All attempted Antigravity models were exhausted: {}. Last error: {}",
        exhausted.join(", "),
        short_err("body", last_err.as_deref().unwrap_or(""))
    ))
}

async fn antigravity_model_candidates(
    client: &reqwest::Client,
    server: &AntigravityLanguageServer,
    requested_model: Option<&str>,
) -> Result<Vec<AntigravityModelInfo>, String> {
    let configs = fetch_antigravity_model_configs(client, server).await?;
    if configs.is_empty() {
        return Err("Antigravity returned no usable model configs.".to_string());
    }

    if let Some(model) = requested_model
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        let resolved = resolve_antigravity_model(model, &configs).ok_or_else(|| {
            let avail = configs
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Antigravity model `{model}` not found. Available: {avail}. Use /models_antigravity to refresh."
            )
        })?;
        let selected = configs
            .iter()
            .find(|c| c.model_id == resolved)
            .cloned()
            .ok_or_else(|| format!("Antigravity model `{model}` resolved but disappeared."))?;
        return Ok(std::iter::once(selected)
            .chain(
                configs
                    .into_iter()
                    .filter(|c| c.model_id != resolved)
                    .filter(antigravity_model_has_quota)
                    .collect::<Vec<_>>(),
            )
            .collect());
    }

    if let Ok(model) = std::env::var("AGENT_BUS_ANTIGRAVITY_MODEL") {
        let model = model.trim();
        if !model.is_empty() {
            return Ok(vec![AntigravityModelInfo {
                label: model.to_string(),
                model_id: model.to_string(),
                remaining_fraction: None,
            }]);
        }
    }

    let mut available = configs
        .iter()
        .filter(|c| antigravity_model_has_quota(c))
        .cloned()
        .collect::<Vec<_>>();
    available.sort_by(|a, b| {
        b.remaining_fraction
            .partial_cmp(&a.remaining_fraction)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if available.is_empty() {
        available = configs;
    }
    Ok(available)
}

fn antigravity_model_has_quota(model: &AntigravityModelInfo) -> bool {
    model.remaining_fraction.unwrap_or(0.0) > 0.0
}

fn is_antigravity_quota_exhausted(detail: &str) -> bool {
    detail.contains("RESOURCE_EXHAUSTED") || detail.to_ascii_lowercase().contains("quota")
}

fn cascade_config_for_requested_model(model: &str) -> serde_json::Value {
    serde_json::json!({
        "plannerConfig": {
            "requestedModel": { "model": model }
        }
    })
}

#[derive(Debug, Clone)]
struct PendingAntigravityInteraction {
    /// Trajectory id this step belongs to.
    trajectory_id: String,
    /// Step index of the requestedInteraction in the trajectory.
    step_index: i64,
    /// Top-level interaction kind: "permission", "runCommand", "fileEdit", etc.
    kind: String,
    /// Short human-readable summary for the Telegram prompt.
    summary: String,
}

/// Walk the trajectory and return interactions that have a `requestedInteraction`
/// but no `completedInteractions` yet — those are pending user decisions.
async fn find_pending_antigravity_interactions(
    client: &reqwest::Client,
    server: &AntigravityLanguageServer,
    cascade_id: &str,
) -> Result<Vec<PendingAntigravityInteraction>, String> {
    let url = format!(
        "{}/exa.language_server_pb.LanguageServerService/GetCascadeTrajectory",
        server.base_url
    );
    let body = serde_json::json!({ "cascadeId": cascade_id }).to_string();
    let response = client
        .post(url)
        .header("x-codeium-csrf-token", &server.csrf_token)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|err| format!("Failed to fetch cascade trajectory: {err}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "GetCascadeTrajectory failed ({status}): {}",
            short_err("body", text.trim())
        ));
    }
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|err| format!("decode trajectory: {err}"))?;
    let trajectory = value.get("trajectory").cloned().unwrap_or_default();
    let traj_id = trajectory
        .get("trajectoryId")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let steps = trajectory
        .get("steps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for step in steps {
        let Some(req) = step.get("requestedInteraction") else {
            continue;
        };
        // Already responded? (completed list non-empty AND it covers the request)
        let completed = step
            .get("completedInteractions")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if completed {
            continue;
        }
        let step_index = step
            .get("metadata")
            .and_then(|m| m.get("sourceTrajectoryStepInfo"))
            .and_then(|info| info.get("stepIndex"))
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        if step_index < 0 {
            continue;
        }
        let Some((kind, sub)) = req
            .as_object()
            .and_then(|map| map.iter().next())
            .map(|(k, v)| (k.clone(), v.clone()))
        else {
            continue;
        };
        let summary = summarize_antigravity_interaction(&kind, &sub);
        out.push(PendingAntigravityInteraction {
            trajectory_id: traj_id.clone(),
            step_index,
            kind,
            summary,
        });
    }
    Ok(out)
}

fn summarize_antigravity_interaction(kind: &str, payload: &serde_json::Value) -> String {
    match kind {
        "permission" => {
            let action = payload
                .get("resource")
                .and_then(|r| r.get("action"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let target = payload
                .get("resource")
                .and_then(|r| r.get("target"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("{action} {target}")
        }
        "runCommand" => {
            let cmd = payload
                .get("command")
                .and_then(|v| v.as_str())
                .or_else(|| payload.get("commandLine").and_then(|v| v.as_str()))
                .unwrap_or("(unknown command)");
            format!("run: {cmd}")
        }
        other => format!("{other}: {}", short_err("payload", &payload.to_string())),
    }
}

/// Send the user's decision back to the Antigravity language server.
///
/// The verified request shape (probed live against the LS) is:
///   { cascadeId, interaction: { trajectoryId, stepIndex, <kind>: <payload> } }
/// — the verdict travels in the same kind-tagged field as the request.
/// run_command approvals are routed through the permission interaction
/// (action="command"), so we use a single payload shape for all kinds we
/// have observed; other kinds use a best-effort `{allow: bool}` payload.
async fn respond_to_antigravity_interaction(
    client: &reqwest::Client,
    server: &AntigravityLanguageServer,
    cascade_id: &str,
    pending: &PendingAntigravityInteraction,
    allow: bool,
) -> Result<(), String> {
    let kind = pending.kind.as_str();
    let inner = match kind {
        "permission" => serde_json::json!({
            "allow": allow,
            "scope": "PERMISSION_SCOPE_WORKSPACE",
        }),
        _ => serde_json::json!({ "allow": allow }),
    };
    let body = serde_json::json!({
        "cascadeId": cascade_id,
        "interaction": {
            "trajectoryId": pending.trajectory_id,
            "stepIndex": pending.step_index,
            kind: inner,
        },
    })
    .to_string();
    let url = format!(
        "{}/exa.language_server_pb.LanguageServerService/HandleCascadeUserInteraction",
        server.base_url
    );
    let response = client
        .post(url)
        .header("x-codeium-csrf-token", &server.csrf_token)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|err| format!("send approval: {err}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "HandleCascadeUserInteraction failed ({status}): {}",
            short_err("body", text.trim())
        ));
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct AntigravityModelInfo {
    label: String,
    model_id: String,
    remaining_fraction: Option<f64>,
}

async fn fetch_antigravity_model_configs(
    client: &reqwest::Client,
    server: &AntigravityLanguageServer,
) -> Result<Vec<AntigravityModelInfo>, String> {
    let url = format!(
        "{}/exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData",
        server.base_url
    );
    let response = client
        .post(url)
        .header("x-codeium-csrf-token", &server.csrf_token)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .map_err(|err| format!("Failed to read Antigravity model config: {err}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "Antigravity model config RPC failed ({status}): {}",
            short_err("body", text.trim())
        ));
    }
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| format!("Failed to decode Antigravity model config: {err}"))?;
    let configs = value
        .get("clientModelConfigs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let out = configs
        .into_iter()
        .filter_map(|c| {
            let label = c.get("label")?.as_str()?.to_string();
            let model_id = c
                .get("modelOrAlias")
                .and_then(|m| m.get("model").and_then(|v| v.as_str()))
                .or_else(|| {
                    c.get("modelOrAlias")
                        .and_then(|m| m.get("alias").and_then(|v| v.as_str()))
                })?
                .to_string();
            let remaining_fraction = c
                .get("quotaInfo")
                .and_then(|q| q.get("remainingFraction"))
                .and_then(|v| v.as_f64());
            Some(AntigravityModelInfo {
                label,
                model_id,
                remaining_fraction,
            })
        })
        .collect();
    Ok(out)
}

/// Resolve a user-provided model identifier against the live config list.
/// Accepts:
///   - exact `MODEL_*` enum
///   - exact label match (case-insensitive)
///   - normalized substring of label (so "flash" matches "Gemini 3 Flash",
///     "gemini-3-flash" matches "Gemini 3 Flash", etc.)
fn resolve_antigravity_model(input: &str, configs: &[AntigravityModelInfo]) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Exact enum match
    if let Some(c) = configs.iter().find(|c| c.model_id == trimmed) {
        return Some(c.model_id.clone());
    }
    let needle = normalize_for_model_match(trimmed);
    if needle.is_empty() {
        return None;
    }
    // Exact normalized label
    if let Some(c) = configs
        .iter()
        .find(|c| normalize_for_model_match(&c.label) == needle)
    {
        return Some(c.model_id.clone());
    }
    // Substring of normalized label
    configs
        .iter()
        .find(|c| normalize_for_model_match(&c.label).contains(&needle))
        .map(|c| c.model_id.clone())
}

fn normalize_for_model_match(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_space = true;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            for c in ch.to_lowercase() {
                out.push(c);
            }
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

fn antigravity_http_client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|err| format!("Failed to create Antigravity RPC client: {err}"))
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_antigravity_reply<B: BotClient + ?Sized>(
    bot: &B,
    state: StateHandle,
    chat_id: i64,
    repo_id: &str,
    server: &AntigravityLanguageServer,
    cascade_id: &str,
    overview_path: &Path,
    start_offset: u64,
    timeout: Duration,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut current: Option<String> = None;
    let mut last_change = tokio::time::Instant::now();
    let stable_threshold = Duration::from_secs(5);
    let approval_client = antigravity_http_client(Duration::from_secs(15)).ok();
    let mut announced: HashSet<(String, i64)> = HashSet::new();
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Ok(current.unwrap_or_else(|| {
                "Đã gửi message vào Antigravity brain session, nhưng chưa thấy phản hồi hoàn tất trong overview log.".to_string()
            }));
        }

        // Detect pending approval interactions and forward to Telegram.
        if let Some(client) = approval_client.as_ref() {
            if let Ok(pending) =
                find_pending_antigravity_interactions(client, server, cascade_id).await
            {
                for p in pending {
                    let key = (p.trajectory_id.clone(), p.step_index);
                    if announced.contains(&key) {
                        continue;
                    }
                    announced.insert(key);
                    if let Err(err) =
                        announce_antigravity_approval(bot, &state, chat_id, repo_id, cascade_id, &p)
                            .await
                    {
                        tracing::warn!(
                            target: "agent_bus::antigravity_approval",
                            error = %err,
                            "failed to announce pending approval to Telegram"
                        );
                    }
                    // Reset stability timer so we don't return prematurely while
                    // the user is still deciding.
                    last_change = tokio::time::Instant::now();
                }
            }
        }

        if let Ok(text) = read_file_tail_from(overview_path, start_offset) {
            let summary = aggregate_antigravity_state(&text);
            if summary.is_some() && summary != current {
                current = summary;
                last_change = tokio::time::Instant::now();
            }
            if let Some(text_reply) = current
                .as_deref()
                .and_then(antigravity_summary_final_text_only)
            {
                return Ok(text_reply.to_string());
            }
            if current.is_some()
                && announced.is_empty()
                && !current
                    .as_deref()
                    .is_some_and(antigravity_summary_is_waiting_for_approval)
                && last_change.elapsed() >= stable_threshold
            {
                return Ok(current.unwrap());
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Generate a short approval id, persist the pending entry in state, and
/// send a Telegram message with Approve / Deny inline buttons.
async fn announce_antigravity_approval<B: BotClient + ?Sized>(
    bot: &B,
    state: &StateHandle,
    chat_id: i64,
    repo_id: &str,
    cascade_id: &str,
    pending: &PendingAntigravityInteraction,
) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let now = time::OffsetDateTime::now_utc();
    let created_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| now.unix_timestamp().to_string());
    let approval_id = {
        let mut h = Sha256::new();
        h.update(cascade_id.as_bytes());
        h.update(b":");
        h.update(pending.trajectory_id.as_bytes());
        h.update(b":");
        h.update(pending.step_index.to_string().as_bytes());
        let digest = h.finalize();
        // 12 hex chars = 6 bytes — short enough for Telegram callback data
        // (limit 64 bytes) and unique enough across active approvals.
        digest[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };

    // Cross-task dedupe: each @antigravity message spawns its own wait task
    // with its own per-task announce HashSet. When several tasks are alive
    // concurrently (e.g. an earlier turn is still polling for completion
    // when a new turn starts), they all observe the same pending step. The
    // state actor's insert-if-absent semantics serialize the check, so only
    // the first caller actually inserts and announces; the rest skip.
    let entry = agent_bus_core::state::PendingAntigravityApprovalEntry {
        chat_id: chat_id.to_string(),
        repo_id: repo_id.to_string(),
        cascade_id: cascade_id.to_string(),
        trajectory_id: pending.trajectory_id.clone(),
        step_index: pending.step_index,
        kind: pending.kind.clone(),
        summary: pending.summary.clone(),
        created_at,
    };
    let inserted = state
        .insert_pending_antigravity_approval(approval_id.clone(), entry)
        .await
        .map_err(|err| format!("save approval: {err}"))?;
    if !inserted {
        return Ok(());
    }

    let text = format!(
        "🛡️ Antigravity đang xin approval:\n\n• kind: {}\n• {}\n\nClick để quyết định:",
        pending.kind, pending.summary
    );
    let keyboard = InlineKeyboard {
        rows: vec![vec![
            (
                "✅ Approve".to_string(),
                format!("ant_appr:{approval_id}:y"),
            ),
            ("❌ Deny".to_string(), format!("ant_appr:{approval_id}:n")),
        ]],
    };
    bot.send_message(chat_id, text, Some(keyboard))
        .await
        .map_err(|err| format!("send approval prompt: {err}"))?;
    Ok(())
}

/// Returns the trailing text answer from a summary if the agent ended with
/// real text (no awaiting-approval markers). Used so we exit the wait loop
/// immediately when a definitive answer arrives.
fn antigravity_summary_final_text_only(summary: &str) -> Option<&str> {
    if antigravity_summary_is_waiting_for_approval(summary) || summary.contains("Agent thực thi")
    {
        return None;
    }
    Some(summary)
}

fn antigravity_summary_is_waiting_for_approval(summary: &str) -> bool {
    summary.contains("⏳") || summary.contains("Agent đề xuất")
}

fn aggregate_antigravity_state(text: &str) -> Option<String> {
    let mut text_replies: Vec<String> = Vec::new();
    let mut tool_summaries: Vec<String> = Vec::new();
    let mut needs_approval = false;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("source").and_then(|v| v.as_str()) != Some("MODEL") {
            continue;
        }
        if let Some(content) = value.get("content").and_then(|v| v.as_str()) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                text_replies.push(trimmed.to_string());
                continue;
            }
        }
        let Some(tool_calls) = value.get("tool_calls").and_then(|v| v.as_array()) else {
            continue;
        };
        for tc in tool_calls {
            let Some(name) = tc.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let args = tc.get("args").cloned().unwrap_or_default();
            let detail = args
                .get("CommandLine")
                .and_then(|v| v.as_str())
                .or_else(|| args.get("TargetFile").and_then(|v| v.as_str()))
                .or_else(|| args.get("Description").and_then(|v| v.as_str()))
                .map(|s| s.trim_matches('"').to_string())
                .unwrap_or_default();
            let unsafe_flag = args
                .get("SafeToAutoRun")
                .and_then(|v| v.as_str())
                .map(|s| s.trim_matches('"').eq_ignore_ascii_case("false"))
                .unwrap_or(false);
            if unsafe_flag {
                needs_approval = true;
                tool_summaries.push(format!("⏳ chờ approval — `{name}`: {detail}"));
            } else if detail.is_empty() {
                tool_summaries.push(format!("• `{name}`"));
            } else {
                tool_summaries.push(format!("• `{name}`: {detail}"));
            }
        }
    }
    let mut parts = Vec::new();
    if !tool_summaries.is_empty() {
        let header = if needs_approval {
            "Agent đề xuất tool calls (chấp nhận/từ chối trên Antigravity desktop):"
        } else {
            "Agent thực thi tool calls:"
        };
        parts.push(format!("{header}\n{}", tool_summaries.join("\n")));
    }
    if !text_replies.is_empty() {
        parts.push(text_replies.join("\n\n"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

fn read_file_tail_from(path: &Path, start_offset: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek};

    let mut file = std::fs::File::open(path)?;
    file.seek(std::io::SeekFrom::Start(start_offset))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

async fn list_gemini_sessions(
    cwd: &Path,
    limit: usize,
) -> Result<Vec<session_bridge::GeminiSessionInfo>, String> {
    let output = run_gemini_command(cwd, &["--list-sessions"], 15).await?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        return Err(format!(
            "Failed to list Gemini sessions: {}",
            short_err("stderr", detail)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(session_bridge::parse_gemini_list_sessions(&stdout, limit))
}

async fn run_gemini_command(
    cwd: &Path,
    args: &[&str],
    timeout_secs: u64,
) -> Result<std::process::Output, String> {
    tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        tokio::process::Command::new(gemini_bin_path())
            .args(args)
            .current_dir(cwd)
            .kill_on_drop(true)
            .output()
            .await
    })
    .await
    .map_err(|_| format!("❌ gemini timed out after {timeout_secs}s"))?
    .map_err(|err| format!("❌ gemini failed to start: {err}"))
}

async fn run_claude_via_runner(
    runner: &Arc<crate::daemon::runner::AgentRunner<crate::daemon::cli_spawner::CliSpawner>>,
    repo_id: &str,
    repo_path: PathBuf,
    mobile_uuid: String,
    prompt: String,
    timeout: Duration,
    chat_id: i64,
) -> Result<String, String> {
    let request_id = format!(
        "req_{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    let req = AgentRunRequest {
        agent: "claude".to_string(),
        repo_id: repo_id.to_string(),
        repo_path,
        prompt,
        mode: AgentRunMode::ClaudeResume { mobile_uuid },
        preferred_context: None,
        timeout,
        request_id,
        chat_id: Some(chat_id),
    };
    match runner.run(req).await {
        Ok(resp) => match resp.final_kind {
            agent_bus_core::classifier::ResultKind::Success => Ok(resp.stdout),
            agent_bus_core::classifier::ResultKind::QuotaExhausted => Err(format!(
                "⚠️ claude/{} is out of quota. All usable contexts exhausted. Run /quota claude for details.",
                resp.auth_context
            )),
            agent_bus_core::classifier::ResultKind::RateLimited => Err(format!(
                "⚠️ claude/{} is rate-limited. Retry shortly or rotate with /auth_use claude <id>.",
                resp.auth_context
            )),
            agent_bus_core::classifier::ResultKind::AuthExpired
            | agent_bus_core::classifier::ResultKind::ManualReauthRequired => Err(format!(
                "🔒 claude/{} needs re-auth. Run /reauth claude {} on the host machine.",
                resp.auth_context, resp.auth_context
            )),
            _ => Err(format!(
                "❌ claude failed ({:?}): {}",
                resp.final_kind, resp.stderr_excerpt
            )),
        },
        Err(RunnerError::NoUsableContexts { agent }) => Err(format!(
            "All {agent} auth contexts are unavailable. Run /quota {agent} for details."
        )),
        Err(RunnerError::ApprovalPending {
            agent,
            id,
            request_id,
        }) => Err(format!(
            "⏸ Rotation to {agent}/{id} needs owner approval. Use /auth_approve {request_id} or /auth_deny {request_id}."
        )),
        Err(err) => Err(format!("❌ claude failed: {err}")),
    }
}

async fn run_agent_via_runner(
    runner: &Arc<crate::daemon::runner::AgentRunner<crate::daemon::cli_spawner::CliSpawner>>,
    agent: AgentKind,
    input: AgentRunInput,
) -> Result<String, String> {
    let request_id = format!(
        "req_{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    let req = AgentRunRequest {
        agent: agent.to_string(),
        repo_id: input.repo_id,
        repo_path: input.repo_path,
        prompt: input.prompt,
        mode: input.mode,
        preferred_context: None,
        timeout: input.timeout,
        request_id,
        chat_id: Some(input.chat_id),
    };
    match runner.run(req).await {
        Ok(resp) => match resp.final_kind {
            agent_bus_core::classifier::ResultKind::Success => Ok(resp.stdout),
            agent_bus_core::classifier::ResultKind::QuotaExhausted => Err(format!(
                "⚠️ {agent}/{} is out of quota. All usable contexts exhausted. Run /quota {agent} for details.",
                resp.auth_context
            )),
            agent_bus_core::classifier::ResultKind::RateLimited => Err(format!(
                "⚠️ {agent}/{} is rate-limited. Retry shortly or rotate with /auth_use {agent} <id>.",
                resp.auth_context
            )),
            agent_bus_core::classifier::ResultKind::AuthExpired
            | agent_bus_core::classifier::ResultKind::ManualReauthRequired => Err(format!(
                "🔒 {agent}/{} needs re-auth. Run /reauth {agent} {} on the host machine.",
                resp.auth_context, resp.auth_context
            )),
            _ => Err(format!(
                "❌ {agent} failed ({:?}): {}",
                resp.final_kind, resp.stderr_excerpt
            )),
        },
        Err(RunnerError::NoUsableContexts { agent }) => Err(format!(
            "All {agent} auth contexts are unavailable. Run /quota {agent} for details."
        )),
        Err(RunnerError::ApprovalPending {
            agent,
            id,
            request_id,
        }) => Err(format!(
            "⏸ Rotation to {agent}/{id} needs owner approval. Use /auth_approve {request_id} or /auth_deny {request_id}."
        )),
        Err(err) => Err(format!("❌ {agent} failed: {err}")),
    }
}

// ── helpers ───────────────────────────────────────────────────────────

#[cfg(test)]
fn project_hash_for_repo(repo_path: &str) -> String {
    repo_path.replace('/', "-")
}

#[cfg(test)]
fn cwd_matches_repo(session_cwd: &str, repo_path: &str) -> bool {
    let normalize = |s: &str| s.trim_end_matches('/').to_string();
    normalize(session_cwd) == normalize(repo_path)
}

fn claude_bin_path() -> String {
    std::env::var("AGENT_BUS_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string())
}

fn gemini_bin_path() -> String {
    std::env::var("AGENT_BUS_GEMINI_BIN").unwrap_or_else(|_| "gemini".to_string())
}

fn gemini_approval_mode() -> String {
    std::env::var("AGENT_BUS_GEMINI_APPROVAL_MODE").unwrap_or_else(|_| "plan".to_string())
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

fn is_bare_uuid(s: &str) -> bool {
    s.len() == 36 && s.bytes().filter(|&b| b == b'-').count() == 4
}

fn is_stale_gemini_session_error(error_msg: &str) -> bool {
    error_msg.contains("Invalid session identifier") || error_msg.contains("Error resuming session")
}

fn is_session_name_query(body: &str) -> bool {
    let lower = body.to_lowercase();
    let asks_name = lower.contains("tên")
        || lower.contains("ten")
        || lower.contains("name")
        || lower.contains("title");
    let asks_session = lower.contains("session") || lower.contains("phiên");
    asks_name && asks_session
}

fn is_model_query(body: &str) -> bool {
    let lower = body.to_lowercase();
    let asks_model =
        lower.contains("model") || lower.contains("mô hình") || lower.contains("mo hinh");
    let asks_current = lower.contains("hiện tại")
        || lower.contains("hien tai")
        || lower.contains("current")
        || lower.contains("đang dùng")
        || lower.contains("dang dung");
    asks_model && asks_current
}

fn make_session_label(session_id: &str, path: &str) -> String {
    session_label_from(None, session_id, path)
}

fn session_label_for(bridge: &BridgedSessionState) -> String {
    session_label_from(
        bridge.display_name.as_deref(),
        &bridge.desktop_session_id,
        &bridge.desktop_path,
    )
}

fn session_label_from(display_name: Option<&str>, session_id: &str, path: &str) -> String {
    if let Some(name) = display_name {
        if !name.trim().is_empty() {
            return truncate_session_label(name);
        }
    }
    let stem = std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let label = if !stem.is_empty() && !is_bare_uuid(&stem) {
        stem
    } else if !session_id.is_empty() {
        session_id[..session_id.len().min(8)].to_string()
    } else {
        "session".to_string()
    };
    truncate_session_label(&label)
}

fn truncate_session_label(label: &str) -> String {
    if label.chars().count() > 20 {
        let truncated: String = label.chars().take(17).collect();
        format!("{truncated}...")
    } else {
        label.to_string()
    }
}

fn agent_header(agent_display: &str, session_label: &str) -> String {
    format!("[{agent_display} - {session_label}]\n")
}

#[allow(dead_code)]
fn _session_info_dummy() -> SessionInfo {
    SessionInfo {
        uuid: String::new(),
        path: PathBuf::new(),
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
            "No default repo for this chat. Use /switch_rp or @agent:repo msg"
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
    command: &str,
    command_hash: &str,
    matched_pattern: &str,
) -> Result<Option<(MessageRef, String)>, TelegramError> {
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

    // Show the actual command so the user can decide. Telegram messages are
    // capped at ~4096 chars but we trim much earlier to avoid bloat from
    // pathological prompts; the full command is still recoverable via the
    // hash + state if needed for audit.
    const MAX_CMD: usize = 800;
    let cmd_display = if command.len() > MAX_CMD {
        format!("{}…(truncated, {} bytes)", &command[..MAX_CMD], command.len())
    } else {
        command.to_string()
    };

    let text = format!(
        "Permission requested\nRepo: {repo}\nCommand:\n```\n{cmd_display}\n```\nMatched: {matched_pattern}\nHash: {short_hash}",
        short_hash = &command_hash.get(..16).unwrap_or(command_hash),
    );
    let keyboard = InlineKeyboard {
        rows: vec![vec![
            ("Approve".to_string(), format!("perm:approve:{perm_id}")),
            ("Deny".to_string(), format!("perm:deny:{perm_id}")),
        ]],
    };

    let message = bot
        .send_message(chat_id, text.clone(), Some(keyboard))
        .await?;
    Ok(Some((message, text)))
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
            PendingPermStatus::ApprovedByTelegram,
            crate::daemon::perm::PermVerdict::Approve,
            "Approved",
        ),
        "deny" => (
            PendingPermStatus::DeniedByTelegram,
            crate::daemon::perm::PermVerdict::Deny,
            "Denied",
        ),
        _ => return Err(TelegramError::InvalidCallback(callback_data)),
    };

    match state
        .resolve_pending_if_open(perm_id.to_string(), status)
        .await?
    {
        ResolvePendingOutcome::Resolved => {
            registry.resolve(perm_id, verdict).await;
        }
        ResolvePendingOutcome::AlreadyResolved(existing) => {
            bot.answer_callback(
                callback_id,
                format!("Already resolved: {}", pending_perm_status_label(existing)),
            )
            .await?;
            return Ok(());
        }
        ResolvePendingOutcome::Missing => {
            bot.answer_callback(callback_id, "Permission request not found".to_string())
                .await?;
            return Ok(());
        }
    }

    let snapshot = state.snapshot().await;
    if snapshot
        .pending_perms
        .get(perm_id)
        .and_then(|perm| perm.message_id)
        == Some(message.message_id)
    {
        let actor = user_name.unwrap_or_else(|| "user".to_string());
        let icon = match action {
            "approve" => "✅",
            "deny" => "❌",
            _ => "•",
        };
        let status_line = format!("\n\n{icon} {text} by @{actor}");
        let new_text = match snapshot
            .pending_perms
            .get(perm_id)
            .and_then(|perm| perm.prompt_text.as_deref())
        {
            Some(original) => format!("{original}{status_line}"),
            None => format!("{text} by @{actor}"),
        };
        bot.edit_message_text(message, new_text).await?;
    }

    bot.answer_callback(callback_id, text.to_string()).await
}

fn pending_perm_status_label(status: PendingPermStatus) -> &'static str {
    pending_perm_status_text(status)
}

pub fn pending_perm_status_text(status: PendingPermStatus) -> &'static str {
    match status {
        PendingPermStatus::Pending => "pending",
        PendingPermStatus::Sent => "sent",
        PendingPermStatus::Approved => "approved",
        PendingPermStatus::Denied => "denied",
        PendingPermStatus::TimedOut => "timed_out",
        PendingPermStatus::ApprovedByTelegram => "approved_by_telegram",
        PendingPermStatus::DeniedByTelegram => "denied_by_telegram",
        PendingPermStatus::ApprovedByDesktop => "approved_by_desktop",
        PendingPermStatus::DeniedByDesktop => "denied_by_desktop",
        PendingPermStatus::Cancelled => "cancelled",
        PendingPermStatus::Superseded => "superseded",
    }
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

fn repo_by_path<'a>(config: &'a TelegramConfig, repo_path: &str) -> Option<&'a RepoEntry> {
    let target = repo_path.trim_end_matches('/');
    config
        .repos
        .iter()
        .find(|repo| repo.path.trim_end_matches('/') == target)
}

/// Match a candidate path to a registered repo when the candidate is the repo
/// path itself OR a directory inside it. Returns the longest-matching repo so
/// nested registrations resolve to the most specific one.
fn repo_by_path_prefix<'a>(config: &'a TelegramConfig, candidate: &str) -> Option<&'a RepoEntry> {
    let target = candidate.trim_end_matches('/');
    config
        .repos
        .iter()
        .filter(|repo| {
            let r = repo.path.trim_end_matches('/');
            target == r || target.starts_with(&format!("{r}/"))
        })
        .max_by_key(|repo| repo.path.trim_end_matches('/').len())
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

    pub fn answered_callbacks(&self) -> Vec<String> {
        self.callbacks
            .lock()
            .expect("mock bot lock poisoned")
            .clone()
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
    agent_runner: SharedAgentRunner,
) -> ResponseResult<()> {
    let client = TeloxideBotClient::new(bot);
    if let Some(text) = msg.text() {
        let chat_id = msg.chat.id.0;
        let username = msg.from.as_ref().and_then(|user| user.username.as_deref());
        if let Some(BridgeCommand::Chat(AgentKind::Antigravity, body)) =
            session_bridge::parse_bridge_command(text)
        {
            let config = Arc::clone(&config);
            tokio::spawn(async move {
                if let Err(err) =
                    handle_antigravity_bridge_msg(&client, &config, state, chat_id, body).await
                {
                    tracing::warn!(
                        target: "agent_bus::telegram",
                        error = %err,
                        "background antigravity bridge message failed"
                    );
                }
            });
            return Ok(());
        }
        handle_text_command(
            &client,
            &config,
            state,
            &auth_contexts,
            chat_id,
            username,
            text,
            agent_runner.as_ref(),
        )
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
    agent_runner: SharedAgentRunner,
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
    } else if data.starts_with("sel_codex:") {
        handle_callback_sel_codex(
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
    } else if data.starts_with("sel_gemini:") {
        handle_callback_sel_gemini(
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
    } else if data.starts_with("sel_antigravity:") {
        handle_callback_sel_antigravity(
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
    } else if data.starts_with("help:") {
        handle_callback_help(
            &client,
            &config,
            state,
            &auth_contexts,
            chat.id.0,
            query.id,
            &data,
            agent_runner.as_ref(),
        )
        .await
        .map_err(to_teloxide_error)?;
    } else if data.starts_with("ant_appr:") {
        handle_callback_antigravity_approval(
            &client,
            &config,
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
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_callback_help<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    auth_contexts: &Option<AuthContextsConfig>,
    chat_id: i64,
    callback_id: String,
    data: &str,
    agent_runner: Option<
        &Arc<crate::daemon::runner::AgentRunner<crate::daemon::cli_spawner::CliSpawner>>,
    >,
) -> Result<(), TelegramError> {
    let cmd = data.strip_prefix("help:").unwrap_or("");
    match cmd {
        "current" => {
            bot.answer_callback(callback_id, String::new()).await?;
            handle_current_command(bot, config, state, chat_id).await
        }
        "switch_rp" => {
            bot.answer_callback(callback_id, String::new()).await?;
            handle_switch_rp_picker(bot, config, state, chat_id).await
        }
        "auth_list" => {
            bot.answer_callback(callback_id, String::new()).await?;
            if let Some(cfg) = auth_contexts {
                auth_cmds::handle_auth_list_command(bot, state, cfg, chat_id).await
            } else {
                bot.send_message(
                    chat_id,
                    "Auth contexts not configured (legacy mode)".to_string(),
                    None,
                )
                .await?;
                Ok(())
            }
        }
        "quota" => {
            bot.answer_callback(callback_id, "Usage: /quota <agent>".to_string())
                .await
        }
        "auth_rotate" => {
            bot.answer_callback(callback_id, "Usage: /auth_rotate <agent>".to_string())
                .await
        }
        "lead" => {
            bot.answer_callback(callback_id, String::new()).await?;
            if !is_allowed(config, chat_id) {
                return Ok(());
            }
            if let Some(cfg) = auth_contexts {
                auth_cmds::handle_lead_command(bot, state, cfg, chat_id, None).await
            } else {
                handle_legacy_lead_command(bot, state, chat_id, None).await
            }
        }
        "lead_default" => {
            bot.answer_callback(callback_id, "Usage: /lead_default <agent>".to_string())
                .await
        }
        "lead_clear" => {
            bot.answer_callback(callback_id, String::new()).await?;
            if !is_allowed(config, chat_id) {
                return Ok(());
            }
            auth_cmds::handle_lead_clear_command(bot, state, chat_id).await
        }
        "list_claude" => {
            bot.answer_callback(callback_id, String::new()).await?;
            handle_bridge_command(
                bot,
                config,
                state,
                chat_id,
                BridgeCommand::List(AgentKind::Claude),
                agent_runner,
            )
            .await
        }
        "list_codex" => {
            bot.answer_callback(callback_id, String::new()).await?;
            handle_bridge_command(
                bot,
                config,
                state,
                chat_id,
                BridgeCommand::List(AgentKind::Codex),
                agent_runner,
            )
            .await
        }
        "list_gemini" => {
            bot.answer_callback(callback_id, String::new()).await?;
            handle_bridge_command(
                bot,
                config,
                state,
                chat_id,
                BridgeCommand::List(AgentKind::Gemini),
                agent_runner,
            )
            .await
        }
        "list_antigravity" => {
            bot.answer_callback(callback_id, String::new()).await?;
            handle_bridge_command(
                bot,
                config,
                state,
                chat_id,
                BridgeCommand::List(AgentKind::Antigravity),
                agent_runner,
            )
            .await
        }
        _ => {
            bot.answer_callback(callback_id, String::new()).await?;
            Ok(())
        }
    }
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
    use base64::Engine;
    use std::io::Write;
    use std::sync::Mutex;
    use tempfile::tempdir;

    static CODEX_OVERRIDE_TEST_LOCK: Mutex<()> = Mutex::new(());
    static GEMINI_OVERRIDE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_config() -> TelegramConfig {
        config_with_repo_path("/tmp/sample_repo-test")
    }

    fn config_with_repo_path(path: impl Into<String>) -> TelegramConfig {
        TelegramConfig {
            allowed_chats: vec!["100".to_string()],
            repos: vec![RepoEntry {
                id: "sample_repo".to_string(),
                display: "SampleRepo".to_string(),
                path: path.into(),
                agents: vec![
                    "claude".to_string(),
                    "codex".to_string(),
                    "gemini".to_string(),
                    "antigravity".to_string(),
                ],
                codex_mode: CodexMode::LiveBridge,
            }],
        }
    }

    fn restore_env(key: &str, value: Option<String>) {
        if let Some(value) = value {
            std::env::set_var(key, value);
        } else {
            std::env::remove_var(key);
        }
    }

    fn write_fake_gemini_script(path: &Path) {
        std::fs::write(
            path,
            "#!/bin/sh\n\
if [ \"$1\" = \"--list-sessions\" ]; then\n\
  echo \"Available sessions for this project (2):\"\n\
  echo \"  1. Session One (3 minutes ago) [gem-11111111]\"\n\
  echo \"  2. Session Two (2 hours ago) [gem-22222222]\"\n\
  exit 0\n\
fi\n\
if [ \"$1\" = \"--resume\" ]; then\n\
  session=\"$2\"\n\
  shift 2\n\
  echo \"gemini resume: $session $*\"\n\
  echo \"cwd=$PWD\"\n\
  exit 0\n\
fi\n\
echo \"gemini headless: $*\"\n\
echo \"cwd=$PWD\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn write_stale_gemini_script(path: &Path) {
        std::fs::write(
            path,
            "#!/bin/sh\n\
if [ \"$1\" = \"--resume\" ]; then\n\
  echo \"Error resuming session: Invalid session identifier $2\" >&2\n\
  exit 1\n\
fi\n\
echo \"unexpected headless fallback: $*\"\n\
exit 0\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn write_codex_session(codex_home: &Path, id: &str, repo_path: &Path) -> PathBuf {
        let path = codex_home
            .join("sessions/2026/04/19")
            .join(format!("rollout-test-{id}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{id}","cwd":"{}"}}}}"#,
                repo_path.display()
            ),
        )
        .unwrap();
        path
    }

    fn write_claude_session(
        home: &Path,
        project_key: &str,
        session_id: &str,
        repo_path: &Path,
        prompt: &str,
    ) -> PathBuf {
        let project_dir = home.join(".claude/projects").join(project_key);
        std::fs::create_dir_all(&project_dir).unwrap();
        let path = project_dir.join(format!("{session_id}.jsonl"));
        std::fs::write(
            &path,
            format!(
                concat!(
                    r#"{{"type":"summary","sessionId":"{}","summary":"stub"}}"#,
                    "\n",
                    r#"{{"type":"user","sessionId":"{}","cwd":"{}","timestamp":"2026-04-19T00:00:00Z","message":{{"role":"user","content":"{}"}}}}"#
                ),
                session_id,
                session_id,
                repo_path.display(),
                prompt
            ),
        )
        .unwrap();
        path
    }

    fn write_antigravity_db(path: &Path, repo_path: &Path, session_id: &str, title: &str) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
        let entry = proto_field(
            1,
            &proto_message([
                proto_field(1, session_id.as_bytes()),
                proto_field(
                    2,
                    base64::engine::general_purpose::STANDARD
                        .encode(proto_message([proto_field(1, title.as_bytes())]))
                        .as_bytes(),
                ),
                proto_field(
                    5,
                    &proto_message([proto_field(
                        1,
                        format!("file://{}", repo_path.display()).as_bytes(),
                    )]),
                ),
            ]),
        );
        let encoded = base64::engine::general_purpose::STANDARD.encode(entry);
        conn.execute(
            "INSERT INTO ItemTable(key, value) VALUES (?1, ?2)",
            ("antigravityUnifiedStateSync.trajectorySummaries", encoded),
        )
        .unwrap();
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

    #[tokio::test]
    async fn list_claude_lists_all_sessions_with_repo_prefix() {
        let bot = MockBot::default();
        let config = test_config();
        let dir = tempdir().unwrap();
        let repo_path = dir.path().join("sample-repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let home = dir.path().join("home");
        write_claude_session(
            &home,
            "-tmp-sample-repo",
            "11111111-1111-1111-1111-111111111111",
            &repo_path,
            "Help me wire Telegram bridge",
        );
        let old_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &home);
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();

        handle_text_command(&bot, &config, state, &None, 100, None, "/list_claude", None)
            .await
            .unwrap();
        restore_env("HOME", old_home);

        let sent = bot.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].text, "Active Claude sessions:");
        let keyboard = sent[0].keyboard.as_ref().expect("keyboard");
        assert_eq!(keyboard.rows.len(), 1);
        let label = &keyboard.rows[0][0].0;
        assert!(label.contains(&repo_short_label_from_source(
            &repo_path.display().to_string()
        )));
        assert!(label.contains("Help me wire Telegram bridge"));
    }

    #[tokio::test]
    async fn claude_msg_without_selected_session_sends_guard_warning() {
        let bot = MockBot::default();
        let config = test_config();
        let dir = tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();

        handle_text_command(
            &bot,
            &config,
            state.clone(),
            &None,
            100,
            None,
            "@claude hello world",
            None,
        )
        .await
        .unwrap();

        let sent = bot.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(
            sent[0].text.contains("No Claude session selected"),
            "expected guard message, got: {:?}",
            sent[0].text
        );
    }

    #[tokio::test]
    async fn flush_claude_syncs_both_directions_and_updates_state() {
        let bot = MockBot::default();
        let config = test_config();
        let dir = tempdir().unwrap();
        let old_agent_bus_home = std::env::var("AGENT_BUS_HOME").ok();
        std::env::set_var("AGENT_BUS_HOME", dir.path().join("agent-bus-home"));
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let desktop = dir.path().join("desktop.jsonl");
        let mobile = dir.path().join("mobile.jsonl");
        {
            let mut f = std::fs::File::create(&desktop).unwrap();
            writeln!(f, r#"{{"sessionId":"desktop-id","text":"from desktop"}}"#).unwrap();
        }
        {
            let mut f = std::fs::File::create(&mobile).unwrap();
            writeln!(f, r#"{{"sessionId":"mobile-id","text":"from mobile"}}"#).unwrap();
        }
        state
            .set_bridged_session(
                "100",
                AgentKind::Claude.to_string(),
                BridgedSessionState {
                    agent: AgentKind::Claude.to_string(),
                    repo_id: "sample_repo".to_string(),
                    desktop_session_id: "desktop-id".to_string(),
                    desktop_path: desktop.display().to_string(),
                    mobile_session_id: "mobile-id".to_string(),
                    mobile_path: mobile.display().to_string(),
                    selected_at: "2026-04-19T00:00:00Z".to_string(),
                    sync: SessionSyncCursor {
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

        handle_text_command(
            &bot,
            &config,
            state.clone(),
            &None,
            100,
            None,
            "@flush_claude",
            None,
        )
        .await
        .unwrap();
        restore_env("AGENT_BUS_HOME", old_agent_bus_home);

        let sent = bot.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(
            sent[0].text.contains("@flush_claude synced"),
            "{}",
            sent[0].text
        );
        assert!(
            sent[0].text.contains("desktop -> mobile:"),
            "{}",
            sent[0].text
        );
        assert!(
            sent[0].text.contains("mobile -> desktop:"),
            "{}",
            sent[0].text
        );
        assert!(sent[0].text.contains("total:"), "{}", sent[0].text);

        let desktop_content = std::fs::read_to_string(&desktop).unwrap();
        let mobile_content = std::fs::read_to_string(&mobile).unwrap();
        assert!(desktop_content.contains("from mobile"));
        assert!(mobile_content.contains("from desktop"));

        let snapshot = state.snapshot().await;
        let bridge = &snapshot.bridged_sessions["100"]["claude"];
        assert!(bridge.sync.desktop_offset > 0);
        assert!(bridge.sync.mobile_offset > 0);
        assert!(bridge.sync.last_synced_at.is_some());
        assert_eq!(bridge.sync.last_error, None);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn list_codex_lists_matching_sessions() {
        let _guard = CODEX_OVERRIDE_TEST_LOCK.lock().unwrap();
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        let codex_home = dir.path().join("codex-home");
        std::fs::create_dir_all(&repo).unwrap();
        write_codex_session(&codex_home, "codex-session-1", &repo);
        *CODEX_HOME_OVERRIDE.lock().unwrap() = Some(codex_home);
        let config = config_with_repo_path(repo.display().to_string());
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();

        handle_text_command(&bot, &config, state, &None, 100, None, "/list_codex", None)
            .await
            .unwrap();
        *CODEX_HOME_OVERRIDE.lock().unwrap() = None;

        let sent = bot.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].text.contains("Active Codex sessions"));
        let keyboard = sent[0].keyboard.as_ref().expect("codex session keyboard");
        assert_eq!(keyboard.rows[0][0].1, "sel_codex:codex-session-1");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn selecting_codex_writes_generic_bridge_state() {
        let _guard = CODEX_OVERRIDE_TEST_LOCK.lock().unwrap();
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        let codex_home = dir.path().join("codex-home");
        std::fs::create_dir_all(&repo).unwrap();
        let session_path = write_codex_session(&codex_home, "codex-session-2", &repo);
        *CODEX_HOME_OVERRIDE.lock().unwrap() = Some(codex_home);
        let config = config_with_repo_path(repo.display().to_string());
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();

        handle_callback_sel_codex(
            &bot,
            &config,
            state.clone(),
            100,
            MessageRef {
                chat_id: 100,
                message_id: 1,
            },
            "cb1".to_string(),
            "sel_codex:codex-session-2".to_string(),
        )
        .await
        .unwrap();
        *CODEX_HOME_OVERRIDE.lock().unwrap() = None;

        let snapshot = state.snapshot().await;
        let bridge = &snapshot.bridged_sessions["100"]["codex"];
        assert_eq!(bridge.agent, "codex");
        assert_eq!(bridge.desktop_session_id, "codex-session-2");
        assert_eq!(bridge.desktop_path, session_path.display().to_string());
        assert_eq!(bridge.mobile_session_id, "codex-session-2");
        assert_eq!(bridge.mobile_path, session_path.display().to_string());
        assert_eq!(bot.answered_callbacks(), vec!["cb1".to_string()]);
        assert!(bot.edited_messages()[0].text.contains("selected"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn selecting_codex_targets_desktop_transcript_without_mobile_copy() {
        let _guard = CODEX_OVERRIDE_TEST_LOCK.lock().unwrap();
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        let codex_home = dir.path().join("codex-home");
        std::fs::create_dir_all(&repo).unwrap();
        write_codex_session(&codex_home, "codex-session-old", &repo);
        let new_session_path = write_codex_session(&codex_home, "codex-session-new", &repo);
        *CODEX_HOME_OVERRIDE.lock().unwrap() = Some(codex_home.clone());
        let config = config_with_repo_path(repo.display().to_string());
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();

        std::fs::write(
            &new_session_path,
            format!(
                "{}\n{}",
                std::fs::read_to_string(&new_session_path).unwrap(),
                r#"{"sessionId":"codex-session-new","text":"new desktop data"}"#
            ),
        )
        .unwrap();

        handle_callback_sel_codex(
            &bot,
            &config,
            state.clone(),
            100,
            MessageRef {
                chat_id: 100,
                message_id: 1,
            },
            "cb1".to_string(),
            "sel_codex:codex-session-new".to_string(),
        )
        .await
        .unwrap();
        *CODEX_HOME_OVERRIDE.lock().unwrap() = None;

        let snapshot = state.snapshot().await;
        let bridge = &snapshot.bridged_sessions["100"]["codex"];
        assert_eq!(bridge.desktop_session_id, "codex-session-new");
        assert_eq!(bridge.desktop_path, new_session_path.display().to_string());
        assert_eq!(bridge.mobile_session_id, "codex-session-new");
        assert_eq!(bridge.mobile_path, new_session_path.display().to_string());
        assert!(std::fs::read_to_string(&new_session_path)
            .unwrap()
            .contains("new desktop data"));
    }

    #[tokio::test]
    async fn codex_chat_reports_not_implemented() {
        let bot = MockBot::default();
        let config = test_config();
        let dir = tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();

        handle_text_command(&bot, &config, state, &None, 100, None, "@codex hello", None)
            .await
            .unwrap();

        let sent = bot.sent_messages();
        assert!(
            sent[0].text.contains("No codex session selected"),
            "expected setup guidance, got: {:?}",
            sent[0].text
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn list_gemini_lists_sessions() {
        let _guard = GEMINI_OVERRIDE_TEST_LOCK.lock().unwrap();
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let fake_gemini = dir.path().join("fake-gemini.sh");
        write_fake_gemini_script(&fake_gemini);

        let old_bin = std::env::var("AGENT_BUS_GEMINI_BIN").ok();
        std::env::set_var("AGENT_BUS_GEMINI_BIN", &fake_gemini);

        let config = config_with_repo_path(repo.display().to_string());
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();

        let result =
            handle_text_command(&bot, &config, state, &None, 100, None, "/list_gemini", None).await;
        restore_env("AGENT_BUS_GEMINI_BIN", old_bin);
        result.unwrap();

        let sent = bot.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].text.contains("Active Gemini sessions"));
        let keyboard = sent[0].keyboard.as_ref().expect("gemini session keyboard");
        assert_eq!(keyboard.rows[0][0].1, "sel_gemini:gem-11111111");
    }

    #[tokio::test]
    async fn gemini_chat_requires_default_repo() {
        let bot = MockBot::default();
        let config = test_config();
        let dir = tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();

        handle_text_command(
            &bot,
            &config,
            state,
            &None,
            100,
            None,
            "@gemini hello",
            None,
        )
        .await
        .unwrap();

        let sent = bot.sent_messages();
        assert!(
            sent[0].text.contains("No default repo"),
            "expected default repo guidance, got: {:?}",
            sent[0].text
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn selecting_gemini_writes_bridge_state() {
        let _guard = GEMINI_OVERRIDE_TEST_LOCK.lock().unwrap();
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let fake_gemini = dir.path().join("fake-gemini.sh");
        write_fake_gemini_script(&fake_gemini);

        let old_bin = std::env::var("AGENT_BUS_GEMINI_BIN").ok();
        std::env::set_var("AGENT_BUS_GEMINI_BIN", &fake_gemini);

        let config = config_with_repo_path(repo.display().to_string());
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();

        let result = handle_callback_sel_gemini(
            &bot,
            &config,
            state.clone(),
            100,
            MessageRef {
                chat_id: 100,
                message_id: 1,
            },
            "cb1".to_string(),
            "sel_gemini:gem-11111111".to_string(),
        )
        .await;
        restore_env("AGENT_BUS_GEMINI_BIN", old_bin);
        result.unwrap();

        let snapshot = state.snapshot().await;
        let bridge = &snapshot.bridged_sessions["100"]["gemini"];
        assert_eq!(bridge.agent, "gemini");
        assert_eq!(bridge.desktop_session_id, "gem-11111111");
        assert_eq!(bridge.mobile_session_id, "gem-11111111");
        assert_eq!(bridge.desktop_path, "");
        assert_eq!(bridge.mobile_path, "");
        assert_eq!(bot.answered_callbacks(), vec!["cb1".to_string()]);
        assert!(bot.edited_messages()[0].text.contains("@gemini <msg>"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn gemini_chat_runs_headless_cli_without_selected_session() {
        let _guard = GEMINI_OVERRIDE_TEST_LOCK.lock().unwrap();
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let fake_gemini = dir.path().join("fake-gemini.sh");
        write_fake_gemini_script(&fake_gemini);

        let old_bin = std::env::var("AGENT_BUS_GEMINI_BIN").ok();
        let old_approval = std::env::var("AGENT_BUS_GEMINI_APPROVAL_MODE").ok();
        std::env::set_var("AGENT_BUS_GEMINI_BIN", &fake_gemini);
        std::env::set_var("AGENT_BUS_GEMINI_APPROVAL_MODE", "plan");

        let config = config_with_repo_path(repo.display().to_string());
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();

        let result = handle_text_command(
            &bot,
            &config,
            state,
            &None,
            100,
            None,
            "@gemini hello",
            None,
        )
        .await;

        restore_env("AGENT_BUS_GEMINI_BIN", old_bin);
        restore_env("AGENT_BUS_GEMINI_APPROVAL_MODE", old_approval);
        result.unwrap();

        let sent = bot.sent_messages();
        assert!(sent[0].text.contains("gemini thinking"));
        assert!(sent[1].text.contains("gemini headless:"));
        assert!(sent[1].text.contains("--approval-mode plan"));
        assert!(sent[1].text.contains("hello"));
        assert!(sent[1].text.contains(&format!("cwd={}", repo.display())));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn gemini_chat_uses_selected_session() {
        let _guard = GEMINI_OVERRIDE_TEST_LOCK.lock().unwrap();
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let fake_gemini = dir.path().join("fake-gemini.sh");
        write_fake_gemini_script(&fake_gemini);

        let old_bin = std::env::var("AGENT_BUS_GEMINI_BIN").ok();
        let old_approval = std::env::var("AGENT_BUS_GEMINI_APPROVAL_MODE").ok();
        std::env::set_var("AGENT_BUS_GEMINI_BIN", &fake_gemini);
        std::env::set_var("AGENT_BUS_GEMINI_APPROVAL_MODE", "plan");

        let config = config_with_repo_path(repo.display().to_string());
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();
        state
            .set_bridged_session(
                "100",
                AgentKind::Gemini.to_string(),
                BridgedSessionState {
                    agent: AgentKind::Gemini.to_string(),
                    repo_id: "sample_repo".to_string(),
                    desktop_session_id: "gem-22222222".to_string(),
                    desktop_path: String::new(),
                    mobile_session_id: "gem-22222222".to_string(),
                    mobile_path: String::new(),
                    selected_at: "2026-04-19T00:00:00Z".to_string(),
                    sync: SessionSyncCursor {
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

        let result = handle_text_command(
            &bot,
            &config,
            state,
            &None,
            100,
            None,
            "@gemini hello",
            None,
        )
        .await;

        restore_env("AGENT_BUS_GEMINI_BIN", old_bin);
        restore_env("AGENT_BUS_GEMINI_APPROVAL_MODE", old_approval);
        result.unwrap();

        let sent = bot.sent_messages();
        assert!(sent[0].text.contains("gemini thinking"));
        assert!(sent[1].text.contains("gemini resume: gem-22222222"));
        assert!(sent[1].text.contains("--approval-mode plan"));
        assert!(sent[1].text.contains("hello"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn list_antigravity_lists_sessions() {
        let _guard = GEMINI_OVERRIDE_TEST_LOCK.lock().unwrap();
        let _antigravity_guard = session_bridge::ANTIGRAVITY_OVERRIDE_TEST_LOCK
            .lock()
            .unwrap();
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let db_path = dir.path().join("state.vscdb");
        write_antigravity_db(
            &db_path,
            &repo,
            "11111111-1111-1111-1111-111111111111",
            "Desktop Session",
        );
        session_bridge::set_antigravity_state_db_override(Some(db_path));
        session_bridge::set_antigravity_brain_root_override(Some(dir.path().join("brain-empty")));

        let config = config_with_repo_path(repo.display().to_string());
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();

        let result = handle_text_command(
            &bot,
            &config,
            state,
            &None,
            100,
            None,
            "/list_antigravity",
            None,
        )
        .await;
        session_bridge::set_antigravity_state_db_override(None);
        session_bridge::set_antigravity_brain_root_override(None);
        result.unwrap();

        let sent = bot.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].text.contains("Active Antigravity sessions"));
        let keyboard = sent[0]
            .keyboard
            .as_ref()
            .expect("antigravity session keyboard");
        assert_eq!(
            keyboard.rows[0][0].1,
            "sel_antigravity:11111111-1111-1111-1111-111111111111"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn selecting_antigravity_writes_bridge_state() {
        let _guard = GEMINI_OVERRIDE_TEST_LOCK.lock().unwrap();
        let _antigravity_guard = session_bridge::ANTIGRAVITY_OVERRIDE_TEST_LOCK
            .lock()
            .unwrap();
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let db_path = dir.path().join("state.vscdb");
        write_antigravity_db(
            &db_path,
            &repo,
            "22222222-2222-2222-2222-222222222222",
            "Bridge Session",
        );
        session_bridge::set_antigravity_state_db_override(Some(db_path));
        session_bridge::set_antigravity_brain_root_override(Some(dir.path().join("brain-empty")));

        let config = config_with_repo_path(repo.display().to_string());
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();

        let result = handle_callback_sel_antigravity(
            &bot,
            &config,
            state.clone(),
            100,
            MessageRef {
                chat_id: 100,
                message_id: 1,
            },
            "cb1".to_string(),
            "sel_antigravity:22222222-2222-2222-2222-222222222222".to_string(),
        )
        .await;
        session_bridge::set_antigravity_state_db_override(None);
        session_bridge::set_antigravity_brain_root_override(None);
        result.unwrap();

        let snapshot = state.snapshot().await;
        let bridge = &snapshot.bridged_sessions["100"]["antigravity"];
        assert_eq!(bridge.agent, "antigravity");
        assert_eq!(
            bridge.desktop_session_id,
            "22222222-2222-2222-2222-222222222222"
        );
        assert_eq!(bot.answered_callbacks(), vec!["cb1".to_string()]);
        assert!(bot.edited_messages()[0].text.contains("@antigravity <msg>"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn antigravity_chat_uses_selected_brain_session_not_gemini_resume() {
        let _guard = GEMINI_OVERRIDE_TEST_LOCK.lock().unwrap();
        let _antigravity_guard = session_bridge::ANTIGRAVITY_OVERRIDE_TEST_LOCK
            .lock()
            .unwrap();
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let config = config_with_repo_path(repo.display().to_string());
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();
        state
            .set_bridged_session(
                "100",
                AgentKind::Antigravity.to_string(),
                BridgedSessionState {
                    agent: AgentKind::Antigravity.to_string(),
                    repo_id: "sample_repo".to_string(),
                    desktop_session_id: "33333333-3333-3333-3333-333333333333".to_string(),
                    desktop_path: String::new(),
                    mobile_session_id: "33333333-3333-3333-3333-333333333333".to_string(),
                    mobile_path: String::new(),
                    selected_at: "2026-04-19T00:00:00Z".to_string(),
                    sync: SessionSyncCursor {
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

        let result = handle_text_command(
            &bot,
            &config,
            state,
            &None,
            100,
            None,
            "@antigravity hello",
            None,
        )
        .await;

        result.unwrap();

        let sent = bot.sent_messages();
        assert!(sent[0].text.contains("brain resume"));
        assert!(sent[1]
            .text
            .contains("Could not resume selected Antigravity brain session"));
        assert!(sent[1]
            .text
            .contains("33333333-3333-3333-3333-333333333333"));
        assert!(!sent.iter().any(|m| m.text.contains("gemini resume")));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn antigravity_stale_resume_keeps_selected_session_without_headless_fallback() {
        let _guard = GEMINI_OVERRIDE_TEST_LOCK.lock().unwrap();
        let _antigravity_guard = session_bridge::ANTIGRAVITY_OVERRIDE_TEST_LOCK
            .lock()
            .unwrap();
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let fake_gemini = dir.path().join("fake-gemini-stale.sh");
        write_stale_gemini_script(&fake_gemini);

        let old_bin = std::env::var("AGENT_BUS_GEMINI_BIN").ok();
        let old_approval = std::env::var("AGENT_BUS_GEMINI_APPROVAL_MODE").ok();
        std::env::set_var("AGENT_BUS_GEMINI_BIN", &fake_gemini);
        std::env::set_var("AGENT_BUS_GEMINI_APPROVAL_MODE", "plan");

        let config = config_with_repo_path(repo.display().to_string());
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();
        let selected_id = "99999999-9999-4999-9999-999999999999";
        state
            .set_bridged_session(
                "100",
                AgentKind::Antigravity.to_string(),
                BridgedSessionState {
                    agent: AgentKind::Antigravity.to_string(),
                    repo_id: "sample_repo".to_string(),
                    desktop_session_id: selected_id.to_string(),
                    desktop_path: String::new(),
                    mobile_session_id: selected_id.to_string(),
                    mobile_path: String::new(),
                    selected_at: "2026-04-19T00:00:00Z".to_string(),
                    sync: SessionSyncCursor {
                        desktop_offset: 0,
                        mobile_offset: 0,
                        last_synced_at: None,
                        last_error: None,
                    },
                    display_name: Some("Test Approval Gate".to_string()),
                },
            )
            .await
            .unwrap();

        let result = handle_text_command(
            &bot,
            &config,
            state.clone(),
            &None,
            100,
            None,
            "@antigravity hello",
            None,
        )
        .await;

        restore_env("AGENT_BUS_GEMINI_BIN", old_bin);
        restore_env("AGENT_BUS_GEMINI_APPROVAL_MODE", old_approval);
        result.unwrap();

        let sent = bot.sent_messages();
        assert!(sent[0].text.contains("brain resume"));
        assert!(sent[1]
            .text
            .contains("Could not resume selected Antigravity brain session"));
        assert!(sent[1].text.contains(selected_id));
        assert!(!sent
            .iter()
            .any(|m| m.text.contains("unexpected headless fallback")));

        let snapshot = state.snapshot().await;
        let bridge = &snapshot.bridged_sessions["100"]["antigravity"];
        assert_eq!(bridge.desktop_session_id, selected_id);
        assert_eq!(bridge.mobile_session_id, selected_id);
        assert_eq!(bridge.display_name.as_deref(), Some("Test Approval Gate"));
    }

    #[test]
    fn antigravity_language_server_process_and_port_parsers() {
        let process = parse_antigravity_process_line(
            "480376 /usr/share/antigravity/resources/app/extensions/antigravity/bin/language_server_linux_x64 --enable_lsp --csrf_token token-123 --extension_server_port 43049 --workspace_id file_home_john_chuong_Projects_tele_agent_bus",
        )
        .unwrap();
        assert_eq!(process.pid, 480376);
        assert_eq!(process.csrf_token, "token-123");
        assert_eq!(
            process.workspace_id.as_deref(),
            Some("file_home_john_chuong_Projects_tele_agent_bus")
        );
        assert!(process.enable_lsp);

        assert_eq!(
            antigravity_workspace_id("/home/john-chuong/Projects/tele-agent-bus"),
            "file_home_john_chuong_Projects_tele_agent_bus"
        );
        assert_eq!(
            parse_listening_port_for_pid(
                r#"LISTEN 0 4096 127.0.0.1:33171 0.0.0.0:* users:(("language_server",pid=480376,fd=8))"#,
                480376,
            ),
            Some(33171)
        );
    }

    #[test]
    fn antigravity_overview_reply_parser_uses_latest_model_content() {
        let text = r#"{"source":"USER_EXPLICIT","status":"DONE","content":"hello"}
{"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"first"}
{"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"second"}"#;
        assert_eq!(
            aggregate_antigravity_state(text).as_deref(),
            Some("first\n\nsecond")
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn antigravity_session_name_query_answers_from_selected_bridge() {
        let _guard = GEMINI_OVERRIDE_TEST_LOCK.lock().unwrap();
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let config = config_with_repo_path(repo.display().to_string());
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();
        let selected_id = "7d4c0288-4ddf-4398-9766-17e1d3c750f4";
        state
            .set_bridged_session(
                "100",
                AgentKind::Antigravity.to_string(),
                BridgedSessionState {
                    agent: AgentKind::Antigravity.to_string(),
                    repo_id: "sample_repo".to_string(),
                    desktop_session_id: selected_id.to_string(),
                    desktop_path: String::new(),
                    mobile_session_id: selected_id.to_string(),
                    mobile_path: String::new(),
                    selected_at: "2026-04-19T00:00:00Z".to_string(),
                    sync: SessionSyncCursor {
                        desktop_offset: 0,
                        mobile_offset: 0,
                        last_synced_at: None,
                        last_error: None,
                    },
                    display_name: Some(
                        "Test Approval Gate Không cần reply tin nhắn này".to_string(),
                    ),
                },
            )
            .await
            .unwrap();

        handle_text_command(
            &bot,
            &config,
            state,
            &None,
            100,
            None,
            "@antigravity tên Session hiện tại là gì",
            None,
        )
        .await
        .unwrap();

        let sent = bot.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(sent[0]
            .text
            .contains("Test Approval Gate Không cần reply tin nhắn này"));
        assert!(sent[0].text.contains(selected_id));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn antigravity_model_query_answers_from_selected_model_state() {
        let _guard = GEMINI_OVERRIDE_TEST_LOCK.lock().unwrap();
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let config = config_with_repo_path(repo.display().to_string());
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();
        state
            .set_bridged_session(
                "100",
                AgentKind::Antigravity.to_string(),
                BridgedSessionState {
                    agent: AgentKind::Antigravity.to_string(),
                    repo_id: "sample_repo".to_string(),
                    desktop_session_id: "7d4c0288-4ddf-4398-9766-17e1d3c750f4".to_string(),
                    desktop_path: String::new(),
                    mobile_session_id: "7d4c0288-4ddf-4398-9766-17e1d3c750f4".to_string(),
                    mobile_path: String::new(),
                    selected_at: "2026-04-19T00:00:00Z".to_string(),
                    sync: SessionSyncCursor {
                        desktop_offset: 0,
                        mobile_offset: 0,
                        last_synced_at: None,
                        last_error: None,
                    },
                    display_name: Some("Test Approval Gate".to_string()),
                },
            )
            .await
            .unwrap();
        state
            .set_selected_model("100", "antigravity", Some("gemini-3-flash".to_string()))
            .await
            .unwrap();

        handle_text_command(
            &bot,
            &config,
            state,
            &None,
            100,
            None,
            "@antigravity model hiện tại của bạn là gì",
            None,
        )
        .await
        .unwrap();

        let sent = bot.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].text.contains("gemini-3-flash"));
        assert!(!sent[0].text.contains("thinking"));
    }

    #[tokio::test]
    async fn codex_chat_uses_selected_session_via_runner() {
        use crate::daemon::cli_spawner::CliSpawner;
        use crate::daemon::runner::{AgentRunner, EventLog};
        use agent_bus_core::auth_context::AuthContextsConfig;
        use agent_bus_core::state::{BridgedSessionState, SessionSyncCursor};
        use std::sync::Arc;

        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo_path = dir.path().join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let config = config_with_repo_path(repo_path.display().to_string());
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();
        let session_path = dir.path().join("rollout-codex-session-xyz.jsonl");
        std::fs::write(
            &session_path,
            r#"{"type":"session_meta","payload":{"id":"codex-session-xyz","cwd":"/unused"}}"#,
        )
        .unwrap();
        state
            .set_bridged_session(
                "100",
                "codex",
                BridgedSessionState {
                    agent: "codex".to_string(),
                    repo_id: "sample_repo".to_string(),
                    desktop_session_id: "codex-session-xyz".to_string(),
                    desktop_path: session_path.display().to_string(),
                    mobile_session_id: "codex-session-xyz".to_string(),
                    mobile_path: session_path.display().to_string(),
                    selected_at: "2026-04-19T00:00:00Z".to_string(),
                    sync: SessionSyncCursor {
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

        let profile_dir = dir.path().join(".agent-bus/auth/codex/john");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let yaml = format!(
            "version: 1\ndefaults:\n  auto_rotate: false\n  require_owner_approval: false\nagents:\n  codex:\n    contexts:\n      - id: john\n        profile_dir: {}\n",
            profile_dir.display()
        );
        let cfg = AuthContextsConfig::parse(&yaml, dir.path()).unwrap();
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-cli/codex_ok.sh");
        let spawner = CliSpawner::new().with_bin("codex", fixture);
        let events = EventLog::new(dir.path().join("events.jsonl"));
        let runner = Arc::new(AgentRunner::new(spawner, cfg, state.clone(), events));

        handle_text_command(
            &bot,
            &config,
            state,
            &None,
            100,
            None,
            "@codex continue this",
            Some(&runner),
        )
        .await
        .unwrap();

        let sent = bot.sent_messages();
        assert!(
            sent.iter().any(|m| m.text.contains("codex thinking")),
            "messages: {:?}",
            sent.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
        assert!(
            sent.iter().any(|m| m
                .text
                .contains("[args=exec resume --skip-git-repo-check codex-session-xyz -]")),
            "messages: {:?}",
            sent.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
        assert!(sent
            .iter()
            .any(|m| m.text.contains("[agent-bus desktop session bridge]")));
        assert!(sent.iter().any(|m| m
            .text
            .contains("User message from Telegram:\ncontinue this")));
        assert!(profile_dir
            .join("sessions/agent-bus/rollout-codex-session-xyz.jsonl")
            .exists());
    }

    #[tokio::test]
    async fn unauthorized_chat_is_ignored_for_mobile_commands() {
        let bot = MockBot::default();
        let config = test_config();
        let dir = tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();

        handle_text_command(&bot, &config, state, &None, 999, None, "/list_claude", None)
            .await
            .unwrap();

        assert!(bot.sent_messages().is_empty());
    }

    #[tokio::test]
    async fn claude_msg_without_selected_session_uses_guard_message() {
        let bot = MockBot::default();
        let config = test_config();
        let dir = tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();

        handle_text_command(&bot, &config, state, &None, 100, None, "@claude hi", None)
            .await
            .unwrap();

        let sent = bot.sent_messages();
        assert!(sent[0].text.contains("No Claude session selected"));
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
            project_hash_for_repo("/home/user/Projects/SampleRepo"),
            "-home-user-Projects-SampleRepo"
        );
    }

    // Phase 4a.8: when agent_runner is Some, handle_claude_mobile_msg
    // must dispatch through it (not the legacy spawn_claude_resume path).
    // We verify this by pointing the runner at a fake claude that always
    // returns quota_exhausted, and asserting the user sees the runner's
    // "all contexts unavailable" message rather than a raw claude error.
    #[tokio::test]
    async fn claude_mobile_msg_uses_runner_when_provided() {
        use crate::daemon::cli_spawner::CliSpawner;
        use crate::daemon::runner::{AgentRunner, EventLog};
        use agent_bus_core::auth_context::AuthContextsConfig;
        use agent_bus_core::state::MobileSessionState;
        use std::sync::Arc;

        let bot = MockBot::default();
        let config = test_config();
        let dir = tempdir().unwrap();
        // test_config uses `/tmp/sample_repo-test` as repo.path and Command::current_dir
        // fails with ENOENT if the directory is missing. Ensure it exists.
        std::fs::create_dir_all("/tmp/sample_repo-test").unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();

        // Seed a mobile session so the guard passes.
        state
            .set_mobile_session(
                "100".to_string(),
                MobileSessionState {
                    repo_id: "sample_repo".to_string(),
                    mobile_uuid: "aaaa-bbbb".to_string(),
                    mobile_fork_source: String::new(),
                    mobile_forked_at: String::new(),
                    project_hash: String::new(),
                },
            )
            .await
            .unwrap();

        // Build an auth-contexts config with one enabled context, whose
        // profile_dir exists on disk.
        let profile_dir = dir.path().join(".agent-bus/auth/claude/john");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let yaml = format!(
            "version: 1\ndefaults:\n  auto_rotate: false\n  require_owner_approval: false\nagents:\n  claude:\n    contexts:\n      - id: john\n        profile_dir: {}\n",
            profile_dir.display()
        );
        let cfg = AuthContextsConfig::parse(&yaml, dir.path()).unwrap();

        // Point the CLI spawner at the quota-exhausted fake fixture.
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/fake-cli/claude_quota.sh");
        let spawner = CliSpawner::new().with_bin("claude", fixture);
        let events = EventLog::new(dir.path().join("events.jsonl"));
        let runner = Arc::new(AgentRunner::new(spawner, cfg, state.clone(), events));

        handle_claude_mobile_msg(
            &bot,
            &config,
            state,
            100,
            "do it".to_string(),
            Some(&runner),
        )
        .await
        .unwrap();

        let sent = bot.sent_messages();
        // thinking... message, then runner's "unavailable" error mapped by run_claude_via_runner.
        assert!(sent.iter().any(|m| m.text.contains("thinking")));
        assert!(
            sent.iter().any(|m| m.text.contains("out of quota")
                || m.text.contains("unavailable")
                || m.text.contains("/quota claude")),
            "expected runner-path error message, got: {:?}",
            sent.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn unaddressed_message_with_claude_lead_and_mobile_session_uses_resume() {
        use crate::daemon::cli_spawner::CliSpawner;
        use crate::daemon::runner::{AgentRunner, EventLog};
        use agent_bus_core::auth_context::AuthContextsConfig;
        use agent_bus_core::state::MobileSessionState;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;

        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo_path = dir.path().join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let args_file = dir.path().join("claude-args.txt");
        let fake_claude = dir.path().join("fake-claude.sh");
        std::fs::write(
            &fake_claude,
            format!(
                "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" > {}\necho resumed-ok\n",
                args_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o755)).unwrap();

        let config = TelegramConfig {
            allowed_chats: vec!["100".to_string()],
            repos: vec![RepoEntry {
                id: "sample_repo".to_string(),
                display: "SampleRepo".to_string(),
                path: repo_path.display().to_string(),
                agents: vec!["claude".to_string()],
                codex_mode: CodexMode::LiveBridge,
            }],
        };
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();
        state
            .set_mobile_session(
                "100",
                MobileSessionState {
                    repo_id: "sample_repo".to_string(),
                    mobile_uuid: "mobile-uuid-123".to_string(),
                    mobile_fork_source: String::new(),
                    mobile_forked_at: String::new(),
                    project_hash: String::new(),
                },
            )
            .await
            .unwrap();

        let profile_dir = dir.path().join(".agent-bus/auth/claude/john");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let yaml = format!(
            "version: 1\nlead:\n  default: claude\nagents:\n  claude:\n    contexts:\n      - id: john\n        profile_dir: {}\n",
            profile_dir.display()
        );
        let cfg = AuthContextsConfig::parse(&yaml, dir.path()).unwrap();
        let runner = Arc::new(AgentRunner::new(
            CliSpawner::new().with_bin("claude", fake_claude),
            cfg.clone(),
            state.clone(),
            EventLog::new(dir.path().join("events.jsonl")),
        ));

        handle_text_command(
            &bot,
            &config,
            state,
            &Some(cfg),
            100,
            None,
            "hello lead",
            Some(&runner),
        )
        .await
        .unwrap();

        let args = std::fs::read_to_string(args_file).unwrap();
        assert!(args.contains("--resume mobile-uuid-123"), "args: {args}");
        assert!(bot
            .sent_messages()
            .iter()
            .any(|m| m.text.contains("resumed-ok")));
    }

    #[tokio::test]
    async fn unaddressed_message_with_codex_lead_and_selected_session_uses_bridge() {
        use crate::daemon::cli_spawner::CliSpawner;
        use crate::daemon::runner::{AgentRunner, EventLog};
        use agent_bus_core::auth_context::AuthContextsConfig;
        use agent_bus_core::state::{BridgedSessionState, SessionSyncCursor};
        use std::sync::Arc;

        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo_path = dir.path().join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let config = TelegramConfig {
            allowed_chats: vec!["100".to_string()],
            repos: vec![RepoEntry {
                id: "sample_repo".to_string(),
                display: "SampleRepo".to_string(),
                path: repo_path.display().to_string(),
                agents: vec!["codex".to_string()],
                codex_mode: CodexMode::LiveBridge,
            }],
        };
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();
        state.set_lead_for_chat("100", "codex").await.unwrap();
        let session_path = dir.path().join("rollout-codex-session-xyz.jsonl");
        std::fs::write(
            &session_path,
            r#"{"type":"session_meta","payload":{"id":"codex-session-xyz","cwd":"/repo"}}"#,
        )
        .unwrap();
        let mobile_path = dir.path().join("mobile.jsonl");
        std::fs::File::create(&mobile_path).unwrap();
        state
            .set_bridged_session(
                "100",
                "codex",
                BridgedSessionState {
                    agent: "codex".to_string(),
                    repo_id: "sample_repo".to_string(),
                    desktop_session_id: "codex-session-xyz".to_string(),
                    desktop_path: session_path.display().to_string(),
                    mobile_session_id: "agent-bus-mobile-codex".to_string(),
                    mobile_path: mobile_path.display().to_string(),
                    selected_at: "2026-04-19T00:00:00Z".to_string(),
                    sync: SessionSyncCursor {
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

        let profile_dir = dir.path().join(".agent-bus/auth/codex/john");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let yaml = format!(
            "version: 1\nlead:\n  default: codex\nagents:\n  codex:\n    contexts:\n      - id: john\n        profile_dir: {}\n",
            profile_dir.display()
        );
        let cfg = AuthContextsConfig::parse(&yaml, dir.path()).unwrap();
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-cli/codex_ok.sh");
        let runner = Arc::new(AgentRunner::new(
            CliSpawner::new().with_bin("codex", fixture),
            cfg.clone(),
            state.clone(),
            EventLog::new(dir.path().join("events.jsonl")),
        ));

        handle_text_command(
            &bot,
            &config,
            state,
            &Some(cfg),
            100,
            None,
            "OK, vậy là đúng rồi. Good job",
            Some(&runner),
        )
        .await
        .unwrap();

        let sent = bot.sent_messages();
        assert!(
            sent.iter().any(|m| m.text.contains("codex thinking")),
            "messages: {:?}",
            sent.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
        assert!(sent
            .iter()
            .any(|m| m.text.contains("[agent-bus desktop session bridge]")));
        assert!(sent
            .iter()
            .any(|m| m.text.contains("OK, vậy là đúng rồi. Good job")));
        assert!(profile_dir
            .join("sessions/agent-bus/rollout-codex-session-xyz.jsonl")
            .exists());
    }

    #[tokio::test]
    async fn selecting_claude_writes_new_generic_state_ac_sb4() {
        let bot = MockBot::default();
        let dir = tempdir().unwrap();
        let repo_path = dir.path().join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let home = dir.path().join("home");
        let desktop_uuid = "11111111-1111-1111-1111-111111111111";
        let project_dir = home
            .join(".claude")
            .join("projects")
            .join(project_hash_for_repo(&repo_path.display().to_string()));
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join(format!("{desktop_uuid}.jsonl")),
            format!(
                r#"{{"type":"user","sessionId":"{desktop_uuid}","cwd":"{}","timestamp":"2026-04-19T00:00:00Z","message":{{"role":"user","content":"hello"}}}}"#,
                repo_path.display()
            ),
        )
        .unwrap();
        let old_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &home);
        let config = TelegramConfig {
            allowed_chats: vec!["100".to_string()],
            repos: vec![RepoEntry {
                id: "sample_repo".to_string(),
                display: "SampleRepo".to_string(),
                path: repo_path.display().to_string(),
                agents: vec!["claude".to_string()],
                codex_mode: CodexMode::LiveBridge,
            }],
        };
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state.set_default_repo("100", "sample_repo").await.unwrap();

        handle_callback_sel_claude(
            &bot,
            &config,
            state.clone(),
            100,
            MessageRef {
                chat_id: 100,
                message_id: 42,
            },
            "cb-1".to_string(),
            format!("sel_claude:{desktop_uuid}"),
        )
        .await
        .unwrap();
        restore_env("HOME", old_home);

        let snap = state.snapshot().await;
        assert!(
            snap.bridged_sessions.contains_key("100"),
            "bridged_sessions should contain chat 100"
        );
        assert!(
            snap.bridged_sessions["100"].contains_key("claude"),
            "bridged_sessions['100'] should contain 'claude'"
        );
        assert!(
            !snap.mobile_sessions.contains_key("100"),
            "new Claude selections should not create a legacy mobile session"
        );
        let bridge = &snap.bridged_sessions["100"]["claude"];
        assert_eq!(bridge.desktop_session_id, desktop_uuid);
        assert_eq!(bridge.mobile_session_id, desktop_uuid);
        assert_eq!(bridge.desktop_path, bridge.mobile_path);

        let edited = bot.edited_messages();
        assert_eq!(edited.len(), 1);
        assert!(edited[0].text.contains("Claude session \"hello\" selected"));
        assert!(!edited[0].text.contains("\"11111111\""));
    }
}
