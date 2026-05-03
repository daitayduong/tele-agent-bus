use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_bus_core::approval_gate::ApprovalGate;
use agent_bus_core::repo_id::RepoId;
use agent_bus_core::state::{PendingPerm, PendingPermStatus, ResolvePendingOutcome, StateHandle};
use agent_bus_proto::{
    Decision, PermCheckRequest, PermCheckResponse, PermResolveResponse, PROTOCOL_VERSION,
};
use sha2::{Digest, Sha256};
use tokio::sync::{oneshot, Mutex};

use super::telegram::{
    pending_perm_status_text, send_perm_prompt, BotClient, MessageRef, TelegramConfig,
};

const APPROVAL_GATE_POLL_INTERVAL: Duration = Duration::from_secs(5);

pub trait GateLoader: Send + Sync {
    fn load(&self) -> anyhow::Result<Vec<String>>;
    fn modified(&self) -> anyhow::Result<Option<SystemTime>>;
}

#[derive(Debug, Clone)]
pub struct FsGateLoader {
    conf_path: PathBuf,
    hmac_path: PathBuf,
    key_path: PathBuf,
}

impl FsGateLoader {
    pub fn new(conf_path: PathBuf, hmac_path: PathBuf, key_path: PathBuf) -> Self {
        Self {
            conf_path,
            hmac_path,
            key_path,
        }
    }
}

impl GateLoader for FsGateLoader {
    fn load(&self) -> anyhow::Result<Vec<String>> {
        if !self.conf_path.exists() {
            return Ok(Vec::new());
        }
        agent_bus_core::approval_gate_integrity::load_and_verify(
            &self.conf_path,
            &self.hmac_path,
            &self.key_path,
        )
        .map_err(|e| {
            tracing::warn!(
                event = "approval_gate_integrity_failed",
                path = ?self.conf_path,
                error = %e
            );
            anyhow::anyhow!("gate integrity failed: {}", e)
        })
    }

    fn modified(&self) -> anyhow::Result<Option<SystemTime>> {
        Ok(std::fs::metadata(&self.conf_path)
            .and_then(|metadata| metadata.modified())
            .ok())
    }
}

pub struct MergedGateLoader {
    global_loader: Arc<dyn GateLoader>,
    repo_loader_fn: RepoLoaderFn,
}

type RepoLoaderFn = Box<dyn Fn(&RepoId) -> Arc<dyn GateLoader> + Send + Sync>;

impl MergedGateLoader {
    pub fn new(global_loader: Arc<dyn GateLoader>, repo_loader_fn: RepoLoaderFn) -> Self {
        Self {
            global_loader,
            repo_loader_fn,
        }
    }

