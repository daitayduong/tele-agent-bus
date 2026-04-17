use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_bus_core::blacklist::Blacklist;
use agent_bus_core::state::{PendingPerm, PendingPermStatus, StateHandle};
use agent_bus_proto::{Decision, PermCheckRequest, PermCheckResponse, PROTOCOL_VERSION};
use sha2::{Digest, Sha256};
use tokio::sync::{oneshot, Mutex};

use super::telegram::{send_perm_prompt, BotClient, TelegramConfig};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const BLACKLIST_POLL_INTERVAL: Duration = Duration::from_secs(5);

pub trait BlacklistLoader: Send + Sync {
    fn load(&self) -> anyhow::Result<Vec<String>>;
    fn modified(&self) -> anyhow::Result<Option<SystemTime>> {
        Ok(None)
    }
}

#[derive(Debug, Clone)]
pub struct FsBlacklistLoader {
    conf_path: PathBuf,
}

impl FsBlacklistLoader {
    pub fn new(conf_path: PathBuf) -> Self {
        Self { conf_path }
    }
}

impl BlacklistLoader for FsBlacklistLoader {
    fn load(&self) -> anyhow::Result<Vec<String>> {
        let body = std::fs::read_to_string(&self.conf_path)?;
        Ok(body.lines().map(str::to_string).collect())
    }

