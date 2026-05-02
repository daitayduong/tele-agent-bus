use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_bus_core::approval_gate::ApprovalGate;
use agent_bus_core::state::{BridgedSessionState, PendingPerm, PendingPermStatus, StateHandle};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, Command};

use super::telegram::{
    pending_perm_status_text, send_perm_prompt, BotClient, RepoEntry, TelegramConfig,
};

const APP_SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const APPROVAL_POLL_INTERVAL: Duration = Duration::from_millis(250);
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run_codex_turn_via_app_server<B: BotClient + ?Sized>(
    bot: &B,
    config: &TelegramConfig,
    state: StateHandle,
    repo: &RepoEntry,
    bridge: &BridgedSessionState,
    prompt: &str,
    timeout: Duration,
    chat_id: i64,
) -> Result<String, String> {
    let mut client = CodexAppServerClient::spawn(repo.path.clone()).await?;
    client.initialize().await?;
    client
        .resume_thread(&bridge.desktop_session_id, &repo.path)
        .await?;
    let turn_id = client
        .start_turn(&bridge.desktop_session_id, &repo.path, prompt)
        .await?;

    tokio::time::timeout(
        timeout,
        client.collect_turn_output(
            bot,
            config,
            state,
            repo,
            chat_id,
            &bridge.desktop_session_id,
            &turn_id,
        ),
    )
    .await
    .map_err(|_| "Codex App Server timed out waiting for the turn to complete.".to_string())?
}

struct CodexAppServerClient {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
    backlog: Vec<Value>,
}

