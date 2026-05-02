use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use agent_bus_core::peer_uid::{current_euid, verify_peer_uid, PeerUid, StdPeerUid};
use agent_bus_core::state::StateHandle;
use agent_bus_proto::{
    PermCheckRequest, PermResolveRequest, SetDefaultRepoRequest, PROTOCOL_VERSION,
};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use super::perm::PermService;

#[derive(Clone)]
pub struct UdsServer {
    pub socket_path: PathBuf,
    pub daemon_uid: u32,
    pub state: StateHandle,
    pub perm: PermService,
}

impl UdsServer {
    pub fn new(socket_path: PathBuf, state: StateHandle, perm: PermService) -> Self {
        Self {
            socket_path,
            daemon_uid: current_euid(),
            state,
            perm,
        }
    }
}

pub async fn run_uds_server(server: UdsServer) -> anyhow::Result<()> {
    serve_uds_with_peer(server, StdPeerUid, shutdown_signal()).await
}

pub async fn serve_uds_with_peer<P, S>(
    server: UdsServer,
    peer_uid: P,
    shutdown: S,
) -> anyhow::Result<()>
where
    P: PeerUid<std::os::unix::net::UnixStream> + Clone + Send + Sync + 'static,
    S: std::future::Future<Output = ()> + Send + 'static,
{
    if let Some(parent) = server.socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if tokio::fs::try_exists(&server.socket_path).await? {
        tokio::fs::remove_file(&server.socket_path).await?;
    }

    let listener = UnixListener::bind(&server.socket_path)?;
    chmod_socket_private(&server.socket_path)?;
    let server = Arc::new(server);

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
                let server = Arc::clone(&server);
                let peer_uid = peer_uid.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_connection(stream, server, peer_uid).await {
                        tracing::warn!("UDS connection failed: {err}");
                    }
                });
            }
            () = &mut shutdown => {
                break;
            }
        }
    }

    let _ = tokio::fs::remove_file(&server.socket_path).await;
    Ok(())
}

async fn handle_connection<P>(
    stream: UnixStream,
    server: Arc<UdsServer>,
    peer_uid: P,
) -> anyhow::Result<()>
where
    P: PeerUid<std::os::unix::net::UnixStream>,
{
    let std_stream = stream.into_std()?;
    std_stream.set_nonblocking(true)?;
    if let Err(err) = verify_peer_uid(&peer_uid, &std_stream, server.daemon_uid) {
        tracing::warn!("rejecting UDS peer: {err}");
        let mut stream = UnixStream::from_std(std_stream)?;
        write_response(&mut stream, 403, b"forbidden").await?;
        return Ok(());
    }
    let mut stream = UnixStream::from_std(std_stream)?;
    let request = read_http_request(&mut stream).await?;
    let response = route_request(&server, request).await;
    match response {
        Ok((status, body)) => write_json_response(&mut stream, status, &body).await?,
        Err(err) => {
            let body = json!({"protocol_version": PROTOCOL_VERSION, "error": err.to_string()});
            write_json_response(&mut stream, 500, &body).await?;
        }
    }
    Ok(())
}

async fn route_request(
    server: &UdsServer,
    request: HttpRequest,
) -> anyhow::Result<(u16, serde_json::Value)> {
    match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/perm/check") => {
            ensure_protocol(&request.body)?;
            let req: PermCheckRequest = serde_json::from_slice(&request.body)?;
            let resp = server.perm.check(req).await?;
            Ok((200, serde_json::to_value(resp)?))
        }
        ("POST", "/perm/resolve") => {
            ensure_protocol(&request.body)?;
            let req: PermResolveRequest = serde_json::from_slice(&request.body)?;
            let resp = server
                .perm
                .resolve_external(req.perm_id, req.decision, &req.source)
                .await?;
            Ok((200, serde_json::to_value(resp)?))
        }
        ("GET", "/state") => Ok((200, serde_json::to_value(server.state.snapshot().await)?)),
        ("POST", "/state/default-repo") => {
            ensure_protocol(&request.body)?;
            let req: SetDefaultRepoRequest = serde_json::from_slice(&request.body)?;
            server
                .state
                .set_default_repo(req.chat_id, req.repo_id)
                .await?;
            Ok((
                200,
                json!({"protocol_version": PROTOCOL_VERSION, "ok": true}),
            ))
        }
        ("POST", "/inbox/post") => {
            ensure_protocol(&request.body)?;
            Ok((
                200,
                json!({"protocol_version": PROTOCOL_VERSION, "ok": true}),
            ))
        }
        _ => Ok((
            404,
            json!({"protocol_version": PROTOCOL_VERSION, "error": "not_found"}),
        )),
    }
}

