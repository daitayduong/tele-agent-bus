pub mod perm;
pub mod telegram;
pub mod uds;

use std::path::PathBuf;
use std::sync::Arc;

use agent_bus_core::state::spawn_state_actor;
use anyhow::Context;
use serde::Deserialize;
use teloxide::dispatching::UpdateFilterExt;
use teloxide::prelude::Dispatcher;
use teloxide::types::Update;

use self::perm::{FsBlacklistLoader, PendingPermRegistry, PermService};
use self::telegram::{RepoEntry, TelegramConfig, TeloxideBotClient};

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub home: PathBuf,
    pub bot_token: String,
    pub telegram: TelegramConfig,
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    telegram: TelegramFile,
}

#[derive(Debug, Deserialize)]
struct TelegramFile {
    bot_token: String,
    #[serde(default)]
    allowed_chats: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ReposFile {
    #[serde(default)]
    repos: Vec<RepoEntry>,
}

pub async fn run_daemon(config: DaemonConfig) -> anyhow::Result<()> {
    let state = spawn_state_actor(config.home.join("state.json"))
        .await
        .context("failed to start state actor")?;
    let telegram_config = Arc::new(config.telegram);
    let bot = teloxide::Bot::new(config.bot_token);
    let registry = PendingPermRegistry::default();
    let bot_client: Arc<dyn telegram::BotClient> = Arc::new(TeloxideBotClient::new(bot.clone()));
    let loader = Arc::new(FsBlacklistLoader::new(PathBuf::from(
        "/etc/agent-bus/blacklist.conf",
    )));
    let perm = PermService::new(
        state.clone(),
        Arc::clone(&telegram_config),
        bot_client,
        loader,
        registry.clone(),
    );
    let uds_server = uds::UdsServer::new(config.home.join("daemon.sock"), state.clone(), perm);
    let uds_task = tokio::spawn(uds::run_uds_server(uds_server));

    let handler = teloxide::dptree::entry()
        .branch(Update::filter_message().endpoint(telegram::teloxide_message_handler))
        .branch(Update::filter_callback_query().endpoint(telegram::teloxide_callback_handler));

    Dispatcher::builder(bot, handler)
        .dependencies(teloxide::dptree::deps![telegram_config, state, registry])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    uds_task.abort();
    Ok(())
}

pub fn load_daemon_config() -> anyhow::Result<DaemonConfig> {
    let home = agent_bus_home()?;
    let config_text = std::fs::read_to_string(home.join("config.yaml"))
        .with_context(|| format!("failed to read {}", home.join("config.yaml").display()))?;
    let repos_text = std::fs::read_to_string(home.join("repos.yaml"))
        .with_context(|| format!("failed to read {}", home.join("repos.yaml").display()))?;

    let config_file: ConfigFile =
        serde_yaml::from_str(&config_text).context("failed to parse config.yaml")?;
    let repos_file: ReposFile =
        serde_yaml::from_str(&repos_text).context("failed to parse repos.yaml")?;

    let bot_token = resolve_env_value(&config_file.telegram.bot_token)?;
    let allowed_chats = config_file
        .telegram
        .allowed_chats
        .iter()
        .map(|value| resolve_env_value(value))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(DaemonConfig {
        home,
        bot_token,
        telegram: TelegramConfig {
            allowed_chats,
            repos: repos_file.repos,
        },
    })
}

fn agent_bus_home() -> anyhow::Result<PathBuf> {
    if let Ok(home) = std::env::var("AGENT_BUS_HOME") {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".agent-bus"))
}

