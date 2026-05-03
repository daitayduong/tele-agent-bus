#![deny(clippy::await_holding_lock)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use crate::auth_context::AgentKind;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, RwLock};

pub const STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("state schema version mismatch: expected {expected}, got {actual}")]
    SchemaVersion { expected: u32, actual: u32 },
    #[error("state actor is closed")]
    ActorClosed,
    #[error("state io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("state json error at {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateSnapshot {
    pub schema_version: u32,
    pub default_repo_by_chat: BTreeMap<String, String>,
    pub tg_offset: Option<i64>,
    pub sessions: BTreeMap<String, SessionState>,
    pub pending_perms: BTreeMap<String, PendingPerm>,
    #[serde(default)]
    pub mobile_sessions: BTreeMap<String, MobileSessionState>,
    // Phase 4 additions (AC-Q9 backward-compat via serde defaults):
    #[serde(default)]
    pub auth_context_status: BTreeMap<String, BTreeMap<String, AuthContextStatus>>,
    #[serde(default)]
    pub active_auth_context: BTreeMap<String, String>,
    #[serde(default)]
    pub pending_rotations: BTreeMap<String, PendingRotation>,
    #[serde(default)]
    pub lead_overrides: LeadOverrides,
    // Phase 5 additions:
    #[serde(default)]
    pub bridged_sessions: BTreeMap<String, BTreeMap<String, BridgedSessionState>>,
    /// User-selected model per chat per agent. Outer key is chat_id, inner
    /// key is the agent kind (e.g. "antigravity"). Empty/missing means the
    /// agent uses its default.
    #[serde(default)]
    pub selected_model_by_chat: BTreeMap<String, BTreeMap<String, String>>,
    /// Pending Antigravity tool approval prompts that have been forwarded to
    /// Telegram and are awaiting a user click. Key is a short approval id
    /// embedded in the inline button callback data.
    #[serde(default)]
    pub pending_antigravity_approvals: BTreeMap<String, PendingAntigravityApprovalEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingAntigravityApprovalEntry {
    pub chat_id: String,
    pub repo_id: String,
    pub cascade_id: String,
    pub trajectory_id: String,
    pub step_index: i64,
    pub kind: String,
    pub summary: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgedSessionState {
    pub agent: String,
    pub repo_id: String,
    pub desktop_session_id: String,
    pub desktop_path: String,
    pub mobile_session_id: String,
    pub mobile_path: String,
    pub selected_at: String,
    pub sync: SessionSyncCursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSyncCursor {
    pub desktop_offset: u64,
    pub mobile_offset: u64,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct LeadOverrides {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub per_chat: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthContextStatusKind {
    Available,
    QuotaExhausted,
    RateLimited,
    AuthExpiringSoon,
    AuthExpired,
    ManualReauthRequired,
    Disabled,
    UnknownFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthContextStatus {
    pub status: AuthContextStatusKind,
    #[serde(default)]
    pub cooldown_until: Option<String>, // RFC3339
    #[serde(default)]
    pub last_event_id: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PendingRotationStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingRotation {
    pub id: String,
    pub agent: String,
    pub from: String,
    pub to: String,
    pub repo_id: String,
    pub request_id: String,
    pub chat_id: i64,
    pub expires_at: String,
    pub status: PendingRotationStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MobileSessionState {
    pub mobile_uuid: String,
    pub mobile_fork_source: String,
    pub mobile_forked_at: String,
    pub project_hash: String,
    pub repo_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionState {
    pub agent: String,
    pub repo_id: String,
    pub task: String,
    pub started_at: String,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PendingPermStatus {
    Pending,
    Sent,
    Approved,
    Denied,
    TimedOut,
    ApprovedByTelegram,
    DeniedByTelegram,
    ApprovedByDesktop,
    DeniedByDesktop,
    Cancelled,
    Superseded,
}

impl PendingPermStatus {
    pub fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::Sent)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvePendingOutcome {
    Resolved,
    AlreadyResolved(PendingPermStatus),
    Missing,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingPerm {
    pub id: String,
    pub repo_id: String,
    pub command_hash: String,
    pub status: PendingPermStatus,
    pub created_at: String,
    pub timeout_at: String,
    pub message_id: Option<i32>,
    #[serde(default)]
    pub prompt_text: Option<String>,
}

impl Default for StateSnapshot {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            default_repo_by_chat: BTreeMap::new(),
            tg_offset: None,
            sessions: BTreeMap::new(),
            pending_perms: BTreeMap::new(),
            mobile_sessions: BTreeMap::new(),
            auth_context_status: BTreeMap::new(),
            active_auth_context: BTreeMap::new(),
            pending_rotations: BTreeMap::new(),
            lead_overrides: LeadOverrides::default(),
            bridged_sessions: BTreeMap::new(),
            selected_model_by_chat: BTreeMap::new(),
            pending_antigravity_approvals: BTreeMap::new(),
        }
    }
}

/// Handle to the state actor.
///
/// Invariant: this actor is the only code path that writes `state.json`.
/// Callers send mutations through the channel; readers receive cloned snapshots.
#[derive(Clone, Debug)]
pub struct StateHandle {
    snapshot: Arc<RwLock<StateSnapshot>>,
    tx: mpsc::Sender<StateCmd>,
}

enum StateCmd {
    SetDefaultRepo {
        chat_id: String,
        repo_id: String,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    InsertPending {
        perm: PendingPerm,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    ResolvePending {
        id: String,
        verdict: PendingPermStatus,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    ResolvePendingIfOpen {
        id: String,
        verdict: PendingPermStatus,
        reply: oneshot::Sender<Result<ResolvePendingOutcome, StateError>>,
    },
    ExpirePending {
        id: String,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    SetMobileSession {
        chat_id: String,
        mobile: MobileSessionState,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    SetBridgedSession {
        chat_id: String,
        agent: String,
        bridge: BridgedSessionState,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    ClearMobileSession {
        chat_id: String,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    SetAuthContextStatus {
        agent: String,
        id: String,
        status: AuthContextStatus,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    SetActiveAuthContext {
        agent: String,
        id: String,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    InsertPendingRotation {
        rotation: PendingRotation,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    ResolvePendingRotation {
        id: String,
        status: PendingRotationStatus,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    SetLeadDefault {
        agent: String,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    SetLeadForChat {
        chat_id: String,
        agent: String,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    ClearLeadForChat {
        chat_id: String,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    SetSelectedModel {
        chat_id: String,
        agent: String,
        model: Option<String>,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    InsertPendingAntigravityApproval {
        approval_id: String,
        entry: PendingAntigravityApprovalEntry,
        reply: oneshot::Sender<Result<bool, StateError>>,
    },
    RemovePendingAntigravityApproval {
        approval_id: String,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
}

impl StateHandle {
    pub async fn snapshot(&self) -> StateSnapshot {
        self.snapshot.read().await.clone()
    }

    pub async fn set_default_repo(
        &self,
        chat_id: impl Into<String>,
        repo_id: impl Into<String>,
    ) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::SetDefaultRepo {
                chat_id: chat_id.into(),
                repo_id: repo_id.into(),
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn insert_pending(&self, perm: PendingPerm) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::InsertPending { perm, reply })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn resolve_pending(
        &self,
        id: impl Into<String>,
        verdict: PendingPermStatus,
    ) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::ResolvePending {
                id: id.into(),
                verdict,
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn resolve_pending_if_open(
        &self,
        id: impl Into<String>,
        verdict: PendingPermStatus,
    ) -> Result<ResolvePendingOutcome, StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::ResolvePendingIfOpen {
                id: id.into(),
                verdict,
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn expire_pending(&self, id: impl Into<String>) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::ExpirePending {
                id: id.into(),
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn set_mobile_session(
        &self,
        chat_id: impl Into<String>,
        mobile: MobileSessionState,
    ) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::SetMobileSession {
                chat_id: chat_id.into(),
                mobile,
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn clear_mobile_session(&self, chat_id: impl Into<String>) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::ClearMobileSession {
                chat_id: chat_id.into(),
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn set_bridged_session(
        &self,
        chat_id: impl Into<String>,
        agent: impl Into<String>,
        bridge: BridgedSessionState,
    ) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::SetBridgedSession {
                chat_id: chat_id.into(),
                agent: agent.into(),
                bridge,
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn set_auth_context_status(
        &self,
        agent: impl Into<String>,
        id: impl Into<String>,
        status: AuthContextStatus,
    ) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::SetAuthContextStatus {
                agent: agent.into(),
                id: id.into(),
                status,
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn set_active_auth_context(
        &self,
        agent: impl Into<String>,
        id: impl Into<String>,
    ) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::SetActiveAuthContext {
                agent: agent.into(),
                id: id.into(),
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn insert_pending_rotation(
        &self,
        rotation: PendingRotation,
    ) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::InsertPendingRotation { rotation, reply })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn resolve_pending_rotation(
        &self,
        id: impl Into<String>,
        status: PendingRotationStatus,
    ) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::ResolvePendingRotation {
                id: id.into(),
                status,
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn set_lead_default(&self, agent: impl Into<String>) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::SetLeadDefault {
                agent: agent.into(),
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn set_lead_for_chat(
        &self,
        chat_id: impl Into<String>,
        agent: impl Into<String>,
    ) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::SetLeadForChat {
                chat_id: chat_id.into(),
                agent: agent.into(),
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn clear_lead_for_chat(&self, chat_id: impl Into<String>) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::ClearLeadForChat {
                chat_id: chat_id.into(),
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    /// Set or clear the user-selected model for a (chat, agent) pair.
    /// Pass `None` to clear and fall back to the agent's default.
    pub async fn set_selected_model(
        &self,
        chat_id: impl Into<String>,
        agent: impl Into<String>,
        model: Option<String>,
    ) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::SetSelectedModel {
                chat_id: chat_id.into(),
                agent: agent.into(),
                model,
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    /// Atomically insert a pending approval entry IF the approval_id is not
    /// already present. Returns `Ok(true)` if a new entry was inserted, or
    /// `Ok(false)` if an entry under the same id already exists. The check
    /// and the insert are serialized through the state actor, so concurrent
    /// callers cannot both observe "absent" and both insert.
    pub async fn insert_pending_antigravity_approval(
        &self,
        approval_id: impl Into<String>,
        entry: PendingAntigravityApprovalEntry,
    ) -> Result<bool, StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::InsertPendingAntigravityApproval {
                approval_id: approval_id.into(),
                entry,
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn remove_pending_antigravity_approval(
        &self,
        approval_id: impl Into<String>,
    ) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::RemovePendingAntigravityApproval {
                approval_id: approval_id.into(),
                reply,
            })
            .await
            .map_err(|_| StateError::ActorClosed)?;
        rx.await.map_err(|_| StateError::ActorClosed)?
    }

    pub async fn resolve_lead(&self, chat_id: impl AsRef<str>) -> Option<AgentKind> {
        let snapshot = self.snapshot.read().await;
        snapshot
            .lead_overrides
            .per_chat
            .get(chat_id.as_ref())
            .and_then(|agent| AgentKind::from_str(agent).ok())
            .or_else(|| {
                snapshot
                    .lead_overrides
                    .default
                    .as_deref()
                    .and_then(|agent| AgentKind::from_str(agent).ok())
            })
    }
}

pub async fn spawn_state_actor(path: PathBuf) -> Result<StateHandle, StateError> {
    let state = load_state(&path)?;
    let snapshot = Arc::new(RwLock::new(state.clone()));
    let (tx, mut rx) = mpsc::channel(1000);
    let actor_snapshot = Arc::clone(&snapshot);

    tokio::spawn(async move {
        let mut state = state;
        while let Some(cmd) = rx.recv().await {
            match cmd {
                StateCmd::SetDefaultRepo {
                    chat_id,
                    repo_id,
                    reply,
                } => {
                    state.default_repo_by_chat.insert(chat_id, repo_id);
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
                StateCmd::InsertPending { perm, reply } => {
                    state.pending_perms.insert(perm.id.clone(), perm);
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
                StateCmd::ResolvePending { id, verdict, reply } => {
                    if let Some(perm) = state.pending_perms.get_mut(&id) {
                        perm.status = verdict;
                    }
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
                StateCmd::ResolvePendingIfOpen { id, verdict, reply } => {
                    let outcome = match state.pending_perms.get_mut(&id) {
                        Some(perm) if perm.status.is_open() => {
                            perm.status = verdict;
                            ResolvePendingOutcome::Resolved
                        }
                        Some(perm) => ResolvePendingOutcome::AlreadyResolved(perm.status),
                        None => ResolvePendingOutcome::Missing,
                    };
                    let result = match outcome {
                        ResolvePendingOutcome::Resolved => {
                            publish_and_flush(&actor_snapshot, &path, &state)
                                .await
                                .map(|_| outcome)
                        }
                        _ => Ok(outcome),
                    };
                    let _ = reply.send(result);
                }
                StateCmd::ExpirePending { id, reply } => {
                    if let Some(perm) = state.pending_perms.get_mut(&id) {
                        if perm.status.is_open() {
                            perm.status = PendingPermStatus::TimedOut;
                        }
                    }
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
                StateCmd::SetMobileSession {
                    chat_id,
                    mobile,
                    reply,
                } => {
                    state.mobile_sessions.insert(chat_id, mobile);
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
                StateCmd::SetBridgedSession {
                    chat_id,
                    agent,
                    bridge,
                    reply,
                } => {
                    state
                        .bridged_sessions
                        .entry(chat_id)
                        .or_default()
                        .insert(agent, bridge);
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
                StateCmd::ClearMobileSession { chat_id, reply } => {
                    state.mobile_sessions.remove(&chat_id);
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
                StateCmd::SetAuthContextStatus {
                    agent,
                    id,
                    status,
                    reply,
                } => {
                    state
                        .auth_context_status
                        .entry(agent)
                        .or_default()
                        .insert(id, status);
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
                StateCmd::SetActiveAuthContext { agent, id, reply } => {
                    state.active_auth_context.insert(agent, id);
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
                StateCmd::InsertPendingRotation { rotation, reply } => {
                    state
                        .pending_rotations
                        .insert(rotation.id.clone(), rotation);
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
                StateCmd::ResolvePendingRotation { id, status, reply } => {
                    if let Some(rot) = state.pending_rotations.get_mut(&id) {
                        rot.status = status;
                    }
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
                StateCmd::SetLeadDefault { agent, reply } => {
                    state.lead_overrides.default = Some(agent);
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
                StateCmd::SetLeadForChat {
                    chat_id,
                    agent,
                    reply,
                } => {
                    state.lead_overrides.per_chat.insert(chat_id, agent);
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
                StateCmd::ClearLeadForChat { chat_id, reply } => {
                    state.lead_overrides.per_chat.remove(&chat_id);
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
                StateCmd::SetSelectedModel {
                    chat_id,
                    agent,
                    model,
                    reply,
                } => {
                    let entry = state
                        .selected_model_by_chat
                        .entry(chat_id.clone())
                        .or_default();
                    match model {
                        Some(m) if !m.trim().is_empty() => {
                            entry.insert(agent, m);
                        }
                        _ => {
                            entry.remove(&agent);
                            if entry.is_empty() {
                                state.selected_model_by_chat.remove(&chat_id);
                            }
                        }
                    }
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
                StateCmd::InsertPendingAntigravityApproval {
                    approval_id,
                    entry,
                    reply,
                } => {
                    let was_absent = !state
                        .pending_antigravity_approvals
                        .contains_key(&approval_id);
                    if was_absent {
                        state
                            .pending_antigravity_approvals
                            .insert(approval_id, entry);
                        let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                        let _ = reply.send(result.map(|()| true));
                    } else {
                        let _ = reply.send(Ok(false));
                    }
                }
                StateCmd::RemovePendingAntigravityApproval { approval_id, reply } => {
                    state.pending_antigravity_approvals.remove(&approval_id);
                    let result = publish_and_flush(&actor_snapshot, &path, &state).await;
                    let _ = reply.send(result);
                }
            }
        }
    });

    Ok(StateHandle { snapshot, tx })
}

async fn publish_and_flush(
    snapshot: &Arc<RwLock<StateSnapshot>>,
    path: &std::path::Path,
    state: &StateSnapshot,
) -> Result<(), StateError> {
    {
        let mut guard = snapshot.write().await;
        *guard = state.clone();
    }

    atomic_write_state(path, state)
}

fn load_state(path: &std::path::Path) -> Result<StateSnapshot, StateError> {
    let tmp = tmp_path(path);
    if tmp.exists() {
        std::fs::remove_file(&tmp).map_err(|source| StateError::Io {
            path: tmp.display().to_string(),
            source,
        })?;
    }

    if !path.exists() {
        return Ok(StateSnapshot::default());
    }

    let bytes = std::fs::read(path).map_err(|source| StateError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let state: StateSnapshot =
        serde_json::from_slice(&bytes).map_err(|source| StateError::Json {
            path: path.display().to_string(),
            source,
        })?;

    if state.schema_version != STATE_SCHEMA_VERSION {
        return Err(StateError::SchemaVersion {
            expected: STATE_SCHEMA_VERSION,
            actual: state.schema_version,
        });
    }

    Ok(state)
}

fn atomic_write_state(path: &std::path::Path, state: &StateSnapshot) -> Result<(), StateError> {
    let tmp = tmp_path(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| StateError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }

    let bytes = serde_json::to_vec(state).map_err(|source| StateError::Json {
        path: path.display().to_string(),
        source,
    })?;
    {
        let mut file = std::fs::File::create(&tmp).map_err(|source| StateError::Io {
            path: tmp.display().to_string(),
            source,
        })?;
        use std::io::Write;
        file.write_all(&bytes).map_err(|source| StateError::Io {
            path: tmp.display().to_string(),
            source,
        })?;
        file.sync_all().map_err(|source| StateError::Io {
            path: tmp.display().to_string(),
            source,
        })?;
    }

    std::fs::rename(&tmp, path).map_err(|source| StateError::Io {
        path: path.display().to_string(),
        source,
    })?;

    if let Some(parent) = path.parent() {
        let dir = std::fs::File::open(parent).map_err(|source| StateError::Io {
            path: parent.display().to_string(),
            source,
        })?;
        dir.sync_all().map_err(|source| StateError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }

    Ok(())
}

fn tmp_path(path: &std::path::Path) -> PathBuf {
    path.with_extension(
        match path.extension().and_then(|extension| extension.to_str()) {
            Some(extension) => format!("{extension}.tmp"),
            None => "tmp".to_string(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(id: &str) -> PendingPerm {
        PendingPerm {
            id: id.to_string(),
            repo_id: "sample_repo_a1b2c3d4".to_string(),
            command_hash: "sha256:abc".to_string(),
            status: PendingPermStatus::Pending,
            created_at: "2026-04-16T00:00:00Z".to_string(),
            timeout_at: "2026-04-16T00:00:10Z".to_string(),
            message_id: None,
            prompt_text: None,
        }
    }

    #[tokio::test]
    async fn concurrent_writes_are_serialized_and_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let handle = spawn_state_actor(path.clone()).await.unwrap();

        let mut joins = Vec::new();
        for idx in 0..50 {
            let handle = handle.clone();
            joins.push(tokio::spawn(async move {
                handle
                    .set_default_repo(format!("chat-{idx}"), format!("repo-{idx}"))
                    .await
                    .unwrap();
            }));
        }
        for join in joins {
            join.await.unwrap();
        }

        let snapshot = handle.snapshot().await;
        assert_eq!(snapshot.default_repo_by_chat.len(), 50);

        drop(handle);
        let persisted: StateSnapshot =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(persisted.default_repo_by_chat.len(), 50);
    }

    #[tokio::test]
    async fn load_rejects_schema_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, br#"{"schema_version":999,"default_repo_by_chat":{},"tg_offset":null,"sessions":{},"pending_perms":{}}"#).unwrap();

        let err = spawn_state_actor(path).await.unwrap_err();

        assert!(matches!(err, StateError::SchemaVersion { actual: 999, .. }));
    }

    #[tokio::test]
    async fn startup_ignores_leftover_tmp_file_after_interrupted_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let original = StateSnapshot::default();
        std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
        std::fs::write(
            path.with_extension("json.tmp"),
            br#"{"schema_version":"partial""#,
        )
        .unwrap();

        let handle = spawn_state_actor(path.clone()).await.unwrap();
        let snapshot = handle.snapshot().await;

        assert_eq!(snapshot, original);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[tokio::test]
    async fn backward_compat_loads_state_without_phase4_fields() {
        // AC-Q9: old state.json without auth_context_status etc. must deserialize.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            br#"{"schema_version":1,"default_repo_by_chat":{},"tg_offset":null,"sessions":{},"pending_perms":{}}"#,
        )
        .unwrap();

        let handle = spawn_state_actor(path).await.unwrap();
        let snap = handle.snapshot().await;
        assert!(snap.auth_context_status.is_empty());
        assert!(snap.active_auth_context.is_empty());
        assert!(snap.pending_rotations.is_empty());
        assert!(snap.bridged_sessions.is_empty());
    }

    #[tokio::test]
    async fn state_with_bridged_sessions_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = StateSnapshot::default();

        let bridge = BridgedSessionState {
            agent: "claude".to_string(),
            repo_id: "sample_repo_123".to_string(),
            desktop_session_id: "desk-uuid".to_string(),
            desktop_path: "/path/to/desk.jsonl".to_string(),
            mobile_session_id: "mob-uuid".to_string(),
            mobile_path: "/path/to/mob.jsonl".to_string(),
            selected_at: "2026-04-19T00:00:00Z".to_string(),
            sync: SessionSyncCursor {
                desktop_offset: 100,
                mobile_offset: 200,
                last_synced_at: Some("2026-04-19T00:00:30Z".to_string()),
                last_error: None,
            },
            display_name: None,
        };

        let mut chat_bridges = BTreeMap::new();
        chat_bridges.insert("claude".to_string(), bridge.clone());
        state
            .bridged_sessions
            .insert("chat1".to_string(), chat_bridges);

        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();

        let handle = spawn_state_actor(path).await.unwrap();
        let snap = handle.snapshot().await;

        assert_eq!(snap.bridged_sessions["chat1"]["claude"], bridge);
    }

    #[tokio::test]
    async fn set_auth_context_status_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let handle = spawn_state_actor(path.clone()).await.unwrap();

        let status = AuthContextStatus {
            status: AuthContextStatusKind::QuotaExhausted,
            cooldown_until: Some("2026-04-18T19:00:00Z".to_string()),
            last_event_id: Some("qevt_01HV".to_string()),
            updated_at: "2026-04-18T14:10:00Z".to_string(),
        };
        handle
            .set_auth_context_status("claude", "john", status.clone())
            .await
            .unwrap();

        let snap = handle.snapshot().await;
        assert_eq!(snap.auth_context_status["claude"]["john"], status);

        drop(handle);
        let reloaded: StateSnapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            reloaded.auth_context_status["claude"]["john"].status,
            AuthContextStatusKind::QuotaExhausted
        );
    }

    #[tokio::test]
    async fn set_active_auth_context_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let handle = spawn_state_actor(path.clone()).await.unwrap();

        handle
            .set_active_auth_context("claude", "partner")
            .await
            .unwrap();

        let snap = handle.snapshot().await;
        assert_eq!(snap.active_auth_context.get("claude").unwrap(), "partner");
    }

    #[tokio::test]
    async fn lead_overrides_persist_and_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let handle = spawn_state_actor(path.clone()).await.unwrap();

        handle.set_lead_default("codex").await.unwrap();
        assert_eq!(handle.resolve_lead("456").await, Some(AgentKind::Codex));

        handle.set_lead_for_chat("123", "gemini").await.unwrap();
        assert_eq!(handle.resolve_lead("123").await, Some(AgentKind::Gemini));
        assert_eq!(handle.resolve_lead("456").await, Some(AgentKind::Codex));

        handle.clear_lead_for_chat("123").await.unwrap();
        assert_eq!(handle.resolve_lead("123").await, Some(AgentKind::Codex));

        drop(handle);
        let reloaded: StateSnapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(reloaded.lead_overrides.default.as_deref(), Some("codex"));
        assert!(!reloaded.lead_overrides.per_chat.contains_key("123"));
    }

    #[tokio::test]
    async fn selected_model_persists_and_can_be_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let handle = spawn_state_actor(path.clone()).await.unwrap();

        handle
            .set_selected_model("123", "antigravity", Some("gemini-3-flash".to_string()))
            .await
            .unwrap();

        let snap = handle.snapshot().await;
        assert_eq!(
            snap.selected_model_by_chat["123"]["antigravity"],
            "gemini-3-flash"
        );

        drop(handle);
        let reloaded: StateSnapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            reloaded.selected_model_by_chat["123"]["antigravity"],
            "gemini-3-flash"
        );

        let handle = spawn_state_actor(path.clone()).await.unwrap();
        handle
            .set_selected_model("123", "antigravity", None)
            .await
            .unwrap();
        drop(handle);

        let reloaded: StateSnapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(!reloaded.selected_model_by_chat.contains_key("123"));
    }

    #[tokio::test]
    async fn pending_rotation_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let handle = spawn_state_actor(path.clone()).await.unwrap();

        let rot = PendingRotation {
            id: "rot_01".to_string(),
            agent: "claude".to_string(),
            from: "john".to_string(),
            to: "partner".to_string(),
            repo_id: "sample_repo".to_string(),
            request_id: "req_01".to_string(),
            chat_id: 123456789,
            expires_at: "2026-04-18T14:20:00Z".to_string(),
            status: PendingRotationStatus::Pending,
        };
        handle.insert_pending_rotation(rot.clone()).await.unwrap();

        let snap = handle.snapshot().await;
        assert_eq!(
            snap.pending_rotations["rot_01"].status,
            PendingRotationStatus::Pending
        );

        handle
            .resolve_pending_rotation("rot_01", PendingRotationStatus::Approved)
            .await
            .unwrap();

        let snap = handle.snapshot().await;
        assert_eq!(
            snap.pending_rotations["rot_01"].status,
            PendingRotationStatus::Approved
        );
    }

    #[tokio::test]
    async fn pending_perm_insert_resolve_expire_persists_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let handle = spawn_state_actor(path.clone()).await.unwrap();

        handle.insert_pending(pending("perm-1")).await.unwrap();
        handle.expire_pending("perm-1").await.unwrap();

        let json = std::fs::read_to_string(path).unwrap();
        assert!(json.contains(r#""id":"perm-1""#));
        assert!(json.contains(r#""status":"timed_out""#));
        assert!(json.contains(r#""message_id":null"#));

        let reloaded: StateSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(
            reloaded
                .pending_perms
                .get("perm-1")
                .map(|perm| &perm.status),
            Some(&PendingPermStatus::TimedOut)
        );
    }

    #[tokio::test]
    async fn pending_perm_resolve_if_open_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let handle = spawn_state_actor(path).await.unwrap();

        handle.insert_pending(pending("perm-1")).await.unwrap();
        let first = handle
            .resolve_pending_if_open("perm-1", PendingPermStatus::ApprovedByDesktop)
            .await
            .unwrap();
        let second = handle
            .resolve_pending_if_open("perm-1", PendingPermStatus::DeniedByTelegram)
            .await
            .unwrap();

        assert_eq!(first, ResolvePendingOutcome::Resolved);
        assert_eq!(
            second,
            ResolvePendingOutcome::AlreadyResolved(PendingPermStatus::ApprovedByDesktop)
        );
        assert_eq!(
            handle
                .snapshot()
                .await
                .pending_perms
                .get("perm-1")
                .map(|perm| perm.status),
            Some(PendingPermStatus::ApprovedByDesktop)
        );
    }

    #[tokio::test]
    async fn pending_perm_expire_does_not_override_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let handle = spawn_state_actor(path).await.unwrap();

        handle.insert_pending(pending("perm-1")).await.unwrap();
        handle
            .resolve_pending_if_open("perm-1", PendingPermStatus::ApprovedByTelegram)
            .await
            .unwrap();
        handle.expire_pending("perm-1").await.unwrap();

        assert_eq!(
            handle
                .snapshot()
                .await
                .pending_perms
                .get("perm-1")
                .map(|perm| perm.status),
            Some(PendingPermStatus::ApprovedByTelegram)
        );
    }
}