fn ensure_protocol(body: &[u8]) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    struct Version {
        protocol_version: u32,
    }
    let version: Version = serde_json::from_slice(body)?;
    if version.protocol_version != PROTOCOL_VERSION {
        anyhow::bail!("upgrade_required");
    }
    Ok(())
}

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

async fn read_http_request(stream: &mut UnixStream) -> anyhow::Result<HttpRequest> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let header_end = loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            anyhow::bail!("connection closed before headers");
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 64 * 1024 {
            anyhow::bail!("request headers too large");
        }
    };
    let header_text = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = header_text.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    Ok(HttpRequest { method, path, body })
}

async fn write_json_response(
    stream: &mut UnixStream,
    status: u16,
    body: &serde_json::Value,
) -> anyhow::Result<()> {
    if status == 500
        && body.get("error").and_then(|value| value.as_str()) == Some("upgrade_required")
    {
        write_response(
            stream,
            426,
            br#"{"protocol_version":1,"error":"upgrade_required"}"#,
        )
        .await
    } else {
        write_response(stream, status, &serde_json::to_vec(body)?).await
    }
}

async fn write_response(stream: &mut UnixStream, status: u16, body: &[u8]) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        426 => "Upgrade Required",
        _ => "Internal Server Error",
    };
    let headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn chmod_socket_private(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::perm::{GateLoader, MergedGateLoader, PendingPermRegistry};
    use crate::daemon::telegram::{MockBot, TelegramConfig};
    use agent_bus_core::peer_uid::MockPeerUid;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use std::time::SystemTime;

    struct EmptyLoader;
    impl GateLoader for EmptyLoader {
        fn load(&self) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
        fn modified(&self) -> anyhow::Result<Option<SystemTime>> {
            Ok(None)
        }
    }

    async fn test_server(socket: PathBuf) -> UdsServer {
        let dir = tempfile::tempdir().unwrap();
        let state = agent_bus_core::state::spawn_state_actor(dir.path().join("state.json"))
            .await
            .unwrap();
        let config = Arc::new(TelegramConfig {
            allowed_chats: vec!["123".to_string()],
            repos: vec![],
        });
        let loader = Arc::new(MergedGateLoader::new(
            Arc::new(EmptyLoader),
            Box::new(|_| Arc::new(EmptyLoader)),
        ));
        let perm = crate::daemon::perm::PermService::new(
            state.clone(),
            config,
            Arc::new(MockBot::default()),
            loader,
            PendingPermRegistry::default(),
            Duration::from_secs(30),
        );
        let mut server = UdsServer::new(socket.clone(), state, perm);
        server.daemon_uid = current_euid();
        std::mem::forget(dir);
        server
    }

    #[tokio::test]
    async fn chmods_socket_path_to_0600() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        std::fs::write(&socket, b"placeholder").unwrap();
        chmod_socket_private(&socket).unwrap();
        let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn protocol_mismatch_returns_426() {
        let dir = tempfile::tempdir().unwrap();
        let server = test_server(dir.path().join("daemon.sock")).await;
        let request = HttpRequest {
            method: "POST".to_string(),
            path: "/perm/check".to_string(),
            body: br#"{"protocol_version":99}"#.to_vec(),
        };

        let err = route_request(&server, request).await.unwrap_err();

        assert_eq!(err.to_string(), "upgrade_required");
    }

    #[test]
    fn peer_uid_mismatch_is_forbidden_before_routing() {
        let checker = MockPeerUid::new(1001);
        let err = verify_peer_uid(&checker, &(), 1000).unwrap_err();
        assert!(matches!(
            err,
            agent_bus_core::peer_uid::PeerUidError::Mismatch { .. }
        ));
    }
}
