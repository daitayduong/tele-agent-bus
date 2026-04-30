use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixStream;

const INITIALIZING_CLIENT: &str = "initializing-client";
const METHOD_INITIALIZE: &str = "initialize";
const METHOD_THREAD_FOLLOWER_START_TURN: &str = "thread-follower-start-turn";
const VERSION_THREAD_FOLLOWER_START_TURN: u64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum CodexIpcError {
    #[error("codex ipc socket unavailable: {0}")]
    Unavailable(String),
    #[error("codex ipc protocol error: {0}")]
    Protocol(String),
    #[error("codex desktop session has no live owner")]
    NoLiveOwner,
    #[error("codex ipc request timed out")]
    Timeout,
    #[error("codex ipc request failed: {0}")]
    RequestFailed(String),
}

#[derive(Debug, Clone)]
pub struct CodexIpcClient {
    socket_path: PathBuf,
}

impl CodexIpcClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    pub fn default_socket_path() -> PathBuf {
        #[cfg(unix)]
        {
            let uid = nix::unistd::Uid::current().as_raw();
            std::env::temp_dir()
                .join("codex-ipc")
                .join(format!("ipc-{uid}.sock"))
        }
        #[cfg(not(unix))]
        {
            std::env::temp_dir()
                .join("codex-ipc")
                .join("unsupported.sock")
        }
    }

    pub fn default() -> Self {
        Self::new(Self::default_socket_path())
    }

    pub async fn start_thread_follower_turn(
        &self,
        conversation_id: &str,
        prompt: &str,
        timeout: Duration,
    ) -> Result<(), CodexIpcError> {
        #[cfg(not(unix))]
        {
            let _ = (&self.socket_path, conversation_id, prompt, timeout);
            return Err(CodexIpcError::Unavailable(
                "Codex desktop live IPC is currently supported only on Unix platforms".to_string(),
            ));
        }
        #[cfg(unix)]
        {
            let run = async {
                let mut stream = UnixStream::connect(&self.socket_path)
                    .await
                    .map_err(|err| CodexIpcError::Unavailable(err.to_string()))?;
                let client_id = initialize_client(&mut stream).await?;
                let request_id = request_id();
                let request = json!({
                    "type": "request",
                    "requestId": request_id,
                    "sourceClientId": client_id,
                    "version": VERSION_THREAD_FOLLOWER_START_TURN,
                    "method": METHOD_THREAD_FOLLOWER_START_TURN,
                    "params": {
                        "conversationId": conversation_id,
                        "turnStartParams": {
                            "input": [{
                                "type": "text",
                                "text": prompt,
                                "text_elements": []
                            }],
                            "cwd": null,
                            "approvalPolicy": null,
                            "sandboxPolicy": null,
                            "model": null,
                            "effort": null,
                            "summary": "auto",
                            "personality": null,
                            "outputSchema": null,
                            "collaborationMode": null,
                            "attachments": []
                        }
                    }
                });
                write_frame(&mut stream, &request).await?;
                wait_for_response(&mut stream, &request_id)
                    .await
                    .map(|_| ())
            };

            match tokio::time::timeout(timeout, run).await {
                Ok(result) => result,
                Err(_) => Err(CodexIpcError::Timeout),
            }
        }
    }
}

#[cfg(unix)]
async fn initialize_client(stream: &mut UnixStream) -> Result<String, CodexIpcError> {
    let request_id = request_id();
    let request = json!({
        "type": "request",
        "requestId": request_id,
        "sourceClientId": INITIALIZING_CLIENT,
        "version": 0,
        "method": METHOD_INITIALIZE,
        "params": {
            "clientType": "agent-bus"
        }
    });
    write_frame(stream, &request).await?;
    let response = wait_for_response(stream, &request_id).await?;
    if response.get("method").and_then(Value::as_str) != Some(METHOD_INITIALIZE) {
        return Err(CodexIpcError::Protocol(
            "initialize response did not include initialize method".to_string(),
        ));
    }
    response
        .get("result")
        .and_then(|result| result.get("clientId"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| CodexIpcError::Protocol("initialize response missing clientId".to_string()))
}

#[cfg(unix)]
async fn wait_for_response(
    stream: &mut UnixStream,
    request_id: &str,
) -> Result<Value, CodexIpcError> {
    loop {
        let message = read_frame(stream).await?;
        if message.get("type").and_then(Value::as_str) != Some("response") {
            continue;
        }
        if message.get("requestId").and_then(Value::as_str) != Some(request_id) {
            continue;
        }
        match message.get("resultType").and_then(Value::as_str) {
            Some("success") => return Ok(message),
            Some("error") => {
                let err = message
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return if err == "no-client-found" {
                    Err(CodexIpcError::NoLiveOwner)
                } else {
                    Err(CodexIpcError::RequestFailed(err.to_string()))
                };
            }
            _ => {
                return Err(CodexIpcError::Protocol(
                    "response missing resultType".to_string(),
                ))
            }
        }
    }
}

#[cfg(unix)]
async fn write_frame(stream: &mut UnixStream, value: &Value) -> Result<(), CodexIpcError> {
    let bytes =
        serde_json::to_vec(value).map_err(|err| CodexIpcError::Protocol(err.to_string()))?;
    if bytes.len() > u32::MAX as usize {
        return Err(CodexIpcError::Protocol("frame too large".to_string()));
    }
    stream
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .await
        .map_err(|err| CodexIpcError::Unavailable(err.to_string()))?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|err| CodexIpcError::Unavailable(err.to_string()))
}

