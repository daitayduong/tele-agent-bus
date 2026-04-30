//! `AgentRunner` orchestration for agent CLI execution and auth rotation.
//!
//! Wraps CLI invocation with:
//! - candidate auth-context resolution
//! - failure classification (via `agent_bus_core::classifier`)
//! - state + event log updates on quota / auth events
//! - rotation to the next candidate when policy allows
//!
//! The actual CLI spawn is abstracted behind the [`AgentSpawner`] trait so the
//! orchestration loop is unit-testable without any real child process.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_bus_core::auth_context::{AuthContext, AuthContextsConfig};
use agent_bus_core::classifier::{
    default_classifier, Classification, ProviderClassifier, ResultKind, RunOutput,
};
use agent_bus_core::jsonl_scan;
use agent_bus_core::state::{
    AuthContextStatus, AuthContextStatusKind, PendingRotation, PendingRotationStatus, StateHandle,
    StateSnapshot,
};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::daemon::mobile_transcript;

pub const DEFAULT_QUOTA_COOLDOWN_SECS: i64 = 6 * 60 * 60; // 6h
pub const DEFAULT_RATE_LIMIT_COOLDOWN_SECS: i64 = 15 * 60; // 15m
pub const DEFAULT_MAX_ATTEMPTS: usize = 2;

/// Phase 4a.8 dptree dep: `None` in legacy mode (no `auth-contexts.yaml`),
/// `Some` when the daemon wraps CLI spawns through `AgentRunner`.
pub type SharedAgentRunner = Option<Arc<AgentRunner<crate::daemon::cli_spawner::CliSpawner>>>;

// ── Request / response types ────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum AgentRunMode {
    Fresh,
    ClaudeResume {
        mobile_uuid: String,
    },
    CodexResume {
        session_id: String,
        transcript_path: Option<PathBuf>,
    },
    WithMobileContext {
        mobile_uuid: String,
    },
}

#[derive(Debug, Clone)]
pub struct AgentRunRequest {
    pub agent: String,
    pub repo_id: String,
    pub repo_path: PathBuf,
    pub prompt: String,
    pub mode: AgentRunMode,
    pub preferred_context: Option<String>,
    pub timeout: Duration,
    pub request_id: String,
    pub chat_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentAttempt {
    pub auth_context: String,
    pub kind: ResultKind,
    pub exit_code: Option<i32>,
    pub classifier: Option<String>,
    pub started_at: String,
    pub finished_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRunResponse {
    pub stdout: String,
    pub stderr_excerpt: String,
    pub auth_context: String,
    pub attempts: Vec<AgentAttempt>,
    pub mobile_ctx_injected: bool,
    pub final_kind: ResultKind,
}

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("no usable auth contexts for agent '{agent}' (all exhausted/disabled/expired)")]
    NoUsableContexts { agent: String },
    #[error("approval required for context {agent}/{id}; rotation request {request_id} sent")]
    ApprovalPending {
        agent: String,
        id: String,
        request_id: String,
    },
    #[error("state error: {0}")]
    State(#[from] agent_bus_core::state::StateError),
    #[error("event log error: {0}")]
    EventLog(#[from] std::io::Error),
    #[error("spawner error: {0}")]
    Spawn(String),
}

// ── Spawner trait ───────────────────────────────────────────────────────

/// Concrete CLI spawn. Implementations must:
/// - apply per-context env isolation (e.g. `CLAUDE_CONFIG_DIR`, `CODEX_HOME`);
/// - respect `req.timeout` (kill on timeout, set `timed_out=true`);
/// - never leak daemon env vars containing tokens.
pub trait AgentSpawner: Send + Sync {
    fn spawn(
        &self,
        ctx: &AuthContext,
        req: &AgentRunRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SpawnOutcome, String>> + Send + '_>,
    >;
}

#[derive(Debug, Clone)]
pub struct SpawnOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

// ── Event log ───────────────────────────────────────────────────────────

/// Append-only JSONL event log (`~/.agent-bus/events.jsonl`).
#[derive(Debug, Clone)]
pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn append(&self, value: &serde_json::Value) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let mut line = serde_json::to_string(value).expect("event is serializable");
        line.push('\n');
        f.write_all(line.as_bytes())?;
        f.sync_all()?;
        Ok(())
    }
}

// ── Pure: candidate selection + usability ───────────────────────────────

/// Returns auth contexts for `agent` ordered as: preferred → active → config
/// order; duplicates removed. Only **enabled** contexts are returned; further
/// usability (cooldown, status) is checked by [`is_usable`].
pub fn select_candidates<'a>(
    cfg: &'a AuthContextsConfig,
    state: &StateSnapshot,
    agent: &str,
    preferred: Option<&str>,
) -> Vec<&'a AuthContext> {
    let all = cfg.contexts_for(agent);
    let mut ordered_ids: Vec<&str> = Vec::new();

    if let Some(id) = preferred {
        if all.iter().any(|c| c.id == id) {
            ordered_ids.push(id);
        }
    }
    if let Some(active) = state.active_auth_context.get(agent) {
        if !ordered_ids.contains(&active.as_str()) && all.iter().any(|c| c.id == *active) {
            ordered_ids.push(active.as_str());
        }
    }
    for c in all {
        if !ordered_ids.contains(&c.id.as_str()) {
            ordered_ids.push(c.id.as_str());
        }
    }

    let mut out = Vec::with_capacity(ordered_ids.len());
    for id in ordered_ids {
        if let Some(ctx) = all.iter().find(|c| c.id == id) {
            if ctx.enabled {
                out.push(ctx);
            }
        }
    }
    out
}