    fn modified(&self) -> anyhow::Result<Option<SystemTime>> {
        Ok(std::fs::metadata(&self.conf_path)
            .and_then(|metadata| metadata.modified())
            .ok())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermVerdict {
    Approve,
    Deny,
}

#[derive(Clone, Default)]
pub struct PendingPermRegistry {
    waiters: Arc<Mutex<HashMap<String, oneshot::Sender<PermVerdict>>>>,
}

impl PendingPermRegistry {
    pub async fn insert(&self, id: String) -> oneshot::Receiver<PermVerdict> {
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.insert(id, tx);
        rx
    }

    pub async fn resolve(&self, id: &str, verdict: PermVerdict) -> bool {
        self.waiters
            .lock()
            .await
            .remove(id)
            .is_some_and(|tx| tx.send(verdict).is_ok())
    }

    async fn remove(&self, id: &str) {
        self.waiters.lock().await.remove(id);
    }
}

#[derive(Clone)]
pub struct PermService {
    state: StateHandle,
    telegram_config: Arc<TelegramConfig>,
    bot: Arc<dyn BotClient>,
    loader: Arc<dyn BlacklistLoader>,
    registry: PendingPermRegistry,
    cache: Arc<Mutex<BlacklistCache>>,
}

#[derive(Debug, Default)]
struct BlacklistCache {
    lines: Vec<String>,
    modified: Option<SystemTime>,
    last_checked: Option<SystemTime>,
    tampered: bool,
}

impl PermService {
    pub fn new(
        state: StateHandle,
        telegram_config: Arc<TelegramConfig>,
        bot: Arc<dyn BotClient>,
        loader: Arc<dyn BlacklistLoader>,
        registry: PendingPermRegistry,
    ) -> Self {
        Self {
            state,
            telegram_config,
            bot,
            loader,
            registry,
            cache: Arc::new(Mutex::new(BlacklistCache::default())),
        }
    }

    pub async fn check(&self, req: PermCheckRequest) -> anyhow::Result<PermCheckResponse> {
        if req.protocol_version != PROTOCOL_VERSION {
            anyhow::bail!("protocol mismatch");
        }

        let lines = match self.blacklist_lines().await {
            Ok(lines) => lines,
            Err(err) => {
                tracing::error!("CRITICAL: blacklist integrity failure: {err}");
                return Ok(response(
                    &req,
                    Decision::Deny,
                    "blacklist_tampered",
                    None,
                    true,
                ));
            }
        };

        let mut blacklist = Blacklist::new();
        for line in lines {
            blacklist.add_rule(&line)?;
        }
        blacklist.compile()?;

        let Some(rule) = blacklist.check(&req.command) else {
            return Ok(response(
                &req,
                Decision::Approve,
                "no_blacklist_match",
                None,
                false,
            ));
        };

        let id = next_perm_id();
        let command_hash = command_hash(&req.command);
        let now = now_string();
        let timeout = timeout_for(&req);
        let timeout_at = now_plus_string(timeout);
        let rx = self.registry.insert(id.clone()).await;
        let pending = PendingPerm {
            id: id.clone(),
            repo_id: request_repo_id(&req),
            command_hash: command_hash.clone(),
            status: PendingPermStatus::Pending,
            created_at: now,
            timeout_at,
            message_id: None,
        };
        self.state.insert_pending(pending).await?;

        let message = send_perm_prompt(
            self.bot.as_ref(),
            &self.telegram_config,
            &request_repo_id(&req),
            &id,
            &command_hash,
            &rule.pattern,
        )
        .await?;
        if let Some(message) = message {
            let mut snapshot = self.state.snapshot().await;
            if let Some(perm) = snapshot.pending_perms.get_mut(&id) {
                perm.status = PendingPermStatus::Sent;
                perm.message_id = Some(message.message_id);
                self.state.insert_pending(perm.clone()).await?;
            }
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(PermVerdict::Approve)) => Ok(response(
                &req,
                Decision::Approve,
                "user_approved",
                Some(rule.pattern),
                rule.destructive,
            )),
            Ok(Ok(PermVerdict::Deny)) => Ok(response(
                &req,
                Decision::Deny,
                "user_denied",
                Some(rule.pattern),
                rule.destructive,
            )),
            _ => {
                self.registry.remove(&id).await;
                self.state.expire_pending(&id).await?;
                let deny = rule.destructive || is_cached_destructive(&req.command);
                Ok(response(
                    &req,
                    if deny {
                        Decision::Deny
                    } else {
                        Decision::Approve
                    },
                    if deny {
                        "timeout_fail_closed"
                    } else {
                        "timeout_fail_open"
                    },
                    Some(rule.pattern),
                    rule.destructive,
                ))
            }
        }
    }

    async fn blacklist_lines(&self) -> anyhow::Result<Vec<String>> {
        let now = SystemTime::now();
        let mut cache = self.cache.lock().await;
        if cache.tampered {
            anyhow::bail!("blacklist_tampered");
        }
        let should_check = cache
            .last_checked
            .and_then(|checked| now.duration_since(checked).ok())
            .is_none_or(|elapsed| elapsed >= BLACKLIST_POLL_INTERVAL);
        if !cache.lines.is_empty() && !should_check {
            return Ok(cache.lines.clone());
        }

        let modified = self.loader.modified()?;
        if cache.lines.is_empty() || modified != cache.modified {
            match self.loader.load() {
                Ok(lines) => {
                    cache.lines = lines;
                    cache.modified = modified;
                    cache.tampered = false;
                }
                Err(err) => {
                    cache.tampered = true;
                    return Err(err);
                }
            }
        }
        cache.last_checked = Some(now);
        Ok(cache.lines.clone())
    }
}

fn response(
    req: &PermCheckRequest,
    verdict: Decision,
    reason: &str,
    matched_pattern: Option<String>,
    destructive: bool,
) -> PermCheckResponse {
    PermCheckResponse {
        protocol_version: PROTOCOL_VERSION,
        request_id: req.request_id.clone(),
        req_id: req.request_id.clone(),
        verdict,
        reason: reason.to_string(),
        matched_pattern,
        destructive,
    }
}

fn next_perm_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("perm-{millis}-{}", std::process::id())
}

