pub mod auth_cmds;
pub mod claude_headless;
pub mod cli_spawner;
pub mod codex_ipc;
pub mod context_lock;
pub mod inbox;
pub mod mobile_session;
pub mod mobile_transcript;
pub mod perm;
pub mod routing;
pub mod runner;
pub mod session_bridge;
pub mod telegram;
pub mod uds;

#[cfg(test)]
mod runner_cli_tests;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_bus_core::auth_context::AuthContextsConfig;
use agent_bus_core::state::spawn_state_actor;
use anyhow::Context;
use serde::Deserialize;
use teloxide::dispatching::UpdateFilterExt;
use teloxide::prelude::Dispatcher;
use teloxide::types::Update;

use self::cli_spawner::CliSpawner;
use self::perm::{FsBlacklistLoader, MergedBlacklistLoader, PendingPermRegistry, PermService};
use self::runner::{AgentRunner, EventLog, SharedAgentRunner};
use self::telegram::{RepoEntry, TelegramConfig, TeloxideBotClient};

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub timeout_seconds: u64,
    pub home: PathBuf,
    pub bot_token: String,
    pub telegram: TelegramConfig,
    /// Phase 4a: loaded from `$AGENT_BUS_HOME/auth-contexts.yaml`.
    /// `None` means legacy mode — existing `@claude` mobile flow unchanged (AC-Q9 / AC-L8).
    pub auth_contexts: Option<AuthContextsConfig>,
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    #[serde(default = "default_permissions")]
    pub permissions: PermissionsFile,
    telegram: TelegramFile,
}

#[derive(Debug, Deserialize)]
pub struct PermissionsFile {
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
}

fn default_timeout_seconds() -> u64 {
    30
}
fn default_permissions() -> PermissionsFile {
    PermissionsFile {
        timeout_seconds: default_timeout_seconds(),
    }
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

    // Seed active_auth_context from config at startup (spec §4.1):
    // `agents.<agent>.active` becomes the initial active context.
    if let Some(ref auth_cfg) = config.auth_contexts {
        for (agent, id) in &auth_cfg.active {
            state.set_active_auth_context(agent, id).await.ok();
        }

        // Phase 4a.12: Token expiry detection on startup
        for (agent_name, contexts) in &auth_cfg.agents {
            for ctx in contexts {
                let status = agent_bus_core::token_expiry::read_for_agent(agent_name, &ctx.profile_dir);
                use agent_bus_core::token_expiry::ExpiryStatus;
                use agent_bus_core::state::{AuthContextStatus, AuthContextStatusKind};
                let kind = match status {
                    ExpiryStatus::Expired { .. } => Some(AuthContextStatusKind::AuthExpired),
                    ExpiryStatus::ExpiringSoon { .. } => Some(AuthContextStatusKind::AuthExpiringSoon),
                    _ => None,
                };
                if let Some(k) = kind {
                    let now = time::OffsetDateTime::now_utc();
                    let st = AuthContextStatus {
                        status: k,
                        cooldown_until: None,
                        last_event_id: None,
                        updated_at: now.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
                    };
                    state.set_auth_context_status(agent_name, &ctx.id, st).await.ok();
                }
            }
        }
    }

    // Phase 4a.8: build AgentRunner when auth-contexts.yaml is present.
    // Legacy mode (None) keeps `handle_claude_mobile_msg` on the existing
    // `spawn_claude_resume` fast path (AC-Q9).
    let agent_runner: SharedAgentRunner = config.auth_contexts.as_ref().map(|cfg| {
        let spawner = CliSpawner::new();
        let events = EventLog::new(config.home.join("events.jsonl"));
        Arc::new(AgentRunner::new(spawner, cfg.clone(), state.clone(), events))
    });

    let auth_contexts = Arc::new(config.auth_contexts);
    let telegram_config = Arc::new(config.telegram);
    let bot = teloxide::Bot::new(config.bot_token);
    let registry = PendingPermRegistry::default();
    let bot_client: Arc<dyn telegram::BotClient> = Arc::new(TeloxideBotClient::new(bot.clone()));

    let etc_dir = PathBuf::from("/etc/agent-bus");
    let global_loader = Arc::new(FsBlacklistLoader::new(
        etc_dir.join("blacklist.conf"),
        etc_dir.join("blacklist.conf.hmac"),
        etc_dir.join("blacklist.key"),
    ));

    let home_dir = config.home.clone();
    let repo_loader_fn = move |repo_id: &agent_bus_core::repo_id::RepoId| {
        let repo_dir = home_dir.join("repos").join(repo_id.as_str());
        Arc::new(FsBlacklistLoader::new(
            repo_dir.join("blacklist.conf"),
            repo_dir.join("blacklist.conf.hmac"),
            etc_dir.join("blacklist.key"),
        )) as Arc<dyn perm::BlacklistLoader>
    };

    let loader = Arc::new(MergedBlacklistLoader::new(global_loader, Box::new(repo_loader_fn)));

    let perm = PermService::new(
        state.clone(),
        Arc::clone(&telegram_config),
        bot_client,
        loader,
        registry.clone(),
        Duration::from_secs(config.timeout_seconds),
    );
    let uds_server = uds::UdsServer::new(config.home.join("daemon.sock"), state.clone(), perm);
    let uds_task = tokio::spawn(uds::run_uds_server(uds_server));
    let bridge_sync_task =
        session_bridge::spawn_session_bridge_sync(state.clone(), Duration::from_secs(30));

    let handler = teloxide::dptree::entry()
        .branch(Update::filter_message().endpoint(telegram::teloxide_message_handler))
        .branch(Update::filter_callback_query().endpoint(telegram::teloxide_callback_handler));

    Dispatcher::builder(bot, handler)
        .dependencies(teloxide::dptree::deps![
            telegram_config,
            state,
            registry,
            auth_contexts,
            agent_runner
        ])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    uds_task.abort();
    bridge_sync_task.abort();
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

    let bot_token = resolve_bot_token(&config_file.telegram.bot_token, |name| {
        std::env::var(name).ok()
    })?;
    let allowed_chats = config_file
        .telegram
        .allowed_chats
        .iter()
        .map(|value| resolve_env_value(value))
        .collect::<Result<Vec<_>, _>>()?;

    let auth_contexts = load_auth_contexts(&home)?;

    Ok(DaemonConfig {
        timeout_seconds: config_file.permissions.timeout_seconds,
        home: home.clone(),
        bot_token,
        telegram: TelegramConfig {
            allowed_chats,
            repos: repos_file.repos,
        },
        auth_contexts,
    })
}

