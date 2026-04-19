#![deny(clippy::await_holding_lock)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PendingPermStatus {
    Pending,
    Sent,
    Approved,
    Denied,
    TimedOut,
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
    ExpirePending {
        id: String,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    SetMobileSession {
        chat_id: String,
        mobile: MobileSessionState,
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
                StateCmd::ExpirePending { id, reply } => {
                    if let Some(perm) = state.pending_perms.get_mut(&id) {
                        perm.status = PendingPermStatus::TimedOut;
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
            repo_id: "rallyup_a1b2c3d4".to_string(),
            command_hash: "sha256:abc".to_string(),
            status: PendingPermStatus::Pending,
            created_at: "2026-04-16T00:00:00Z".to_string(),
            timeout_at: "2026-04-16T00:00:10Z".to_string(),
            message_id: None,
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
    async fn pending_rotation_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let handle = spawn_state_actor(path.clone()).await.unwrap();

        let rot = PendingRotation {
            id: "rot_01".to_string(),
            agent: "claude".to_string(),
            from: "john".to_string(),
            to: "partner".to_string(),
            repo_id: "rallyup".to_string(),
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
        handle
            .resolve_pending("perm-1", PendingPermStatus::Approved)
            .await
            .unwrap();
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
}