fn command_hash(command: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(command.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn now_string() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn now_plus_string(duration: Duration) -> String {
    (time::OffsetDateTime::now_utc() + duration)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn timeout_for(req: &PermCheckRequest) -> Duration {
    if req.timeout_ms == 0 {
        DEFAULT_TIMEOUT
    } else {
        Duration::from_millis(req.timeout_ms).min(DEFAULT_TIMEOUT)
    }
}

fn request_repo_id(req: &PermCheckRequest) -> String {
    req.repo_id
        .clone()
        .or_else(|| req.repo_hint.clone())
        .unwrap_or_else(|| "unknown".to_string())
}

fn is_cached_destructive(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains("rm -rf")
        || lower.contains("git push -f")
        || lower.contains("git push --force")
        || lower.contains("drop table")
        || lower.contains("truncate table")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::telegram::{MockBot, RepoEntry, TelegramConfig};

    struct StaticLoader {
        lines: Vec<String>,
        err: Option<&'static str>,
    }

    impl BlacklistLoader for StaticLoader {
        fn load(&self) -> anyhow::Result<Vec<String>> {
            if let Some(err) = self.err {
                anyhow::bail!(err);
            }
            Ok(self.lines.clone())
        }
    }

    fn config() -> Arc<TelegramConfig> {
        Arc::new(TelegramConfig {
            allowed_chats: vec!["123".to_string()],
            repos: vec![RepoEntry {
                id: "rallyup".to_string(),
                display: "RallyUp".to_string(),
                path: "/tmp/RallyUp".to_string(),
                agents: vec![],
            }],
        })
    }

    fn request(command: &str, timeout_ms: u64) -> PermCheckRequest {
        PermCheckRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id: "req-1".to_string(),
            session_id: "sess-1".to_string(),
            tool: "Bash".to_string(),
            command: command.to_string(),
            repo_id: Some("rallyup".to_string()),
            repo_hint: Some("rallyup".to_string()),
            timeout_ms,
        }
    }

    #[tokio::test]
    async fn no_match_approves_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let state = agent_bus_core::state::spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let service = PermService::new(
            state,
            config(),
            Arc::new(MockBot::default()),
            Arc::new(StaticLoader {
                lines: vec!["rm\\s+-rf\tdestructive".to_string()],
                err: None,
            }),
            PendingPermRegistry::default(),
        );

        let resp = service.check(request("ls /tmp", 1)).await.unwrap();

        assert_eq!(resp.verdict, Decision::Approve);
        assert_eq!(resp.reason, "no_blacklist_match");
    }

    #[tokio::test]
    async fn timeout_denies_destructive_match() {
        let dir = tempfile::tempdir().unwrap();
        let state = agent_bus_core::state::spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let service = PermService::new(
            state.clone(),
            config(),
            Arc::new(MockBot::default()),
            Arc::new(StaticLoader {
                lines: vec!["rm\\s+-rf\tdestructive".to_string()],
                err: None,
            }),
            PendingPermRegistry::default(),
        );

        let resp = service.check(request("rm -rf /tmp/foo", 1)).await.unwrap();

        assert_eq!(resp.verdict, Decision::Deny);
        assert_eq!(resp.reason, "timeout_fail_closed");
        assert_eq!(
            state
                .snapshot()
                .await
                .pending_perms
                .values()
                .next()
                .map(|perm| &perm.status),
            Some(&PendingPermStatus::TimedOut)
        );
    }

    #[tokio::test]
    async fn approval_registry_resolves_held_request() {
        let dir = tempfile::tempdir().unwrap();
        let state = agent_bus_core::state::spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let registry = PendingPermRegistry::default();
        let service = PermService::new(
            state.clone(),
            config(),
            Arc::new(MockBot::default()),
            Arc::new(StaticLoader {
                lines: vec!["git\\s+push\\s+--force".to_string()],
                err: None,
            }),
            registry.clone(),
        );

        let handle = tokio::spawn(async move {
            service
                .check(request("git push --force origin main", 500))
                .await
                .unwrap()
        });
        for _ in 0..50 {
            if let Some(id) = state.snapshot().await.pending_perms.keys().next().cloned() {
                registry.resolve(&id, PermVerdict::Approve).await;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let resp = handle.await.unwrap();

        assert_eq!(resp.verdict, Decision::Approve);
        assert_eq!(resp.reason, "user_approved");
    }

    #[tokio::test]
    async fn tampered_blacklist_denies_all() {
        let dir = tempfile::tempdir().unwrap();
        let state = agent_bus_core::state::spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let service = PermService::new(
            state,
            config(),
            Arc::new(MockBot::default()),
            Arc::new(StaticLoader {
                lines: vec![],
                err: Some("hmac mismatch"),
            }),
            PendingPermRegistry::default(),
        );

        let resp = service.check(request("ls", 1)).await.unwrap();

        assert_eq!(resp.verdict, Decision::Deny);
        assert_eq!(resp.reason, "blacklist_tampered");
    }
}