    fn load_for(
        &self,
        repo_id: Option<&RepoId>,
    ) -> anyhow::Result<(Vec<String>, Option<SystemTime>)> {
        let mut global_patterns = self.global_loader.load()?;
        let global_modified = self.global_loader.modified()?;

        if let Some(repo_id) = repo_id {
            let repo_loader = (self.repo_loader_fn)(repo_id);
            let repo_patterns = repo_loader.load()?;
            let repo_modified = repo_loader.modified()?;
            global_patterns.extend(repo_patterns);

            let latest_modified = global_modified.max(repo_modified);
            Ok((global_patterns, latest_modified))
        } else {
            Ok((global_patterns, global_modified))
        }
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
    pub timeout: Duration,
    state: StateHandle,
    telegram_config: Arc<TelegramConfig>,
    bot: Arc<dyn BotClient>,
    loader: Arc<MergedGateLoader>,
    registry: PendingPermRegistry,
    cache: Arc<Mutex<HashMap<String, GateCache>>>,
}

#[derive(Debug, Default, Clone)]
struct GateCache {
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
        loader: Arc<MergedGateLoader>,
        registry: PendingPermRegistry,
        timeout: Duration,
    ) -> Self {
        Self {
            state,
            telegram_config,
            bot,
            loader,
            registry,
            timeout,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn timeout_for(&self, req: &PermCheckRequest) -> Duration {
        if req.timeout_ms == 0 {
            self.timeout
        } else {
            Duration::from_millis(req.timeout_ms).min(self.timeout)
        }
    }

    pub async fn check(&self, req: PermCheckRequest) -> anyhow::Result<PermCheckResponse> {
        if req.protocol_version != PROTOCOL_VERSION {
            anyhow::bail!("protocol mismatch");
        }

        let repo_id = req
            .repo_id
            .as_deref()
            .map(|s| RepoId::new(s.to_string()))
            .transpose()?;

        let lines = match self.gate_lines_for(repo_id.as_ref()).await {
            Ok(lines) => lines,
            Err(err) => {
                tracing::error!("CRITICAL: gate integrity failure: {err}");
                return Ok(response(
                    &req,
                    Decision::Deny,
                    "approval_gate_tampered",
                    None,
                    true,
                ));
            }
        };

        let mut gate = ApprovalGate::new();
        for line in lines {
            gate.add_rule(&line)?;
        }
        gate.compile()?;

        let Some(rule) = gate.check(&req.command) else {
            return Ok(response(
                &req,
                Decision::Approve,
                "no_gate_match",
                None,
                false,
            ));
        };

        let id = next_perm_id();
        let command_hash = command_hash(&req.command);
        let now = now_string();
        let timeout = self.timeout_for(&req);
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
            prompt_text: None,
        };
        self.state.insert_pending(pending).await?;

        let sent = send_perm_prompt(
            self.bot.as_ref(),
            &self.telegram_config,
            &request_repo_id(&req),
            &id,
            &req.command,
            &command_hash,
            &rule.pattern,
        )
        .await?;
        if let Some((message, prompt_text)) = sent {
            let mut snapshot = self.state.snapshot().await;
            if let Some(perm) = snapshot.pending_perms.get_mut(&id) {
                perm.status = PendingPermStatus::Sent;
                perm.message_id = Some(message.message_id);
                perm.prompt_text = Some(prompt_text);
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

    pub async fn resolve_external(
        &self,
        perm_id: String,
        decision: Decision,
        source: &str,
    ) -> anyhow::Result<PermResolveResponse> {
        let status = match (source, decision) {
            ("desktop", Decision::Approve) | ("native", Decision::Approve) => {
                PendingPermStatus::ApprovedByDesktop
            }
            ("desktop", Decision::Deny) | ("native", Decision::Deny) => {
                PendingPermStatus::DeniedByDesktop
            }
            (_, Decision::Approve) => PendingPermStatus::ApprovedByDesktop,
            (_, Decision::Deny) => PendingPermStatus::DeniedByDesktop,
        };
        let outcome = self
            .state
            .resolve_pending_if_open(perm_id.clone(), status)
            .await?;
        let final_status = match outcome {
            ResolvePendingOutcome::Resolved => {
                let verdict = match decision {
                    Decision::Approve => PermVerdict::Approve,
                    Decision::Deny => PermVerdict::Deny,
                };
                self.registry.resolve(&perm_id, verdict).await;
                self.disable_telegram_card(&perm_id, status, source).await?;
                status
            }
            ResolvePendingOutcome::AlreadyResolved(existing) => existing,
            ResolvePendingOutcome::Missing => {
                return Ok(PermResolveResponse {
                    protocol_version: PROTOCOL_VERSION,
                    perm_id,
                    resolved: false,
                    status: "missing".to_string(),
                });
            }
        };

        Ok(PermResolveResponse {
            protocol_version: PROTOCOL_VERSION,
            perm_id,
            resolved: matches!(outcome, ResolvePendingOutcome::Resolved),
            status: pending_perm_status_text(final_status).to_string(),
        })
    }

    async fn disable_telegram_card(
        &self,
        perm_id: &str,
        status: PendingPermStatus,
        source: &str,
    ) -> anyhow::Result<()> {
        let snapshot = self.state.snapshot().await;
        let Some(perm) = snapshot.pending_perms.get(perm_id) else {
            return Ok(());
        };
        let Some(message_id) = perm.message_id else {
            return Ok(());
        };
        let Some(chat_id) = self
            .telegram_config
            .allowed_chats
            .first()
            .and_then(|chat| chat.parse::<i64>().ok())
        else {
            return Ok(());
        };
        let decision = match status {
            PendingPermStatus::ApprovedByDesktop => "approved",
            PendingPermStatus::DeniedByDesktop => "denied",
            _ => pending_perm_status_text(status),
        };
        self.bot
            .edit_message_text(
                MessageRef {
                    chat_id,
                    message_id,
                },
                format!("Resolved on {source}: {decision}"),
            )
            .await?;
        Ok(())
    }

    async fn gate_lines_for(&self, repo_id: Option<&RepoId>) -> anyhow::Result<Vec<String>> {
        let now = SystemTime::now();
        let cache_key = repo_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "global".to_string());
        let mut cache_map = self.cache.lock().await;
        let cache = cache_map.entry(cache_key).or_default();

        if cache.tampered {
            anyhow::bail!("approval_gate_tampered");
        }
        let should_check = cache
            .last_checked
            .and_then(|checked| now.duration_since(checked).ok())
            .map_or(true, |elapsed| elapsed >= APPROVAL_GATE_POLL_INTERVAL);

        if !cache.lines.is_empty() && !should_check {
            return Ok(cache.lines.clone());
        }

        let (patterns, modified) = self.loader.load_for(repo_id)?;

        if cache.lines.is_empty() || modified != cache.modified {
            cache.lines = patterns;
            cache.modified = modified;
            cache.tampered = false;
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
    use crate::daemon::telegram::{CodexMode, MockBot, RepoEntry, TelegramConfig};
    use agent_bus_core::approval_gate_integrity;
    use std::path::Path;

    #[derive(Clone)]
    struct StaticLoader {
        lines: Vec<String>,
        err: Option<&'static str>,
    }

    impl GateLoader for StaticLoader {
        fn load(&self) -> anyhow::Result<Vec<String>> {
            if let Some(err) = self.err {
                anyhow::bail!(err);
            }
            Ok(self.lines.clone())
        }
        fn modified(&self) -> anyhow::Result<Option<SystemTime>> {
            Ok(None)
        }
    }

    fn config() -> Arc<TelegramConfig> {
        Arc::new(TelegramConfig {
            allowed_chats: vec!["123".to_string()],
            repos: vec![RepoEntry {
                id: "sample_repo".to_string(),
                display: "SampleRepo".to_string(),
                path: "/tmp/SampleRepo".to_string(),
                agents: vec![],
                codex_mode: CodexMode::LiveBridge,
            }],
        })
    }

    fn request(command: &str, timeout_ms: u64, repo_id: Option<&str>) -> PermCheckRequest {
        PermCheckRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id: "req-1".to_string(),
            session_id: "sess-1".to_string(),
            tool: "Bash".to_string(),
            command: command.to_string(),
            repo_id: repo_id.map(|s| s.to_string()),
            repo_hint: repo_id.map(|s| s.to_string()),
            timeout_ms,
        }
    }

    fn write_gate_triplet(dir: &Path, name: &str, body: &[u8]) -> (PathBuf, PathBuf, PathBuf) {
        let conf_path = dir.join(format!("{}.conf", name));
        let hmac_path = dir.join(format!("{}.conf.hmac", name));
        let key_path = dir.join("approval-gate.key");
        let key = b"01234567890123456789012345678901";
        std::fs::write(&key_path, key).unwrap();
        let sig = approval_gate_integrity::compute_hmac(key, body);

        std::fs::write(&conf_path, body).unwrap();
        std::fs::write(&hmac_path, sig).unwrap();

        (conf_path, hmac_path, key_path)
    }

    #[test]
    fn test_fs_loader_accepts_valid_triplet() {
        let dir = tempfile::tempdir().unwrap();
        let (conf_path, hmac_path, key_path) = write_gate_triplet(
            dir.path(),
            "gate",
            b"rm\\s+-rf\tdestructive\n^git push --force\n",
        );
        let loader = FsGateLoader::new(conf_path, hmac_path, key_path);

        let patterns = loader.load().unwrap();

        assert_eq!(
            patterns,
            vec![
                "rm\\s+-rf\tdestructive".to_string(),
                "^git push --force".to_string()
            ]
        );
    }

    #[test]
    fn test_fs_loader_rejects_tampered_conf() {
        let dir = tempfile::tempdir().unwrap();
        let (conf_path, hmac_path, key_path) =
            write_gate_triplet(dir.path(), "gate", b"rm\\s+-rf\tdestructive\n");
        std::fs::write(&conf_path, b"git\\s+push\\s+--force\tdestructive\n").unwrap();
        let loader = FsGateLoader::new(conf_path, hmac_path, key_path);

        assert!(loader.load().is_err());
    }

    #[test]
    fn test_fs_loader_rejects_missing_hmac() {
        let dir = tempfile::tempdir().unwrap();
        let (conf_path, hmac_path, key_path) =
            write_gate_triplet(dir.path(), "gate", b"rm\\s+-rf\tdestructive\n");
        std::fs::remove_file(&hmac_path).unwrap();
        let loader = FsGateLoader::new(conf_path, hmac_path, key_path);

        assert!(loader.load().is_err());
    }

    #[tokio::test]
    async fn no_match_approves_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let state = agent_bus_core::state::spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();

        let global_loader = Arc::new(StaticLoader {
            lines: vec![r"rm\s+-rf	destructive".to_string()],
            err: None,
        });
        let loader = Arc::new(MergedGateLoader::new(
            global_loader,
            Box::new(|_| {
                Arc::new(StaticLoader {
                    lines: vec![],
                    err: None,
                })
            }),
        ));

        let service = PermService::new(
            state,
            config(),
            Arc::new(MockBot::default()),
            loader,
            PendingPermRegistry::default(),
            Duration::from_secs(30),
        );

        let resp = service
            .check(request("ls /tmp", 1, Some("sample_repo")))
            .await
            .unwrap();

        assert_eq!(resp.verdict, Decision::Approve);
        assert_eq!(resp.reason, "no_gate_match");
    }

    #[tokio::test]
    async fn timeout_denies_destructive_match() {
        let dir = tempfile::tempdir().unwrap();
        let state = agent_bus_core::state::spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let global_loader = Arc::new(StaticLoader {
            lines: vec![r"rm\s+-rf	destructive".to_string()],
            err: None,
        });
        let loader = Arc::new(MergedGateLoader::new(
            global_loader,
            Box::new(|_| {
                Arc::new(StaticLoader {
                    lines: vec![],
                    err: None,
                })
            }),
        ));
        let service = PermService::new(
            state.clone(),
            config(),
            Arc::new(MockBot::default()),
            loader,
            PendingPermRegistry::default(),
            Duration::from_secs(30),
        );

        let resp = service
            .check(request("rm -rf /tmp/foo", 1, Some("sample_repo")))
            .await
            .unwrap();

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
        let global_loader = Arc::new(StaticLoader {
            lines: vec![r"git\s+push\s+--force".to_string()],
            err: None,
        });
        let loader = Arc::new(MergedGateLoader::new(
            global_loader,
            Box::new(|_| {
                Arc::new(StaticLoader {
                    lines: vec![],
                    err: None,
                })
            }),
        ));

        let service = PermService::new(
            state.clone(),
            config(),
            Arc::new(MockBot::default()),
            loader,
            registry.clone(),
            Duration::from_secs(30),
        );

        let handle = tokio::spawn(async move {
            service
                .check(request(
                    "git push --force origin main",
                    500,
                    Some("sample_repo"),
                ))
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
    async fn external_desktop_resolution_disables_telegram_card_and_releases_waiter() {
        let dir = tempfile::tempdir().unwrap();
        let state = agent_bus_core::state::spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let registry = PendingPermRegistry::default();
        let global_loader = Arc::new(StaticLoader {
            lines: vec![r"git\s+push\s+--force".to_string()],
            err: None,
        });
        let loader = Arc::new(MergedGateLoader::new(
            global_loader,
            Box::new(|_| {
                Arc::new(StaticLoader {
                    lines: vec![],
                    err: None,
                })
            }),
        ));
        let bot = Arc::new(MockBot::default());
        let service = PermService::new(
            state.clone(),
            config(),
            bot.clone(),
            loader,
            registry,
            Duration::from_secs(30),
        );

        let service_for_check = service.clone();
        let handle = tokio::spawn(async move {
            service_for_check
                .check(request(
                    "git push --force origin main",
                    5_000,
                    Some("sample_repo"),
                ))
                .await
                .unwrap()
        });

        let perm_id = loop {
            if let Some(id) = state.snapshot().await.pending_perms.keys().next().cloned() {
                break id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        let resolved = service
            .resolve_external(perm_id.clone(), Decision::Approve, "desktop")
            .await
            .unwrap();
        let resp = handle.await.unwrap();

        assert!(resolved.resolved);
        assert_eq!(resolved.status, "approved_by_desktop");
        assert_eq!(resp.verdict, Decision::Approve);
        assert_eq!(
            state
                .snapshot()
                .await
                .pending_perms
                .get(&perm_id)
                .map(|perm| perm.status),
            Some(PendingPermStatus::ApprovedByDesktop)
        );
        assert_eq!(
            bot.edited_messages()[0].text,
            "Resolved on desktop: approved"
        );
    }

    #[tokio::test]
    async fn tampered_gate_denies_all() {
        let dir = tempfile::tempdir().unwrap();
        let state = agent_bus_core::state::spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let global_loader = Arc::new(StaticLoader {
            lines: vec![],
            err: Some("hmac mismatch"),
        });
        let loader = Arc::new(MergedGateLoader::new(
            global_loader,
            Box::new(|_| {
                Arc::new(StaticLoader {
                    lines: vec![],
                    err: None,
                })
            }),
        ));
        let service = PermService::new(
            state,
            config(),
            Arc::new(MockBot::default()),
            loader,
            PendingPermRegistry::default(),
            Duration::from_secs(30),
        );

        let resp = service
            .check(request("ls", 1, Some("sample_repo")))
            .await
            .unwrap();

        assert_eq!(resp.verdict, Decision::Deny);
        assert_eq!(resp.reason, "approval_gate_tampered");
    }

    #[test]
    fn test_merged_loader_unions_patterns() {
        let global_loader = StaticLoader {
            lines: vec!["global_rule".to_string()],
            err: None,
        };
        let repo_loader = StaticLoader {
            lines: vec!["repo_rule".to_string()],
            err: None,
        };

        let loader = MergedGateLoader::new(
            Arc::new(global_loader),
            Box::new(move |_| Arc::new(repo_loader.clone())),
        );
        let repo_id = RepoId::new("test-repo".to_string()).unwrap();
        let (patterns, _) = loader.load_for(Some(&repo_id)).unwrap();
        assert_eq!(patterns, vec!["global_rule", "repo_rule"]);
    }

    #[tokio::test]
    async fn test_merged_loader_tampered_per_repo_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let state = agent_bus_core::state::spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let global_loader = Arc::new(StaticLoader {
            lines: vec!["global_rule".to_string()],
            err: None,
        });
        let repo_loader = StaticLoader {
            lines: vec![],
            err: Some("tampered"),
        };
        let loader = Arc::new(MergedGateLoader::new(
            global_loader,
            Box::new(move |_| Arc::new(repo_loader.clone())),
        ));
        let service = PermService::new(
            state,
            config(),
            Arc::new(MockBot::default()),
            loader,
            PendingPermRegistry::default(),
            Duration::from_secs(30),
        );

        let resp = service
            .check(request("ls", 1, Some("sample_repo")))
            .await
            .unwrap();
        assert_eq!(resp.verdict, Decision::Deny);
        assert_eq!(resp.reason, "approval_gate_tampered");
    }

    #[test]
    fn test_merged_loader_missing_per_repo_treated_as_empty() {
        let global_loader = StaticLoader {
            lines: vec!["global_rule".to_string()],
            err: None,
        };
        // This loader will simulate a file not found error, which should be handled gracefully.
        #[derive(Clone, Copy)]
        struct MissingFileLoader;
        impl GateLoader for MissingFileLoader {
            fn load(&self) -> anyhow::Result<Vec<String>> {
                Ok(vec![])
            }
            fn modified(&self) -> anyhow::Result<Option<SystemTime>> {
                Ok(None)
            }
        }
        let repo_loader = MissingFileLoader;

        let loader = MergedGateLoader::new(
            Arc::new(global_loader),
            Box::new(move |_| Arc::new(repo_loader)),
        );
        let repo_id = RepoId::new("test-repo".to_string()).unwrap();
        let (patterns, _) = loader.load_for(Some(&repo_id)).unwrap();
        assert_eq!(patterns, vec!["global_rule"]);
    }

    #[tokio::test]
    async fn test_merged_loader_tampered_global_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let state = agent_bus_core::state::spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let global_loader = Arc::new(StaticLoader {
            lines: vec![],
            err: Some("tampered"),
        });
        let repo_loader = StaticLoader {
            lines: vec!["repo_rule".to_string()],
            err: None,
        };
        let loader = Arc::new(MergedGateLoader::new(
            global_loader,
            Box::new(move |_| Arc::new(repo_loader.clone())),
        ));
        let service = PermService::new(
            state,
            config(),
            Arc::new(MockBot::default()),
            loader,
            PendingPermRegistry::default(),
            Duration::from_secs(30),
        );
        let resp = service
            .check(request("ls", 1, Some("sample_repo")))
            .await
            .unwrap();
        assert_eq!(resp.verdict, Decision::Deny);
        assert_eq!(resp.reason, "approval_gate_tampered");
    }
}
