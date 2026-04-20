//! Phase 4a — Real CLI spawner (spec §5.3).
//!
//! Implements [`AgentSpawner`] for [`AgentRunner`] by launching the
//! provider CLI as a child process under a per-context environment:
//!
//! - `env_clear()` then a narrow whitelist (PATH/HOME/USER/LANG/LC_ALL/TERM/TMPDIR).
//! - Provider-specific env: `CLAUDE_CONFIG_DIR`, `CODEX_HOME`. Gemini is
//!   scope-out in 4a and returns an explicit error.
//! - Prompt is piped to stdin, then stdin is closed.
//! - `req.timeout` is enforced — on expiry the child is killed and partial
//!   stdout/stderr are returned with `timed_out=true`, `exit_code=None`.
//!
//! Never logs env var values (token leak risk).

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use agent_bus_core::auth_context::AuthContext;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::daemon::runner::{AgentRunMode, AgentRunRequest, AgentSpawner, SpawnOutcome};

const ENV_WHITELIST: &[&str] = &[
    "PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM", "TMPDIR",
];

/// Concrete `AgentSpawner` that shells out to real CLI binaries.
pub struct CliSpawner {
    bins: HashMap<String, PathBuf>,
}

impl CliSpawner {
    /// Build a spawner with binary paths resolved from `AGENT_BUS_<AGENT>_BIN`
    /// env vars, falling back to bare agent name (resolved via PATH).
    pub fn new() -> Self {
        let mut bins = HashMap::new();
        for agent in ["claude", "codex", "gemini"] {
            let env_var = format!("AGENT_BUS_{}_BIN", agent.to_uppercase());
            let bin = std::env::var(&env_var).unwrap_or_else(|_| agent.to_string());
            bins.insert(agent.to_string(), PathBuf::from(bin));
        }
        Self { bins }
    }

    /// Override the binary for a single agent. Useful in tests.
    pub fn with_bin(mut self, agent: &str, bin: PathBuf) -> Self {
        self.bins.insert(agent.to_string(), bin);
        self
    }

    fn build_args(agent: &str, mode: &AgentRunMode) -> Result<Vec<String>, String> {
        match agent {
            "claude" => {
                let mut args = vec![
                    "--print".to_string(),
                    "--output-format".to_string(),
                    "text".to_string(),
                ];
                if let AgentRunMode::ClaudeResume { mobile_uuid } = mode {
                    args.push("--resume".to_string());
                    args.push(mobile_uuid.clone());
                }
                Ok(args)
            }
            "codex" => match mode {
                AgentRunMode::CodexResume { session_id } => Ok(vec![
                    "exec".to_string(),
                    "resume".to_string(),
                    "--skip-git-repo-check".to_string(),
                    session_id.clone(),
                    "-".to_string(),
                ]),
                _ => Ok(vec![
                    "exec".to_string(),
                    "--skip-git-repo-check".to_string(),
                    "-".to_string(),
                ]),
            },
            "gemini" => Err("spawn: gemini not supported in 4a".to_string()),
            other => Err(format!("spawn: unknown agent '{other}'")),
        }
    }
}

impl Default for CliSpawner {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentSpawner for CliSpawner {
    fn spawn(
        &self,
        ctx: &AuthContext,
        req: &AgentRunRequest,
    ) -> Pin<Box<dyn Future<Output = Result<SpawnOutcome, String>> + Send + '_>> {
        let bin_opt = self.bins.get(&req.agent).cloned();
        let agent = req.agent.clone();
        let profile_dir = ctx.profile_dir.clone();
        let cwd = req.repo_path.clone();
        let prompt = req.prompt.clone();
        let timeout = req.timeout;
        let args_result = Self::build_args(&agent, &req.mode);

        Box::pin(async move {
            let bin = bin_opt
                .ok_or_else(|| format!("spawn: no binary configured for '{agent}'"))?;
            let args = args_result?;
            run_child(&agent, &bin, &args, &profile_dir, &cwd, &prompt, timeout).await
        })
    }
}