/// Is a context currently usable per §5.1 of the spec?
pub fn is_usable(
    ctx: &AuthContext,
    status: Option<&AuthContextStatus>,
    now: OffsetDateTime,
) -> bool {
    if !ctx.enabled {
        return false;
    }
    let Some(st) = status else {
        return true; // never seen before → available
    };
    match st.status {
        AuthContextStatusKind::Available | AuthContextStatusKind::AuthExpiringSoon => true,
        AuthContextStatusKind::Disabled
        | AuthContextStatusKind::AuthExpired
        | AuthContextStatusKind::ManualReauthRequired => false,
        AuthContextStatusKind::QuotaExhausted | AuthContextStatusKind::RateLimited => {
            match st.cooldown_until.as_deref() {
                Some(ts) => match OffsetDateTime::parse(ts, &Rfc3339) {
                    Ok(until) => now >= until,
                    Err(_) => true, // malformed timestamp → treat as expired
                },
                None => false,
            }
        }
        AuthContextStatusKind::UnknownFailure => true, // conservative: let next call retry
    }
}

/// Derive `cooldown_until` for a failure result. Currently uses fixed
/// defaults; provider-specific retry-after parsing is deferred.
pub fn derive_cooldown(kind: ResultKind, now: OffsetDateTime) -> Option<OffsetDateTime> {
    let secs = match kind {
        ResultKind::QuotaExhausted => DEFAULT_QUOTA_COOLDOWN_SECS,
        ResultKind::RateLimited => DEFAULT_RATE_LIMIT_COOLDOWN_SECS,
        _ => return None,
    };
    Some(now + time::Duration::seconds(secs))
}

pub fn kind_to_status(kind: ResultKind) -> AuthContextStatusKind {
    match kind {
        ResultKind::Success => AuthContextStatusKind::Available,
        ResultKind::QuotaExhausted => AuthContextStatusKind::QuotaExhausted,
        ResultKind::RateLimited => AuthContextStatusKind::RateLimited,
        ResultKind::AuthExpired => AuthContextStatusKind::AuthExpired,
        ResultKind::ManualReauthRequired => AuthContextStatusKind::ManualReauthRequired,
        ResultKind::Timeout => AuthContextStatusKind::UnknownFailure,
        ResultKind::UnknownFailure => AuthContextStatusKind::UnknownFailure,
    }
}

// ── Orchestrator ────────────────────────────────────────────────────────

pub struct AgentRunner<S: AgentSpawner> {
    pub spawner: S,
    pub cfg: AuthContextsConfig,
    pub state: StateHandle,
    pub events: EventLog,
    pub classifiers: BTreeMap<String, ProviderClassifier>,
    pub max_attempts: usize,
    /// Clock injection for tests.
    pub now_fn: fn() -> OffsetDateTime,
}

impl<S: AgentSpawner> AgentRunner<S> {
    pub fn new(spawner: S, cfg: AuthContextsConfig, state: StateHandle, events: EventLog) -> Self {
        let mut classifiers = BTreeMap::new();
        for &agent in &["claude", "codex", "gemini"] {
            if let Some(c) = default_classifier(agent) {
                classifiers.insert(agent.to_string(), c);
            }
        }
        Self {
            spawner,
            cfg,
            state,
            events,
            classifiers,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            now_fn: OffsetDateTime::now_utc,
        }
    }

