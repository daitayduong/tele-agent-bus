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
    Created,
    Sent,
    Approved,
    Denied,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingPerm {
    pub repo_id: String,
    pub command: String,
    pub destructive: bool,
    pub status: PendingPermStatus,
    pub telegram_message_id: Option<i64>,
    pub requested_at: String,
    pub timeout_at: String,
}

impl Default for StateSnapshot {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            default_repo_by_chat: BTreeMap::new(),
            tg_offset: None,
            sessions: BTreeMap::new(),
            pending_perms: BTreeMap::new(),
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
    UpsertPendingPerm {
        req_id: String,
        perm: PendingPerm,
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

    pub async fn upsert_pending_perm(
        &self,
        req_id: impl Into<String>,
        perm: PendingPerm,
    ) -> Result<(), StateError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StateCmd::UpsertPendingPerm {
                req_id: req_id.into(),
                perm,
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
                StateCmd::UpsertPendingPerm {
                    req_id,
                    perm,
                    reply,
                } => {
                    state.pending_perms.insert(req_id, perm);
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

    fn pending(command: &str) -> PendingPerm {
        PendingPerm {
            repo_id: "rallyup_a1b2c3d4".to_string(),
            command: command.to_string(),
            destructive: true,
            status: PendingPermStatus::Created,
            telegram_message_id: None,
            requested_at: "2026-04-16T00:00:00Z".to_string(),
            timeout_at: "2026-04-16T00:00:10Z".to_string(),
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
    async fn pending_perm_schema_includes_status_and_telegram_message_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let handle = spawn_state_actor(path.clone()).await.unwrap();

        handle
            .upsert_pending_perm("req-1", pending("git reset --hard"))
            .await
            .unwrap();

        let json = std::fs::read_to_string(path).unwrap();
        assert!(json.contains(r#""status":"created""#));
        assert!(json.contains(r#""telegram_message_id":null"#));
    }
}