/// Load `$AGENT_BUS_HOME/auth-contexts.yaml` if it exists. Missing file is
/// NOT an error — the daemon falls back to legacy mode (AC-Q9 / AC-L8).
/// Malformed file IS an error — fail-fast so the operator sees it.
fn load_auth_contexts(home: &std::path::Path) -> anyhow::Result<Option<AuthContextsConfig>> {
    let path = home.join("auth-contexts.yaml");
    if !path.exists() {
        tracing::info!(
            path = %path.display(),
            "auth-contexts.yaml not found; falling back to legacy mode"
        );
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let home_for_paths = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.to_path_buf());
    let cfg = AuthContextsConfig::parse(&text, &home_for_paths)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let claude_ctx = cfg.contexts_for("claude").len();
    let codex_ctx = cfg.contexts_for("codex").len();
    tracing::info!(
        path = %path.display(),
        claude_contexts = claude_ctx,
        codex_contexts = codex_ctx,
        "loaded auth-contexts.yaml"
    );
    Ok(Some(cfg))
}

fn agent_bus_home() -> anyhow::Result<PathBuf> {
    if let Ok(home) = std::env::var("AGENT_BUS_HOME") {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".agent-bus"))
}

fn resolve_bot_token(
    value: &str,
    getenv: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<String> {
    if let Some(name) = value.strip_prefix("env:") {
        if name == "TELE_BUS_BOT_TOKEN" || name == "TELE_BUS_TOKEN" {
            if let Some(val) = getenv("TELE_BUS_BOT_TOKEN") {
                return Ok(val);
            }
            if let Some(val) = getenv("TELE_BUS_TOKEN") {
                tracing::warn!("TELE_BUS_TOKEN is deprecated, use TELE_BUS_BOT_TOKEN");
                return Ok(val);
            }
            anyhow::bail!("missing environment variable TELE_BUS_BOT_TOKEN");
        }
        getenv(name).with_context(|| format!("missing environment variable {name}"))
    } else {
        Ok(value.to_string())
    }
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
    use std::sync::{Arc, Mutex};

    use super::telegram::{
        handle_callback_perm, handle_callback_switch, handle_current_command,
        handle_list_rp_command, handle_switch_rp_command, handle_text_command, InlineKeyboard,
        MessageRef, MockBot, RepoEntry, TelegramConfig,
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

    fn config_with_repo_path(path: &Path) -> TelegramConfig {
        TelegramConfig {
            allowed_chats: vec!["123".to_string()],
            repos: vec![RepoEntry {
                id: "rallyup".to_string(),
                display: "RallyUp".to_string(),
                path: path.display().to_string(),
                agents: vec!["codex".to_string()],
            }],
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

    #[tokio::test]
    async fn test_routing_unknown_agent_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let state = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let bot = MockBot::default();

        handle_text_command(&bot, &config_with_repo_path(dir.path()), state, &None, 123,
            Some("alice"),
            "@bogus:rallyup foo",
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            bot.sent_messages()[0].text,
            "Unknown agent or repo: @bogus:rallyup foo"
        );
        assert!(!dir.path().join(".agents/inbox/bogus.md").exists());
    }

    #[test]
    fn test_routing_logs_reject_reason_with_snippet() {
        #[derive(Clone)]
        struct SharedWriter(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for SharedWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let logs = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer({
                let logs = Arc::clone(&logs);
                move || SharedWriter(Arc::clone(&logs))
            })
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                let dir = tempfile::tempdir().unwrap();
                let state = spawn_state_actor(dir.path().join("state.json"))
                    .await
                    .unwrap();
                let bot = MockBot::default();
                handle_text_command(&bot, &config_with_repo_path(dir.path()), state, &None, 123,
                    Some("alice"),
                    "@codex:rallyup     ",
                    None,
                )
                .await
                .unwrap();
            });
        });

        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("routing_rejected"));
        assert!(logs.contains("reason=empty_body"));
        assert!(logs.contains("raw_snippet=\"@codex:rallyup     \""));
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

    #[test]
    fn test_bot_token_prefers_new_env_var() {
        let getenv = |name: &str| match name {
            "TELE_BUS_BOT_TOKEN" => Some("new".to_string()),
            "TELE_BUS_TOKEN" => Some("old".to_string()),
            _ => None,
        };
        let token = super::resolve_bot_token("env:TELE_BUS_TOKEN", getenv).unwrap();
        assert_eq!(token, "new");
    }

    #[test]
    fn test_bot_token_falls_back_to_legacy() {
        let getenv = |name: &str| match name {
            "TELE_BUS_TOKEN" => Some("legacy".to_string()),
            _ => None,
        };
        let token = super::resolve_bot_token("env:TELE_BUS_TOKEN", getenv).unwrap();
        assert_eq!(token, "legacy");
    }

    fn assert_daemon_config_home(config: &DaemonConfig, home: &Path) {
        assert_eq!(config.home, home);
    }

    #[test]
    fn load_auth_contexts_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let result = super::load_auth_contexts(dir.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_auth_contexts_present_parses() {
        let dir = tempfile::tempdir().unwrap();
        // Create profile dir so AuthContextsConfig path validation passes.
        let home = dir.path();
        std::fs::create_dir_all(home.join(".agent-bus/auth/claude/john")).unwrap();
        std::fs::write(
            home.join("auth-contexts.yaml"),
            format!(
                r#"
version: 1
agents:
  claude:
    contexts:
      - id: john
        profile_dir: {}/.agent-bus/auth/claude/john
"#,
                home.display()
            ),
        )
        .unwrap();
        std::env::set_var("HOME", home);
        let cfg = super::load_auth_contexts(home).unwrap().unwrap();
        assert_eq!(cfg.contexts_for("claude").len(), 1);
        assert_eq!(cfg.contexts_for("claude")[0].id, "john");
    }
}