    pub async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResponse, RunnerError> {
        let snap = self.state.snapshot().await;
        let mut req = req;
        let mut mobile_ctx_injected = false;

        // Phase 4b.2: Mobile Context Injection
        if let AgentRunMode::WithMobileContext { mobile_uuid } = &req.mode {
            let mobile_cfg = &self.cfg.mobile_context;
            if !mobile_cfg.enabled {
                let _ = self.events.append(&serde_json::json!({
                    "ts": (self.now_fn)().format(&Rfc3339).unwrap_or_default(),
                    "event": "mobile_context_skipped",
                    "agent": &req.agent,
                    "mobile_uuid": mobile_uuid,
                    "reason": "disabled",
                }));
            } else {
                let claude_ctx_id = snap.active_auth_context.get("claude");
                let claude_ctx = claude_ctx_id.and_then(|id| self.cfg.context("claude", id));

                let jsonl_path = claude_ctx.map(|ctx| {
                    ctx.profile_dir
                        .join("projects")
                        .join(project_hash_for_repo(&req.repo_path))
                        .join(format!("{}.jsonl", mobile_uuid))
                });

                if let Some(path) = jsonl_path {
                    if path.exists() {
                        match mobile_transcript::read_claude_jsonl(&path) {
                            Ok(msgs) => {
                                let (rendered, stats) = mobile_transcript::render_context(
                                    &msgs,
                                    mobile_cfg.max_bytes,
                                    mobile_cfg.max_messages,
                                    mobile_cfg.include_tool_use,
                                );
                                req.prompt = format!(
                                    "{}\n\n<user_prompt>\n{}\n</user_prompt>",
                                    rendered, req.prompt
                                );
                                let _ = self.events.append(&serde_json::json!({
                                    "ts": (self.now_fn)().format(&Rfc3339).unwrap_or_default(),
                                    "event": "mobile_context_injected",
                                    "agent": &req.agent,
                                    "mobile_uuid": mobile_uuid,
                                    "bytes": stats.bytes,
                                    "messages": stats.messages_used,
                                    "trimmed": stats.trimmed,
                                }));
                                mobile_ctx_injected = true;
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "failed to read mobile transcript");
                                let _ = self.events.append(&serde_json::json!({
                                    "ts": (self.now_fn)().format(&Rfc3339).unwrap_or_default(),
                                    "event": "mobile_context_skipped",
                                    "agent": &req.agent,
                                    "mobile_uuid": mobile_uuid,
                                    "reason": "read_error",
                                }));
                            }
                        }
                    } else {
                        let _ = self.events.append(&serde_json::json!({
                            "ts": (self.now_fn)().format(&Rfc3339).unwrap_or_default(),
                            "event": "mobile_context_skipped",
                            "agent": &req.agent,
                            "mobile_uuid": mobile_uuid,
                            "reason": "jsonl_missing",
                        }));
                    }
                } else {
                    let _ = self.events.append(&serde_json::json!({
                        "ts": (self.now_fn)().format(&Rfc3339).unwrap_or_default(),
                        "event": "mobile_context_skipped",
                        "agent": &req.agent,
                        "mobile_uuid": mobile_uuid,
                        "reason": "claude_context_missing",
                    }));
                }
            }
        }

        let all_candidates = select_candidates(
            &self.cfg,
            &snap,
            &req.agent,
            req.preferred_context.as_deref(),
        );
        if all_candidates.is_empty() {
            return Err(RunnerError::NoUsableContexts {
                agent: req.agent.clone(),
            });
        }

        let now = (self.now_fn)();
        let usable: Vec<&AuthContext> = all_candidates
            .into_iter()
            .filter(|c| {
                let st = snap
                    .auth_context_status
                    .get(&req.agent)
                    .and_then(|m| m.get(&c.id));
                is_usable(c, st, now)
            })
            .collect();

        if usable.is_empty() {
            return Err(RunnerError::NoUsableContexts {
                agent: req.agent.clone(),
            });
        }

        let mut attempts = Vec::new();
        let classifier = self
            .classifiers
            .get(&req.agent)
            .cloned()
            .ok_or_else(|| RunnerError::Spawn(format!("no classifier for {}", req.agent)))?;