#[cfg(unix)]
async fn read_frame(stream: &mut UnixStream) -> Result<Value, CodexIpcError> {
    let mut len = [0_u8; 4];
    stream
        .read_exact(&mut len)
        .await
        .map_err(|err| CodexIpcError::Unavailable(err.to_string()))?;
    let len = u32::from_le_bytes(len) as usize;
    let mut bytes = vec![0_u8; len];
    stream
        .read_exact(&mut bytes)
        .await
        .map_err(|err| CodexIpcError::Unavailable(err.to_string()))?;
    serde_json::from_slice(&bytes).map_err(|err| CodexIpcError::Protocol(err.to_string()))
}

#[cfg(unix)]
fn request_id() -> String {
    format!("agent-bus-{}", rand::random::<u64>())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn start_turn_ignores_broadcasts_until_matching_response() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("codex.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let init = read_frame(&mut stream).await.unwrap();
            assert_eq!(
                init.get("method").and_then(Value::as_str),
                Some(METHOD_INITIALIZE)
            );
            let init_id = init
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap()
                .to_string();
            write_frame(
                &mut stream,
                &json!({
                    "type": "response",
                    "requestId": init_id,
                    "resultType": "success",
                    "method": "initialize",
                    "result": { "clientId": "client-1" }
                }),
            )
            .await
            .unwrap();

            let req = read_frame(&mut stream).await.unwrap();
            assert_eq!(
                req.get("method").and_then(Value::as_str),
                Some(METHOD_THREAD_FOLLOWER_START_TURN)
            );
            assert_eq!(req.get("version").and_then(Value::as_u64), Some(1));
            assert_eq!(
                req.pointer("/params/conversationId")
                    .and_then(Value::as_str),
                Some("conv-1")
            );
            assert_eq!(
                req.pointer("/params/turnStartParams/input/0/text")
                    .and_then(Value::as_str),
                Some("hello from tg")
            );
            let req_id = req
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap()
                .to_string();
            write_frame(
                &mut stream,
                &json!({
                    "type": "broadcast",
                    "method": "thread-stream-state-changed",
                    "params": { "ignored": true }
                }),
            )
            .await
            .unwrap();
            write_frame(
                &mut stream,
                &json!({
                    "type": "response",
                    "requestId": req_id,
                    "resultType": "success",
                    "method": "thread-follower-start-turn",
                    "result": { "ok": true }
                }),
            )
            .await
            .unwrap();
        });

        CodexIpcClient::new(socket)
            .start_thread_follower_turn("conv-1", "hello from tg", Duration::from_secs(2))
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn start_turn_maps_no_client_found_to_no_live_owner() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("codex.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let init = read_frame(&mut stream).await.unwrap();
            let init_id = init.get("requestId").and_then(Value::as_str).unwrap();
            write_frame(
                &mut stream,
                &json!({
                    "type": "response",
                    "requestId": init_id,
                    "resultType": "success",
                    "method": "initialize",
                    "result": { "clientId": "client-1" }
                }),
            )
            .await
            .unwrap();
            let req = read_frame(&mut stream).await.unwrap();
            let req_id = req.get("requestId").and_then(Value::as_str).unwrap();
            write_frame(
                &mut stream,
                &json!({
                    "type": "response",
                    "requestId": req_id,
                    "resultType": "error",
                    "error": "no-client-found"
                }),
            )
            .await
            .unwrap();
        });

        let err = CodexIpcClient::new(socket)
            .start_thread_follower_turn("conv-1", "hello", Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(matches!(err, CodexIpcError::NoLiveOwner));
        server.await.unwrap();
    }
}