fn resolve_env_value(value: &str) -> anyhow::Result<String> {
    if let Some(name) = value.strip_prefix("env:") {
        std::env::var(name).with_context(|| format!("missing environment variable {name}"))
    } else {
        Ok(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::telegram::{
        handle_callback_perm, handle_callback_switch, handle_current_command,
        handle_list_rp_command, handle_switch_rp_command, InlineKeyboard, MessageRef, MockBot,
        RepoEntry, TelegramConfig,
    };
    use super::{load_daemon_config, DaemonConfig};
    use agent_bus_core::peer_uid::{verify_peer_uid, MockPeerUid};
    use agent_bus_core::state::{spawn_state_actor, PendingPerm, PendingPermStatus};

    fn config() -> TelegramConfig {
        TelegramConfig {
            allowed_chats: vec!["123".to_string()],
            repos: vec![
                RepoEntry {
                    id: "rallyup_a1b2c3d4".to_string(),
                    display: "RallyUp".to_string(),
                    path: "/tmp/RallyUp".to_string(),
                    agents: vec![
                        "claude".to_string(),
                        "gemini".to_string(),
                        "codex".to_string(),
                    ],
                },
                RepoEntry {
                    id: "docprivy_d4e5f6a7".to_string(),
                    display: "DocPrivy".to_string(),
                    path: "/tmp/DocPrivy".to_string(),
                    agents: vec!["claude".to_string()],
                },
            ],
        }
    }

    #[tokio::test]
    async fn list_rp_replies_with_switch_keyboard() {
        let dir = tempfile::tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state
            .set_default_repo("123", "rallyup_a1b2c3d4")
            .await
            .unwrap();
        let bot = MockBot::default();

        handle_list_rp_command(&bot, &config(), state, 123)
            .await
            .unwrap();

        let sent = bot.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].text.contains("Registered repos"));
        assert!(sent[0].text.contains("RallyUp"));
        assert_eq!(
            sent[0].keyboard,
            Some(InlineKeyboard {
                rows: vec![
                    vec![(
                        "RallyUp *".to_string(),
                        "switch:rallyup_a1b2c3d4".to_string()
                    )],
                    vec![(
                        "DocPrivy".to_string(),
                        "switch:docprivy_d4e5f6a7".to_string()
                    )],
                ],
            })
        );
    }

    #[tokio::test]
    async fn switch_rp_updates_state_json_and_confirms() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let state = spawn_state_actor(state_path.clone()).await.unwrap();
        let bot = MockBot::default();

        handle_switch_rp_command(&bot, &config(), state, 123, "docprivy_d4e5f6a7".to_string())
            .await
            .unwrap();

        let persisted = std::fs::read_to_string(state_path).unwrap();
        assert!(persisted.contains(r#""123":"docprivy_d4e5f6a7""#));
        let sent = bot.sent_messages();
        assert_eq!(sent[0].text, "Default repo set to DocPrivy");
    }

    #[tokio::test]
    async fn callback_switch_edits_message_and_updates_state() {
        let dir = tempfile::tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let bot = MockBot::default();

        handle_callback_switch(
            &bot,
            &config(),
            state.clone(),
            123,
            MessageRef {
                chat_id: 123,
                message_id: 42,
            },
            "cb-1".to_string(),
            "switch:docprivy_d4e5f6a7".to_string(),
        )
        .await
        .unwrap();

        let snapshot = state.snapshot().await;
        assert_eq!(
            snapshot.default_repo_by_chat.get("123").map(String::as_str),
            Some("docprivy_d4e5f6a7")
        );
        assert_eq!(bot.answered_callbacks(), vec!["cb-1".to_string()]);
        assert_eq!(bot.edited_messages()[0].text, "Default -> DocPrivy");
    }

    #[tokio::test]
    async fn callback_perm_denies_pending_and_edits_message() {
        let dir = tempfile::tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        state
            .insert_pending(PendingPerm {
                id: "perm-1".to_string(),
                repo_id: "rallyup_a1b2c3d4".to_string(),
                command_hash: "sha256:abc".to_string(),
                status: PendingPermStatus::Sent,
                created_at: "2026-04-16T00:00:00Z".to_string(),
                timeout_at: "2026-04-16T00:00:10Z".to_string(),
                message_id: Some(42),
            })
            .await
            .unwrap();
        let registry = super::perm::PendingPermRegistry::default();
        let rx = registry.insert("perm-1".to_string()).await;
        let bot = MockBot::default();

        handle_callback_perm(
            &bot,
            state.clone(),
            registry,
            MessageRef {
                chat_id: 123,
                message_id: 42,
            },
            "cb-perm".to_string(),
            "perm:deny:perm-1".to_string(),
            Some("alice".to_string()),
        )
        .await
        .unwrap();

        assert_eq!(rx.await.unwrap(), super::perm::PermVerdict::Deny);
        assert_eq!(
            state
                .snapshot()
                .await
                .pending_perms
                .get("perm-1")
                .map(|perm| &perm.status),
            Some(&PendingPermStatus::Denied)
        );
        assert_eq!(bot.edited_messages()[0].text, "Denied by @alice");
        assert_eq!(bot.answered_callbacks(), vec!["cb-perm".to_string()]);
    }

    #[tokio::test]
    async fn current_reports_none_for_allowed_chat_without_default() {
        let dir = tempfile::tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let bot = MockBot::default();

        handle_current_command(&bot, &config(), state, 123)
            .await
            .unwrap();

        assert_eq!(bot.sent_messages()[0].text, "Current default repo: none");
    }

    #[tokio::test]
    async fn disallowed_chat_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let bot = MockBot::default();

        handle_current_command(&bot, &config(), state, 999)
            .await
            .unwrap();

        assert!(bot.sent_messages().is_empty());
    }

    #[test]
    fn daemon_config_loads_split_yaml_from_agent_bus_home() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.yaml"),
            r#"
schema_version: 1
telegram:
  bot_token: env:TEST_TELE_BUS_TOKEN
  allowed_chats:
    - env:TEST_TELE_BUS_CHAT_ID
fail_mode: hybrid
log_level: info
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("repos.yaml"),
            r#"
schema_version: 1
repos:
  - id: rallyup_a1b2c3d4
    display: RallyUp
    path: /tmp/RallyUp
    agents: [claude, gemini, codex]
"#,
        )
        .unwrap();
        std::env::set_var("AGENT_BUS_HOME", dir.path());
        std::env::set_var("TEST_TELE_BUS_TOKEN", "token");
        std::env::set_var("TEST_TELE_BUS_CHAT_ID", "123");

        let loaded = load_daemon_config().unwrap();

        assert_daemon_config_home(&loaded, dir.path());
        assert_eq!(loaded.bot_token, "token");
        assert_eq!(loaded.telegram.allowed_chats, vec!["123".to_string()]);
        assert_eq!(loaded.telegram.repos[0].display, "RallyUp");
    }

    #[test]
    fn daemon_side_peer_uid_trait_is_usable_for_future_uds_server() {
        verify_peer_uid(&MockPeerUid::new(1000), &(), 1000).unwrap();
    }

    fn assert_daemon_config_home(config: &DaemonConfig, home: &Path) {
        assert_eq!(config.home, home);
    }
}