        for (idx, ctx) in usable.iter().take(self.max_attempts).enumerate() {
            let started_at = (self.now_fn)();
            let outcome = self
                .spawner
                .spawn(ctx, &req)
                .await
                .map_err(RunnerError::Spawn)?;
            let finished_at = (self.now_fn)();

            let out = RunOutput {
                exit_code: outcome.exit_code,
                stdout: &outcome.stdout,
                stderr: &outcome.stderr,
                timed_out: outcome.timed_out,
            };
            let mut class: Classification = classifier.classify(&out);
            if class.kind == ResultKind::UnknownFailure {
                if let Some((kind, matched)) = scan_claude_jsonl_tail(&req, ctx) {
                    class.kind = kind;
                    class.classifier = Some(matched);
                }
            }
            let kind = class.kind;

            attempts.push(AgentAttempt {
                auth_context: ctx.id.clone(),
                kind,
                exit_code: outcome.exit_code,
                classifier: class.classifier.clone(),
                started_at: started_at.format(&Rfc3339).unwrap_or_default(),
                finished_at: finished_at.format(&Rfc3339).unwrap_or_default(),
            });

            match kind {
                ResultKind::Success => {
                    // Mark available + record active context.
                    self.mark_available(&req.agent, &ctx.id, finished_at)
                        .await?;
                    self.state
                        .set_active_auth_context(&req.agent, &ctx.id)
                        .await?;
                    // AC-Q8 / 4a.10: on successful rotation (idx > 0),
                    // emit an auth_context_rotated event with from->to path.
                    if idx > 0 {
                        if let Some(prev) = attempts.get(idx - 1) {
                            let _ = self.events.append(&serde_json::json!({
                                "type": "auth_context_rotated",
                                "agent": &req.agent,
                                "from": &prev.auth_context,
                                "to": &ctx.id,
                                "repo_id": &req.repo_id,
                                "request_id": &req.request_id,
                                "trigger": format!("{:?}", prev.kind),
                                "ts": finished_at.format(&Rfc3339).unwrap_or_default(),
                            }));
                        }
                    }
                    return Ok(AgentRunResponse {
                        stdout: outcome.stdout,
                        stderr_excerpt: excerpt(&outcome.stderr),
                        auth_context: ctx.id.clone(),
                        attempts,
                        mobile_ctx_injected,
                        final_kind: ResultKind::Success,
                    });
                }
                ResultKind::QuotaExhausted | ResultKind::RateLimited => {
                    self.record_quota_event(
                        &req,
                        ctx,
                        kind,
                        class.classifier.as_deref(),
                        &outcome.stderr,
                        finished_at,
                    )
                    .await?;

                    // Try next? Allow rotation when either the exhausted source
                    // permits leaving it or the target permits rotating into it.
                    let next = usable.get(idx + 1);
                    let can_rotate = self.cfg.defaults.auto_rotate
                        || ctx.auto_rotate
                        || next.is_some_and(|next| next.auto_rotate);
                    if !can_rotate || idx + 1 >= self.max_attempts {
                        return Ok(self.failure_response(
                            outcome,
                            ctx.id.clone(),
                            attempts,
                            kind,
                            mobile_ctx_injected,
                        ));
                    }

                    // Check next candidate's approval gate.
                    if let Some(next) = next {
                        if next.require_owner_approval {
                            let request_id = format!("rot_{}", short_id(finished_at));
                            let rot = PendingRotation {
                                id: request_id.clone(),
                                agent: req.agent.clone(),
                                from: ctx.id.clone(),
                                to: next.id.clone(),
                                repo_id: req.repo_id.clone(),
                                request_id: req.request_id.clone(),
                                chat_id: req.chat_id.unwrap_or(0),
                                expires_at: (finished_at + time::Duration::minutes(10))
                                    .format(&Rfc3339)
                                    .unwrap_or_default(),
                                status: PendingRotationStatus::Pending,
                            };
                            self.state.insert_pending_rotation(rot).await?;
                            return Err(RunnerError::ApprovalPending {
                                agent: req.agent.clone(),
                                id: next.id.clone(),
                                request_id,
                            });
                        }
                    }
                    // fall through → next iteration picks next usable context
                    continue;
                }
                ResultKind::AuthExpired | ResultKind::ManualReauthRequired => {
                    self.mark_status(
                        &req.agent,
                        &ctx.id,
                        AuthContextStatusKind::ManualReauthRequired,
                        None,
                        finished_at,
                    )
                    .await?;
                    self.record_auth_event(
                        &req,
                        ctx,
                        kind,
                        class.classifier.as_deref(),
                        &outcome.stderr,
                        finished_at,
                    )
                    .await?;
                    // Rotation under normal policy
                    let next = usable.get(idx + 1);
                    let can_rotate = self.cfg.defaults.auto_rotate
                        || ctx.auto_rotate
                        || next.is_some_and(|next| next.auto_rotate);
                    if !can_rotate || idx + 1 >= self.max_attempts {
                        return Ok(self.failure_response(
                            outcome,
                            ctx.id.clone(),
                            attempts,
                            kind,
                            mobile_ctx_injected,
                        ));
                    }
                    continue;
                }
                ResultKind::Timeout | ResultKind::UnknownFailure => {
                    // Do NOT auto-rotate on unknown/timeout in v1.
                    return Ok(self.failure_response(
                        outcome,
                        ctx.id.clone(),
                        attempts,
                        kind,
                        mobile_ctx_injected,
                    ));
                }
            }
        }