impl CodexAppServerClient {
    async fn spawn(repo_path: String) -> Result<Self, String> {
        let mut child = Command::new("codex")
            .args([
                "app-server",
                "--listen",
                "stdio://",
                "-c",
                "sandbox=false",
            ])
            .current_dir(repo_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("failed to spawn `codex app-server`: {err}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open codex app-server stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to open codex app-server stdout".to_string())?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "agent_bus::codex_app_server", stderr = %line);
                }
            });
        }

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            next_id: 1,
            backlog: Vec::new(),
        })
    }

    async fn initialize(&mut self) -> Result<(), String> {
        let _ = self
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "agent-bus",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            )
            .await?;
        Ok(())
    }

    async fn resume_thread(&mut self, thread_id: &str, repo_path: &str) -> Result<(), String> {
        let _ = self
            .request(
                "thread/resume",
                json!({
                    "threadId": thread_id,
                    "cwd": repo_path,
                    "approvalPolicy": "untrusted",
                }),
            )
            .await?;
        Ok(())
    }

    async fn start_turn(
        &mut self,
        thread_id: &str,
        repo_path: &str,
        prompt: &str,
    ) -> Result<String, String> {
        let response = self
            .request(
                "turn/start",
                json!({
                    "threadId": thread_id,
                    "cwd": repo_path,
                    "approvalPolicy": "untrusted",
                    "input": [{
                        "type": "text",
                        "text": prompt,
                        "text_elements": [],
                    }],
                }),
            )
            .await?;
        response
            .get("result")
            .and_then(|result| result.get("turn"))
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| "Codex App Server turn/start response missing turn id".to_string())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let request_id = self.next_request_id();
        self.write_message(json!({
            "id": request_id,
            "method": method,
            "params": params,
        }))
        .await?;

        tokio::time::timeout(APP_SERVER_REQUEST_TIMEOUT, async {
            loop {
                let Some(message) = self.next_message().await? else {
                    return Err(format!(
                        "Codex App Server closed stdout while waiting for `{method}`"
                    ));
                };
                if message.get("id").and_then(request_id_as_u64) != Some(request_id) {
                    // Pure notifications (no id) are dropped — they can't be a response
                    // and re-queueing them causes a backlog busy-loop that prevents the
                    // timeout from firing (e.g. configWarning arrives after initialize).
                    if message.get("id").is_some() {
                        self.backlog.push(message);
                    }
                    continue;
                }
                if let Some(error) = message.get("error") {
                    return Err(format!("Codex App Server `{method}` failed: {}", error));
                }
                return Ok(message);
            }
        })
        .await
        .map_err(|_| format!("Codex App Server `{method}` timed out"))?
    }

    async fn collect_turn_output<B: BotClient + ?Sized>(
        &mut self,
        bot: &B,
        config: &TelegramConfig,
        state: StateHandle,
        repo: &RepoEntry,
        chat_id: i64,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<String, String> {
        let mut reply = String::new();
        let mut completed_text: Option<String> = None;

        loop {
            let Some(message) = self.next_message().await? else {
                return Err(
                    "Codex App Server ended the session before the turn completed.".to_string(),
                );
            };

            if is_server_request(&message, "item/commandExecution/requestApproval") {
                self.handle_command_approval(bot, config, state.clone(), repo, chat_id, &message)
                    .await?;
                continue;
            }
            if is_server_request(&message, "item/fileChange/requestApproval")
                || is_server_request(&message, "item/permissions/requestApproval")
            {
                self.respond_to_request(&message, json!({ "decision": "accept" }))
                    .await?;
                continue;
            }

            match message.get("method").and_then(Value::as_str) {
                Some("item/agentMessage/delta")
                    if notification_turn_matches(&message, thread_id, turn_id) =>
                {
                    if let Some(delta) = message
                        .get("params")
                        .and_then(|params| params.get("delta"))
                        .and_then(Value::as_str)
                    {
                        reply.push_str(delta);
                    }
                }
                Some("item/completed")
                    if notification_turn_matches(&message, thread_id, turn_id) =>
                {
                    if let Some(text) = message
                        .get("params")
                        .and_then(|params| params.get("item"))
                        .and_then(|item| {
                            (item.get("type").and_then(Value::as_str) == Some("agentMessage"))
                                .then(|| item.get("text").and_then(Value::as_str))
                                .flatten()
                        })
                    {
                        completed_text = Some(text.to_string());
                    }
                }
                Some("turn/completed") if completed_turn_matches(&message, thread_id, turn_id) => {
                    let _ = self.child.start_kill();
                    if reply.trim().is_empty() {
                        return Ok(completed_text.unwrap_or_default());
                    }
                    return Ok(reply);
                }
                Some("error") => {
                    tracing::warn!(
                        target: "agent_bus::codex_app_server",
                        message = %message,
                        "received error notification from Codex app-server"
                    );
                    let error = message
                        .get("params")
                        .and_then(|params| {
                            params
                                .get("message")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned)
                                .or_else(|| Some(params.to_string()))
                        })
                        .unwrap_or_else(|| message.to_string());
                    let _ = self.child.start_kill();
                    return Err(format!("Codex app-server error: {error}"));
                }
                _ => {}
            }
        }
    }

    async fn handle_command_approval<B: BotClient + ?Sized>(
        &mut self,
        bot: &B,
        config: &TelegramConfig,
        state: StateHandle,
        repo: &RepoEntry,
        chat_id: i64,
        request: &Value,
    ) -> Result<(), String> {
        let params = request
            .get("params")
            .ok_or_else(|| "Codex approval request missing params".to_string())?;
        let command = params
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let gate = load_gate_for_repo(&repo.id)?;
        let Some(rule) = gate.check(command) else {
            return self
                .respond_to_request(request, json!({ "decision": "accept" }))
                .await;
        };

        let perm_id = next_perm_id();
        let command_hash = command_hash(command);
        let now = now_string();
        let pending = PendingPerm {
            id: perm_id.clone(),
            repo_id: repo.id.clone(),
            command_hash: command_hash.clone(),
            status: PendingPermStatus::Pending,
            created_at: now,
            timeout_at: now_plus_string(APPROVAL_TIMEOUT),
            message_id: None,
        };
        state
            .insert_pending(pending)
            .await
            .map_err(|err| format!("failed to persist pending approval: {err}"))?;

        let message = send_perm_prompt(
            bot,
            config,
            &repo.id,
            &perm_id,
            &command_hash,
            &rule.pattern,
        )
        .await
        .map_err(|err| format!("failed to send Telegram approval prompt: {err}"))?;
        if let Some(message) = message {
            let mut snapshot = state.snapshot().await;
            if let Some(perm) = snapshot.pending_perms.get_mut(&perm_id) {
                perm.status = PendingPermStatus::Sent;
                perm.message_id = Some(message.message_id);
                state
                    .insert_pending(perm.clone())
                    .await
                    .map_err(|err| format!("failed to update pending approval: {err}"))?;
            }
        }

        let decision =
            wait_for_telegram_decision(state.clone(), &perm_id, rule.destructive).await?;
        let response = match decision {
            PendingPermStatus::ApprovedByTelegram => json!({ "decision": "accept" }),
            PendingPermStatus::DeniedByTelegram => json!({ "decision": "decline" }),
            PendingPermStatus::TimedOut if rule.destructive => json!({ "decision": "decline" }),
            PendingPermStatus::TimedOut => json!({ "decision": "accept" }),
            status => {
                return Err(format!(
                    "unexpected approval resolution for Codex command: {}",
                    pending_perm_status_text(status)
                ))
            }
        };

        tracing::info!(
            target: "agent_bus::codex_app_server",
            repo_id = %repo.id,
            chat_id,
            command = %command,
            decision = %response,
            "resolved Codex App Server command approval"
        );

        self.respond_to_request(request, response).await
    }

    async fn respond_to_request(&mut self, request: &Value, result: Value) -> Result<(), String> {
        let id = request
            .get("id")
            .cloned()
            .ok_or_else(|| "Codex server request missing id".to_string())?;
        self.write_message(json!({ "id": id, "result": result }))
            .await
    }

    async fn write_message(&mut self, value: Value) -> Result<(), String> {
        let line = serde_json::to_string(&value)
            .map_err(|err| format!("failed to serialize Codex App Server message: {err}"))?;
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|err| format!("failed to write to Codex App Server stdin: {err}"))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|err| format!("failed to terminate Codex App Server message: {err}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|err| format!("failed to flush Codex App Server stdin: {err}"))
    }

    async fn next_message(&mut self) -> Result<Option<Value>, String> {
        if let Some(message) = self.backlog.pop() {
            return Ok(Some(message));
        }

        while let Some(line) = self
            .stdout
            .next_line()
            .await
            .map_err(|err| format!("failed to read Codex App Server stdout: {err}"))?
        {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(trimmed).map_err(|err| {
                format!("failed to parse Codex App Server JSON message `{trimmed}`: {err}")
            })?;
            tracing::debug!(
                target: "agent_bus::codex_app_server",
                msg = %message,
                "stdout message"
            );
            return Ok(Some(message));
        }
        Ok(None)
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

fn is_server_request(message: &Value, method: &str) -> bool {
    message.get("id").is_some() && message.get("method").and_then(Value::as_str) == Some(method)
}

fn notification_turn_matches(message: &Value, thread_id: &str, turn_id: &str) -> bool {
    let params = match message.get("params") {
        Some(params) => params,
        None => return false,
    };
    params.get("threadId").and_then(Value::as_str) == Some(thread_id)
        && params.get("turnId").and_then(Value::as_str) == Some(turn_id)
}

fn completed_turn_matches(message: &Value, thread_id: &str, turn_id: &str) -> bool {
    let params = match message.get("params") {
        Some(params) => params,
        None => return false,
    };
    params.get("threadId").and_then(Value::as_str) == Some(thread_id)
        && params
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            == Some(turn_id)
}

fn request_id_as_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn load_gate_for_repo(repo_id: &str) -> Result<ApprovalGate, String> {
    let mut lines = Vec::new();
    let key_path = PathBuf::from("/etc/agent-bus/approval-gate.key");
    let global_conf = PathBuf::from("/etc/agent-bus/approval-gate.conf");
    let global_hmac = PathBuf::from("/etc/agent-bus/approval-gate.conf.hmac");
    lines.extend(load_gate_lines(&global_conf, &global_hmac, &key_path)?);

    let home = agent_bus_home()?;
    let repo_root = home.join("repos").join(repo_id);
    let repo_conf = repo_root.join("approval-gate.conf");
    let repo_hmac = repo_root.join("approval-gate.conf.hmac");
    lines.extend(load_gate_lines(&repo_conf, &repo_hmac, &key_path)?);

    let mut gate = ApprovalGate::new();
    for line in lines {
        gate.add_rule(&line)
            .map_err(|err| format!("invalid approval-gate rule: {err}"))?;
    }
    gate.compile()
        .map_err(|err| format!("failed to compile approval-gate rules: {err}"))?;
    Ok(gate)
}

fn load_gate_lines(
    conf_path: &Path,
    hmac_path: &Path,
    key_path: &Path,
) -> Result<Vec<String>, String> {
    if !conf_path.exists() {
        return Ok(Vec::new());
    }
    agent_bus_core::approval_gate_integrity::load_and_verify(conf_path, hmac_path, key_path)
        .map_err(|err| {
            format!(
                "approval-gate integrity failed for {}: {err}",
                conf_path.display()
            )
        })
}

async fn wait_for_telegram_decision(
    state: StateHandle,
    perm_id: &str,
    destructive: bool,
) -> Result<PendingPermStatus, String> {
    let deadline = tokio::time::Instant::now() + APPROVAL_TIMEOUT;
    loop {
        let snapshot = state.snapshot().await;
        let Some(status) = snapshot.pending_perms.get(perm_id).map(|perm| perm.status) else {
            return Err("pending approval disappeared".to_string());
        };
        match status {
            PendingPermStatus::ApprovedByTelegram | PendingPermStatus::DeniedByTelegram => {
                return Ok(status)
            }
            PendingPermStatus::Pending | PendingPermStatus::Sent => {}
            _ => return Ok(status),
        }

        if tokio::time::Instant::now() >= deadline {
            state
                .expire_pending(perm_id)
                .await
                .map_err(|err| format!("failed to expire pending approval: {err}"))?;
            return Ok(if destructive {
                PendingPermStatus::TimedOut
            } else {
                PendingPermStatus::TimedOut
            });
        }
        tokio::time::sleep(APPROVAL_POLL_INTERVAL).await;
    }
}

fn agent_bus_home() -> Result<PathBuf, String> {
    if let Ok(home) = std::env::var("AGENT_BUS_HOME") {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
    Ok(PathBuf::from(home).join(".agent-bus"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_turn_match_helpers_are_strict() {
        let delta = json!({
            "method": "item/agentMessage/delta",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-1",
                "delta": "hi",
            }
        });
        assert!(notification_turn_matches(&delta, "thread-1", "turn-1"));
        assert!(!notification_turn_matches(&delta, "thread-1", "turn-2"));

        let completed = json!({
            "method": "turn/completed",
            "params": {
                "threadId": "thread-1",
                "turn": { "id": "turn-1" }
            }
        });
        assert!(completed_turn_matches(&completed, "thread-1", "turn-1"));
        assert!(!completed_turn_matches(&completed, "thread-2", "turn-1"));
    }

    #[test]
    fn request_id_parser_accepts_strings_and_numbers() {
        assert_eq!(request_id_as_u64(&json!(7)), Some(7));
        assert_eq!(request_id_as_u64(&json!("8")), Some(8));
        assert_eq!(request_id_as_u64(&json!("bad")), None);
    }
}