async fn run_child(
    agent: &str,
    bin: &Path,
    args: &[String],
    profile_dir: &Path,
    cwd: &Path,
    prompt: &str,
    timeout: Duration,
) -> Result<SpawnOutcome, String> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .kill_on_drop(true)
        .process_group(0) // new process group — lets us killpg() subprocesses on timeout
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for key in ENV_WHITELIST {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    match agent {
        "claude" => {
            cmd.env("CLAUDE_CONFIG_DIR", profile_dir);
        }
        "codex" => {
            cmd.env("CODEX_HOME", profile_dir);
        }
        _ => {}
    }

    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .map_err(|e| format!("stdin write failed: {e}"))?;
        // Drop closes stdin → signals EOF to child.
    }

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf).await;
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });

    let child_pid = child.id();
    let wait_result = tokio::time::timeout(timeout, child.wait()).await;

    let (exit_code, timed_out) = match wait_result {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(e)) => return Err(format!("child wait failed: {e}")),
        Err(_) => {
            // Timeout — SIGKILL the whole process group so children (e.g.
            // `sleep` spawned by a bash wrapper) die too; otherwise they
            // inherit our stdout/stderr pipes and block `read_to_end`.
            if let Some(pid) = child_pid {
                // killpg on the child's process group so any grandchildren
                // spawned by a shell wrapper are also reaped. Errors (ESRCH
                // if already dead) are ignored.
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            (None, true)
        }
    };

    // Bound pipe drainage so orphaned/buffered data cannot deadlock us.
    let stdout_bytes = tokio::time::timeout(Duration::from_secs(2), stdout_task)
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    let stderr_bytes = tokio::time::timeout(Duration::from_secs(2), stderr_task)
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();

    Ok(SpawnOutcome {
        stdout: String::from_utf8_lossy(&stdout_bytes).to_string(),
        stderr: String::from_utf8_lossy(&stderr_bytes).to_string(),
        exit_code,
        timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::runner::AgentRunMode;

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/fake-cli")
    }

    fn ctx(agent: &str, profile_dir: PathBuf) -> AuthContext {
        AuthContext {
            agent: agent.to_string(),
            id: "john".to_string(),
            label: None,
            owner: None,
            profile_dir,
            enabled: true,
            auto_rotate: false,
            require_owner_approval: false,
        }
    }

    fn req(agent: &str, prompt: &str, cwd: PathBuf) -> AgentRunRequest {
        AgentRunRequest {
            agent: agent.to_string(),
            repo_id: "rallyup".to_string(),
            repo_path: cwd,
            prompt: prompt.to_string(),
            mode: AgentRunMode::Fresh,
            preferred_context: None,
            timeout: Duration::from_secs(5),
            request_id: "req-test".to_string(),
            chat_id: None,
        }
    }

    #[tokio::test]
    async fn spawn_success_via_claude_ok_fixture() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = tmp.path().to_path_buf();
        let bin = fixture_dir().join("claude_ok.sh");
        let spawner = CliSpawner::new().with_bin("claude", bin);

        let outcome = spawner
            .spawn(&ctx("claude", profile.clone()), &req("claude", "hello", tmp.path().to_path_buf()))
            .await
            .unwrap();

        assert_eq!(outcome.exit_code, Some(0));
        assert!(!outcome.timed_out);
        assert!(outcome.stdout.contains("ok: hello"), "stdout: {}", outcome.stdout);
        assert!(
            outcome.stdout.contains(&format!("[config={}]", profile.display())),
            "stdout should echo CLAUDE_CONFIG_DIR: {}",
            outcome.stdout
        );
        assert!(outcome.stderr.is_empty());
    }

    #[tokio::test]
    async fn spawn_claude_quota_returns_stderr_and_exit_1() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin = fixture_dir().join("claude_quota.sh");
        let spawner = CliSpawner::new().with_bin("claude", bin);

        let outcome = spawner
            .spawn(&ctx("claude", tmp.path().to_path_buf()), &req("claude", "", tmp.path().to_path_buf()))
            .await
            .unwrap();

        assert_eq!(outcome.exit_code, Some(1));
        assert!(outcome.stderr.contains("usage limit"), "stderr: {}", outcome.stderr);
    }

    #[tokio::test]
    async fn spawn_codex_ok_via_fixture() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = tmp.path().to_path_buf();
        let bin = fixture_dir().join("codex_ok.sh");
        let spawner = CliSpawner::new().with_bin("codex", bin);

        let outcome = spawner
            .spawn(&ctx("codex", profile.clone()), &req("codex", "hi", tmp.path().to_path_buf()))
            .await
            .unwrap();

        assert_eq!(outcome.exit_code, Some(0));
        assert!(outcome.stdout.contains("codex-ok: hi"));
        assert!(outcome.stdout.contains(&format!("[config={}]", profile.display())));
    }

    #[tokio::test]
    async fn spawn_codex_resume_uses_exec_resume_session_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = tmp.path().to_path_buf();
        let bin = fixture_dir().join("codex_ok.sh");
        let spawner = CliSpawner::new().with_bin("codex", bin);

        let mut r = req("codex", "hi again", tmp.path().to_path_buf());
        r.mode = AgentRunMode::CodexResume {
            session_id: "codex-session-123".to_string(),
        };
        let outcome = spawner
            .spawn(&ctx("codex", profile), &r)
            .await
            .unwrap();

        assert_eq!(outcome.exit_code, Some(0));
        assert!(
            outcome
                .stdout
                .contains("[args=exec resume --skip-git-repo-check codex-session-123 -]"),
            "stdout: {}",
            outcome.stdout
        );
        assert!(outcome.stdout.contains("codex-ok: hi again"));
    }

    #[tokio::test]
    async fn spawn_resumes_with_mobile_uuid() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin = fixture_dir().join("claude_ok.sh");
        let spawner = CliSpawner::new().with_bin("claude", bin);

        let mut r = req("claude", "msg", tmp.path().to_path_buf());
        r.mode = AgentRunMode::ClaudeResume {
            mobile_uuid: "mob-uuid-xyz".to_string(),
        };
        let outcome = spawner
            .spawn(&ctx("claude", tmp.path().to_path_buf()), &r)
            .await
            .unwrap();
        assert!(
            outcome.stdout.contains("[resumed uuid=mob-uuid-xyz]"),
            "stdout: {}",
            outcome.stdout
        );
    }

    #[tokio::test]
    async fn spawn_env_is_isolated() {
        // Parent has SECRET_TOKEN; child must NOT see it because env_clear()
        // is applied and SECRET_TOKEN is not in the whitelist.
        std::env::set_var("SECRET_TOKEN", "sk-should-not-leak");
        let tmp = tempfile::TempDir::new().unwrap();

        // Inline script that echoes SECRET_TOKEN if present.
        let script = tmp.path().join("echo_secret.sh");
        std::fs::write(
            &script,
            "#!/usr/bin/env bash\ncat -\necho \"leak=${SECRET_TOKEN:-none}\"\nexit 0\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let spawner = CliSpawner::new().with_bin("claude", script);
        let outcome = spawner
            .spawn(&ctx("claude", tmp.path().to_path_buf()), &req("claude", "x", tmp.path().to_path_buf()))
            .await
            .unwrap();

        std::env::remove_var("SECRET_TOKEN");
        assert!(
            outcome.stdout.contains("leak=none"),
            "env leaked to child: {}",
            outcome.stdout
        );
    }

    #[tokio::test]
    async fn spawn_timeout_kills_process() {
        let tmp = tempfile::TempDir::new().unwrap();
        let script = tmp.path().join("hang.sh");
        std::fs::write(
            &script,
            "#!/usr/bin/env bash\ncat - >/dev/null\nsleep 10\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let spawner = CliSpawner::new().with_bin("claude", script);
        let mut r = req("claude", "", tmp.path().to_path_buf());
        r.timeout = Duration::from_millis(200);

        let start = std::time::Instant::now();
        let outcome = spawner
            .spawn(&ctx("claude", tmp.path().to_path_buf()), &r)
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert!(outcome.timed_out);
        assert_eq!(outcome.exit_code, None);
        // Must return quickly after timeout (kill propagates); allow slack.
        assert!(
            elapsed < Duration::from_secs(3),
            "did not kill on timeout ({:?})",
            elapsed
        );
    }

    #[tokio::test]
    async fn spawn_rejects_gemini_in_4a() {
        let tmp = tempfile::TempDir::new().unwrap();
        let spawner = CliSpawner::new();
        let err = spawner
            .spawn(&ctx("gemini", tmp.path().to_path_buf()), &req("gemini", "hi", tmp.path().to_path_buf()))
            .await
            .unwrap_err();
        assert!(err.contains("gemini"), "err: {err}");
    }
}