        // All attempts consumed without success.
        let last = attempts
            .last()
            .map(|a| a.kind)
            .unwrap_or(ResultKind::UnknownFailure);
        Err(RunnerError::NoUsableContexts {
            agent: req.agent.clone(),
        })
        .map_err(|e| {
            tracing::warn!(agent=%req.agent, last_kind=?last, "runner exhausted candidates");
            e
        })
    }

    fn failure_response(
        &self,
        outcome: SpawnOutcome,
        auth_context: String,
        attempts: Vec<AgentAttempt>,
        kind: ResultKind,
        mobile_ctx_injected: bool,
    ) -> AgentRunResponse {
        AgentRunResponse {
            stdout: outcome.stdout,
            stderr_excerpt: excerpt(&outcome.stderr),
            auth_context,
            attempts,
            mobile_ctx_injected,
            final_kind: kind,
        }
    }

    async fn mark_available(
        &self,
        agent: &str,
        id: &str,
        now: OffsetDateTime,
    ) -> Result<(), RunnerError> {
        self.mark_status(agent, id, AuthContextStatusKind::Available, None, now)
            .await
    }

    async fn mark_status(
        &self,
        agent: &str,
        id: &str,
        kind: AuthContextStatusKind,
        cooldown_until: Option<OffsetDateTime>,
        now: OffsetDateTime,
    ) -> Result<(), RunnerError> {
        let status = AuthContextStatus {
            status: kind,
            cooldown_until: cooldown_until.and_then(|t| t.format(&Rfc3339).ok()),
            last_event_id: None,
            updated_at: now.format(&Rfc3339).unwrap_or_default(),
        };
        self.state
            .set_auth_context_status(agent, id, status)
            .await?;
        Ok(())
    }

    async fn record_quota_event(
        &self,
        req: &AgentRunRequest,
        ctx: &AuthContext,
        kind: ResultKind,
        classifier: Option<&str>,
        stderr: &str,
        now: OffsetDateTime,
    ) -> Result<(), RunnerError> {
        let cooldown = derive_cooldown(kind, now);
        let status_kind = kind_to_status(kind);
        self.mark_status(&req.agent, &ctx.id, status_kind, cooldown, now)
            .await?;

        let evt = serde_json::json!({
            "ts": now.format(&Rfc3339).unwrap_or_default(),
            "event": match kind {
                ResultKind::QuotaExhausted => "quota_exhausted",
                ResultKind::RateLimited => "rate_limited",
                _ => "quota_event",
            },
            "id": format!("qevt_{}", short_id(now)),
            "agent": req.agent,
            "repo_id": req.repo_id,
            "auth_context": ctx.id,
            "status": format!("{:?}", status_kind).to_lowercase(),
            "cooldown_until": cooldown.and_then(|t| t.format(&Rfc3339).ok()),
            "classifier": classifier,
            "raw_excerpt": excerpt(stderr),
        });
        self.events.append(&evt)?;
        Ok(())
    }

    async fn record_auth_event(
        &self,
        req: &AgentRunRequest,
        ctx: &AuthContext,
        kind: ResultKind,
        classifier: Option<&str>,
        stderr: &str,
        now: OffsetDateTime,
    ) -> Result<(), RunnerError> {
        let evt = serde_json::json!({
            "ts": now.format(&Rfc3339).unwrap_or_default(),
            "event": "auth_expired",
            "id": format!("aevt_{}", short_id(now)),
            "agent": req.agent,
            "repo_id": req.repo_id,
            "auth_context": ctx.id,
            "kind": format!("{:?}", kind).to_lowercase(),
            "classifier": classifier,
            "raw_excerpt": excerpt(stderr),
        });
        self.events.append(&evt)?;
        Ok(())
    }
}

