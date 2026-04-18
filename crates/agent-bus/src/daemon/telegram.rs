use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;

use crate::daemon::routing::{Routed, RoutingError, RoutingParser};
use agent_bus_core::state::{PendingPermStatus, StateHandle};
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
    chat_id: i64,
    username: Option<&str>,
    text: &str,
) -> Result<(), TelegramError> {
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
        _ => Ok(()),
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
) -> ResponseResult<()> {
    let client = TeloxideBotClient::new(bot);
    if let Some(text) = msg.text() {
        let username = msg.from.as_ref().and_then(|user| user.username.as_deref());
        handle_text_command(&client, &config, state, msg.chat.id.0, username, text)
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