fn excerpt(s: &str) -> String {
    // Cap at 300 UTF-8 bytes on a char boundary.
    const MAX: usize = 300;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn short_id(now: OffsetDateTime) -> String {
    format!("{}", now.unix_timestamp_nanos() % 1_000_000_000)
}

fn scan_claude_jsonl_tail(
    req: &AgentRunRequest,
    ctx: &AuthContext,
) -> Option<(ResultKind, String)> {
    if req.agent != "claude" {
        return None;
    }
    let mobile_uuid = match &req.mode {
        AgentRunMode::ClaudeResume { mobile_uuid }
        | AgentRunMode::WithMobileContext { mobile_uuid } => mobile_uuid,
        AgentRunMode::CodexResume { .. } => return None,
        AgentRunMode::Fresh => return None,
    };
    let path = ctx
        .profile_dir
        .join("projects")
        .join(project_hash_for_repo(&req.repo_path))
        .join(format!("{mobile_uuid}.jsonl"));
    jsonl_scan::scan_and_classify(&path)
}

fn project_hash_for_repo(repo_path: &std::path::Path) -> String {
    repo_path.to_string_lossy().replace('/', "-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bus_core::state::spawn_state_actor;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    // ── Fake spawner with scripted outcomes ──────────────────────────────

    #[derive(Default, Clone)]
    struct FakeSpawner {
        script: Arc<Mutex<Vec<SpawnOutcome>>>,
        calls: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl FakeSpawner {
        fn new(outcomes: Vec<SpawnOutcome>) -> Self {
            Self {
                script: Arc::new(Mutex::new(outcomes)),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl AgentSpawner for FakeSpawner {
        fn spawn(
            &self,
            ctx: &AuthContext,
            req: &AgentRunRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<SpawnOutcome, String>> + Send + '_>,
        > {
            self.calls
                .lock()
                .unwrap()
                .push((ctx.id.clone(), req.prompt.clone()));
            let next = {
                let mut s = self.script.lock().unwrap();
                if s.is_empty() {
                    None
                } else {
                    Some(s.remove(0))
                }
            };
            Box::pin(async move { next.ok_or_else(|| "spawner script exhausted".to_string()) })
        }
    }

    fn ok_out(stdout: &str) -> SpawnOutcome {
        SpawnOutcome {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            timed_out: false,
        }
    }
    fn quota_out() -> SpawnOutcome {
        SpawnOutcome {
            stdout: String::new(),
            stderr: "Claude usage limit reached".to_string(),
            exit_code: Some(1),
            timed_out: false,
        }
    }
    fn auth_expired_out() -> SpawnOutcome {
        SpawnOutcome {
            stdout: String::new(),
            stderr: "please sign in first".to_string(),
            exit_code: Some(1),
            timed_out: false,
        }
    }

    fn cfg_two_claude_contexts(
        home: &Path,
        auto_rotate: bool,
        approval: bool,
    ) -> AuthContextsConfig {
        cfg_two_claude_contexts_explicit(home, auto_rotate, false, approval)
    }

    fn cfg_two_claude_contexts_explicit(
        home: &Path,
        john_auto_rotate: bool,
        partner_auto_rotate: bool,
        approval: bool,
    ) -> AuthContextsConfig {
        // Build a config directly via parse to exercise the real path validator.
        let y = format!(
            r#"
version: 1
defaults:
  auto_rotate: false
  require_owner_approval: {appr}
agents:
  claude:
    contexts:
      - id: john
        profile_dir: ~/.agent-bus/auth/claude/john
        auto_rotate: {john_auto}
        require_owner_approval: false
      - id: partner
        profile_dir: ~/.agent-bus/auth/claude/partner
        auto_rotate: {partner_auto}
        require_owner_approval: {appr}
"#,
            john_auto = john_auto_rotate,
            partner_auto = partner_auto_rotate,
            appr = approval,
        );
        AuthContextsConfig::parse(&y, home).unwrap()
    }

    fn req(agent: &str) -> AgentRunRequest {
        AgentRunRequest {
            agent: agent.to_string(),
            repo_id: "sample_repo".to_string(),
            repo_path: PathBuf::from("/tmp/rp"),
            prompt: "hi".to_string(),
            mode: AgentRunMode::Fresh,
            preferred_context: None,
            timeout: Duration::from_secs(5),
            request_id: "req-1".to_string(),
            chat_id: Some(123),
        }
    }

    async fn fresh_state() -> (tempfile::TempDir, StateHandle) {
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        (dir, handle)
    }

    fn events_log(dir: &Path) -> EventLog {
        EventLog::new(dir.join("events.jsonl"))
    }

    // ── AC-Q1 / AC-Q8 ────────────────────────────────────────────────────

    #[tokio::test]
    async fn classifies_and_emits_quota_event_on_failure() {
        let (dir, state) = fresh_state().await;
        let home = PathBuf::from("/home/alice");
        let cfg = cfg_two_claude_contexts(&home, false, false); // no rotate
        let spawner = FakeSpawner::new(vec![quota_out()]);
        let events = events_log(dir.path());
        let runner = AgentRunner::new(spawner.clone(), cfg, state.clone(), events.clone());

        let resp = runner.run(req("claude")).await.unwrap();
        assert_eq!(resp.final_kind, ResultKind::QuotaExhausted);
        assert_eq!(resp.auth_context, "john");

        let snap = state.snapshot().await;
        assert_eq!(
            snap.auth_context_status["claude"]["john"].status,
            AuthContextStatusKind::QuotaExhausted
        );
        let log = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
        assert!(log.contains("quota_exhausted"));
        assert!(log.contains(r#""agent":"claude""#));
        assert!(log.contains(r#""auth_context":"john""#));
    }

    // ── AC-Q3: auto-rotate on quota ──────────────────────────────────────

    #[tokio::test]
    async fn auto_rotates_on_quota_when_policy_allows() {
        let (dir, state) = fresh_state().await;
        let home = PathBuf::from("/home/alice");
        let cfg = cfg_two_claude_contexts(&home, true, false);
        let spawner = FakeSpawner::new(vec![quota_out(), ok_out("hello world")]);
        let events = events_log(dir.path());
        let runner = AgentRunner::new(spawner.clone(), cfg, state.clone(), events.clone());

        let resp = runner.run(req("claude")).await.unwrap();
        assert_eq!(resp.final_kind, ResultKind::Success);
        assert_eq!(resp.auth_context, "partner");
        assert_eq!(resp.attempts.len(), 2);
        assert_eq!(resp.attempts[0].kind, ResultKind::QuotaExhausted);
        assert_eq!(resp.attempts[1].kind, ResultKind::Success);
        let calls = spawner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "john");
        assert_eq!(calls[1].0, "partner");
    }

    #[tokio::test]
    async fn auto_rotates_when_target_context_allows_rotation() {
        let (dir, state) = fresh_state().await;
        let home = PathBuf::from("/home/alice");
        let cfg = cfg_two_claude_contexts_explicit(&home, false, true, false);
        let spawner = FakeSpawner::new(vec![quota_out(), ok_out("partner ok")]);
        let events = events_log(dir.path());
        let runner = AgentRunner::new(spawner.clone(), cfg, state, events);

        let resp = runner.run(req("claude")).await.unwrap();
        assert_eq!(resp.final_kind, ResultKind::Success);
        assert_eq!(resp.auth_context, "partner");
        let calls = spawner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "john");
        assert_eq!(calls[1].0, "partner");
    }

    // ── AC-Q8 / 4a.10: rotation emits auth_context_rotated event ──────────

    #[tokio::test]
    async fn auto_rotate_emits_auth_context_rotated_event() {
        let (dir, state) = fresh_state().await;
        let home = PathBuf::from("/home/alice");
        let cfg = cfg_two_claude_contexts(&home, true, false);
        let spawner = FakeSpawner::new(vec![quota_out(), ok_out("ok")]);
        let events = events_log(dir.path());
        let runner = AgentRunner::new(spawner, cfg, state, events);

        let resp = runner.run(req("claude")).await.unwrap();
        assert_eq!(resp.final_kind, ResultKind::Success);

        let log = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
        assert!(log.contains("auth_context_rotated"), "log: {log}");
        assert!(log.contains(r#""from":"john""#), "log: {log}");
        assert!(log.contains(r#""to":"partner""#), "log: {log}");
    }

    // ── AC-Q4: approval required ─────────────────────────────────────────

    #[tokio::test]
    async fn approval_required_does_not_spawn_partner() {
        let (dir, state) = fresh_state().await;
        let home = PathBuf::from("/home/alice");
        let cfg = cfg_two_claude_contexts(&home, true, true);
        let spawner = FakeSpawner::new(vec![quota_out()]); // only john called
        let events = events_log(dir.path());
        let runner = AgentRunner::new(spawner.clone(), cfg, state.clone(), events.clone());

        let err = runner.run(req("claude")).await.unwrap_err();
        assert!(matches!(err, RunnerError::ApprovalPending { .. }));
        let calls = spawner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "john");

        let snap = state.snapshot().await;
        assert_eq!(snap.pending_rotations.len(), 1);
        let rot = snap.pending_rotations.values().next().unwrap();
        assert_eq!(rot.from, "john");
        assert_eq!(rot.to, "partner");
        assert_eq!(rot.status, PendingRotationStatus::Pending);
    }

    // ── AC-Q2: cooldown skip ─────────────────────────────────────────────

    #[tokio::test]
    async fn skips_context_with_future_cooldown() {
        let (dir, state) = fresh_state().await;
        let home = PathBuf::from("/home/alice");
        let cfg = cfg_two_claude_contexts(&home, true, false);

        // Pre-mark john as quota_exhausted with future cooldown
        let future = OffsetDateTime::now_utc() + time::Duration::hours(5);
        state
            .set_auth_context_status(
                "claude",
                "john",
                AuthContextStatus {
                    status: AuthContextStatusKind::QuotaExhausted,
                    cooldown_until: Some(future.format(&Rfc3339).unwrap()),
                    last_event_id: None,
                    updated_at: OffsetDateTime::now_utc().format(&Rfc3339).unwrap(),
                },
            )
            .await
            .unwrap();

        let spawner = FakeSpawner::new(vec![ok_out("ok")]);
        let events = events_log(dir.path());
        let runner = AgentRunner::new(spawner.clone(), cfg, state.clone(), events.clone());

        let resp = runner.run(req("claude")).await.unwrap();
        assert_eq!(resp.auth_context, "partner");
        let calls = spawner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "partner");
    }

    // ── AC-Q5: all exhausted ─────────────────────────────────────────────

    #[tokio::test]
    async fn returns_no_usable_contexts_when_all_unavailable() {
        let (dir, state) = fresh_state().await;
        let home = PathBuf::from("/home/alice");
        let cfg = cfg_two_claude_contexts(&home, false, false);

        let future = OffsetDateTime::now_utc() + time::Duration::hours(1);
        for id in ["john", "partner"] {
            state
                .set_auth_context_status(
                    "claude",
                    id,
                    AuthContextStatus {
                        status: AuthContextStatusKind::QuotaExhausted,
                        cooldown_until: Some(future.format(&Rfc3339).unwrap()),
                        last_event_id: None,
                        updated_at: OffsetDateTime::now_utc().format(&Rfc3339).unwrap(),
                    },
                )
                .await
                .unwrap();
        }

        let spawner = FakeSpawner::new(vec![]); // no calls expected
        let events = events_log(dir.path());
        let runner = AgentRunner::new(spawner, cfg, state, events);

        let err = runner.run(req("claude")).await.unwrap_err();
        assert!(matches!(err, RunnerError::NoUsableContexts { .. }));
    }

    // ── AC-Q6: auth expired ──────────────────────────────────────────────

    #[tokio::test]
    async fn auth_expired_marks_reauth_required() {
        let (dir, state) = fresh_state().await;
        let home = PathBuf::from("/home/alice");
        let cfg = cfg_two_claude_contexts(&home, false, false); // no rotate
        let spawner = FakeSpawner::new(vec![auth_expired_out()]);
        let events = events_log(dir.path());
        let runner = AgentRunner::new(spawner, cfg, state.clone(), events);

        let resp = runner.run(req("claude")).await.unwrap();
        assert_eq!(resp.final_kind, ResultKind::AuthExpired);

        let snap = state.snapshot().await;
        assert_eq!(
            snap.auth_context_status["claude"]["john"].status,
            AuthContextStatusKind::ManualReauthRequired
        );
    }

    // ── AC-Q11-ish: preferred ordering respected ─────────────────────────

    #[tokio::test]
    async fn preferred_context_tried_first() {
        let (dir, state) = fresh_state().await;
        let home = PathBuf::from("/home/alice");
        let cfg = cfg_two_claude_contexts(&home, true, false);
        let spawner = FakeSpawner::new(vec![ok_out("done")]);
        let events = events_log(dir.path());
        let runner = AgentRunner::new(spawner.clone(), cfg, state.clone(), events);

        let mut r = req("claude");
        r.preferred_context = Some("partner".to_string());
        let resp = runner.run(r).await.unwrap();
        assert_eq!(resp.auth_context, "partner");
        let calls = spawner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "partner");
    }

    // ── Pure selection helper tests ──────────────────────────────────────

    #[test]
    fn excerpt_caps_at_char_boundary() {
        let s = "á".repeat(200); // 400 UTF-8 bytes
        let e = excerpt(&s);
        assert!(e.len() <= 300);
        assert!(e.chars().all(|c| c == 'á'));
    }

    #[test]
    fn is_usable_respects_future_cooldown() {
        let home = PathBuf::from("/home/alice");
        let cfg = cfg_two_claude_contexts(&home, false, false);
        let ctx = cfg.context("claude", "john").unwrap();

        let now = OffsetDateTime::now_utc();
        let future = now + time::Duration::hours(1);

        let st = AuthContextStatus {
            status: AuthContextStatusKind::QuotaExhausted,
            cooldown_until: Some(future.format(&Rfc3339).unwrap()),
            last_event_id: None,
            updated_at: now.format(&Rfc3339).unwrap(),
        };
        assert!(!is_usable(ctx, Some(&st), now));

        // Past cooldown
        let past = now - time::Duration::hours(1);
        let st2 = AuthContextStatus {
            status: AuthContextStatusKind::QuotaExhausted,
            cooldown_until: Some(past.format(&Rfc3339).unwrap()),
            last_event_id: None,
            updated_at: now.format(&Rfc3339).unwrap(),
        };
        assert!(is_usable(ctx, Some(&st2), now));
    }

    #[test]
    fn derive_cooldown_uses_defaults() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let c_quota = derive_cooldown(ResultKind::QuotaExhausted, now).unwrap();
        assert_eq!((c_quota - now).whole_seconds(), DEFAULT_QUOTA_COOLDOWN_SECS);
        let c_rate = derive_cooldown(ResultKind::RateLimited, now).unwrap();
        assert_eq!(
            (c_rate - now).whole_seconds(),
            DEFAULT_RATE_LIMIT_COOLDOWN_SECS
        );
        assert!(derive_cooldown(ResultKind::Success, now).is_none());
        assert!(derive_cooldown(ResultKind::AuthExpired, now).is_none());
    }

    #[tokio::test]
    async fn injects_mobile_context_into_prompt() {
        let (dir, state) = fresh_state().await;
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        let claude_profile = home.join(".agent-bus/auth/claude/john");
        let proj_dir = claude_profile.join("projects/-tmp-rp");
        std::fs::create_dir_all(&proj_dir).unwrap();
        let jsonl_path = proj_dir.join("mobile-123.jsonl");
        std::fs::write(
            &jsonl_path,
            r#"{"type":"user","message":{"role":"user","content":"hello from mobile"}}"#,
        )
        .unwrap();

        let cfg_yaml = format!(
            r#"
version: 1
agents:
  claude:
    contexts:
      - id: john
        profile_dir: {home}/.agent-bus/auth/claude/john
  codex:
    contexts:
      - id: john
        profile_dir: {home}/.agent-bus/auth/codex/john
"#,
            home = home.display()
        );
        let cfg = AuthContextsConfig::parse(&cfg_yaml, &home).unwrap();

        state
            .set_active_auth_context("claude", "john")
            .await
            .unwrap();
        state
            .set_active_auth_context("codex", "john")
            .await
            .unwrap();

        let spawner = FakeSpawner::new(vec![ok_out("codex reply")]);
        let runner = AgentRunner::new(spawner.clone(), cfg, state, events_log(dir.path()));

        let mut r = req("codex");
        r.mode = AgentRunMode::WithMobileContext {
            mobile_uuid: "mobile-123".into(),
        };

        let resp = runner.run(r).await.unwrap();
        assert!(resp.mobile_ctx_injected);

        let calls = spawner.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.contains("<mobile_session_context>"));
        assert!(calls[0].1.contains("hello from mobile"));
        assert!(calls[0].1.contains("<user_prompt>\nhi\n</user_prompt>"));
    }
}
