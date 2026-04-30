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

const ENV_WHITELIST: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM", "TMPDIR"];

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
    #[cfg_attr(not(test), allow(dead_code))]
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
                AgentRunMode::CodexResume { session_id, .. } => Ok(vec![
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
        let mode = req.mode.clone();

        Box::pin(async move {
            let bin =
                bin_opt.ok_or_else(|| format!("spawn: no binary configured for '{agent}'"))?;
            let args = args_result?;
            let mirror = materialize_resume_session(&agent, &profile_dir, &cwd, &mode)?;
            let outcome =
                run_child(&agent, &bin, &args, &profile_dir, &cwd, &prompt, timeout).await;
            if let Some(mirror) = mirror {
                mirror.copy_back()?;
            }
            outcome
        })
    }
}

struct SessionMirror {
    source_path: PathBuf,
    profile_path: PathBuf,
}

impl SessionMirror {
    fn copy_back(&self) -> Result<(), String> {
        if !self.profile_path.exists() {
            return Ok(());
        }
        copy_file_atomic(&self.profile_path, &self.source_path)
            .map_err(|e| format!("codex session copy-back failed: {e}"))
    }
}

fn materialize_resume_session(
    agent: &str,
    profile_dir: &Path,
    repo_path: &Path,
    mode: &AgentRunMode,
) -> Result<Option<SessionMirror>, String> {
    match (agent, mode) {
        (
            "codex",
            AgentRunMode::CodexResume {
                session_id,
                transcript_path: Some(source_path),
            },
        ) => {
            let source_path = source_path
                .canonicalize()
                .map_err(|e| format!("codex transcript path invalid: {e}"))?;
            let profile_path = codex_profile_session_path(profile_dir, session_id, &source_path);
            copy_file_atomic(&source_path, &profile_path)
                .map_err(|e| format!("codex session materialize failed: {e}"))?;
            Ok(Some(SessionMirror {
                source_path,
                profile_path,
            }))
        }
        ("claude", AgentRunMode::ClaudeResume { mobile_uuid }) => {
            let Some(source_path) = claude_default_session_path(repo_path, mobile_uuid) else {
                return Ok(None);
            };
            let source_path = source_path
                .canonicalize()
                .map_err(|e| format!("claude transcript path invalid: {e}"))?;
            let profile_path = claude_profile_session_path(profile_dir, repo_path, mobile_uuid);
            if source_path == profile_path {
                return Ok(None);
            }
            copy_file_atomic(&source_path, &profile_path)
                .map_err(|e| format!("claude session materialize failed: {e}"))?;
            Ok(Some(SessionMirror {
                source_path,
                profile_path,
            }))
        }
        _ => Ok(None),
    }
}

fn codex_profile_session_path(profile_dir: &Path, session_id: &str, source_path: &Path) -> PathBuf {
    let components = source_path
        .components()
        .map(|c| c.as_os_str().to_os_string())
        .collect::<Vec<_>>();
    if let Some(pos) = components.iter().position(|c| c == "sessions") {
        let mut target = profile_dir.to_path_buf();
        for component in &components[pos..] {
            target.push(component);
        }
        return target;
    }
    profile_dir
        .join("sessions")
        .join("agent-bus")
        .join(format!("rollout-{session_id}.jsonl"))
}

fn claude_default_session_path(repo_path: &Path, session_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(
        home.join(".claude")
            .join("projects")
            .join(project_hash_for_repo(repo_path))
            .join(format!("{session_id}.jsonl")),
    )
    .filter(|path| path.exists())
}

fn claude_profile_session_path(profile_dir: &Path, repo_path: &Path, session_id: &str) -> PathBuf {
    profile_dir
        .join("projects")
        .join(project_hash_for_repo(repo_path))
        .join(format!("{session_id}.jsonl"))
}

fn project_hash_for_repo(repo_path: &Path) -> String {
    repo_path.to_string_lossy().replace('/', "-")
}

fn copy_file_atomic(source: &Path, target: &Path) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension(format!("jsonl.tmp.{}", std::process::id()));
    std::fs::copy(source, &tmp)?;
    std::fs::rename(tmp, target)?;
    Ok(())
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
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-cli")
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
            repo_id: "sample_repo".to_string(),
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
            .spawn(
                &ctx("claude", profile.clone()),
                &req("claude", "hello", tmp.path().to_path_buf()),
            )
            .await
            .unwrap();

        assert_eq!(outcome.exit_code, Some(0));
        assert!(!outcome.timed_out);
        assert!(
            outcome.stdout.contains("ok: hello"),
            "stdout: {}",
            outcome.stdout
        );
        assert!(
            outcome
                .stdout
                .contains(&format!("[config={}]", profile.display())),
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
            .spawn(
                &ctx("claude", tmp.path().to_path_buf()),
                &req("claude", "", tmp.path().to_path_buf()),
            )
            .await
            .unwrap();

        assert_eq!(outcome.exit_code, Some(1));
        assert!(
            outcome.stderr.contains("usage limit"),
            "stderr: {}",
            outcome.stderr
        );
    }

    #[tokio::test]
    async fn spawn_codex_ok_via_fixture() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = tmp.path().to_path_buf();
        let bin = fixture_dir().join("codex_ok.sh");
        let spawner = CliSpawner::new().with_bin("codex", bin);

        let outcome = spawner
            .spawn(
                &ctx("codex", profile.clone()),
                &req("codex", "hi", tmp.path().to_path_buf()),
            )
            .await
            .unwrap();

        assert_eq!(outcome.exit_code, Some(0));
        assert!(outcome.stdout.contains("codex-ok: hi"));
        assert!(outcome
            .stdout
            .contains(&format!("[config={}]", profile.display())));
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
            transcript_path: None,
        };
        let outcome = spawner.spawn(&ctx("codex", profile), &r).await.unwrap();

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
    async fn spawn_codex_resume_materializes_transcript_into_context_home_and_copies_back() {
        let tmp = tempfile::TempDir::new().unwrap();
        let source = tmp
            .path()
            .join(".codex/sessions/2026/04/20/rollout-codex-session-123.jsonl");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(
            &source,
            r#"{"type":"session_meta","payload":{"id":"codex-session-123","cwd":"/repo"}}"#,
        )
        .unwrap();

        let bin = tmp.path().join("codex_append.sh");
        std::fs::write(
            &bin,
            r#"#!/usr/bin/env bash
set -euo pipefail
cat >/dev/null
target="$CODEX_HOME/sessions/2026/04/20/rollout-codex-session-123.jsonl"
test -f "$target"
printf '\n{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"from profile"}]}}\n' >> "$target"
echo codex-ok
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let profile = tmp.path().join("profile");
        let spawner = CliSpawner::new().with_bin("codex", bin);
        let mut r = req("codex", "hi again", tmp.path().to_path_buf());
        r.mode = AgentRunMode::CodexResume {
            session_id: "codex-session-123".to_string(),
            transcript_path: Some(source.clone()),
        };

        let outcome = spawner
            .spawn(&ctx("codex", profile.clone()), &r)
            .await
            .unwrap();

        assert_eq!(outcome.exit_code, Some(0));
        let profile_copy = profile.join("sessions/2026/04/20/rollout-codex-session-123.jsonl");
        assert!(profile_copy.exists());
        let copied_back = std::fs::read_to_string(&source).unwrap();
        assert!(copied_back.contains("from profile"));
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
    async fn spawn_claude_resume_materializes_desktop_transcript_into_auth_profile_and_copies_back()
    {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let repo = home.join("Projects/SampleRepo");
        let profile = home.join(".agent-bus/auth/claude/john");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&profile).unwrap();
        std::env::set_var("HOME", &home);

        let session_id = "783f5777-38f9-4152-8692-17b175c44aff";
        let project_hash = project_hash_for_repo(&repo);
        let desktop = home
            .join(".claude/projects")
            .join(&project_hash)
            .join(format!("{session_id}.jsonl"));
        std::fs::create_dir_all(desktop.parent().unwrap()).unwrap();
        std::fs::write(&desktop, "{\"type\":\"user\",\"message\":\"desktop\"}\n").unwrap();

        let bin = tmp.path().join("claude_materialize.sh");
        std::fs::write(
            &bin,
            format!(
                r#"#!/usr/bin/env bash
set -euo pipefail
cat >/dev/null
target="$CLAUDE_CONFIG_DIR/projects/{project_hash}/{session_id}.jsonl"
test -f "$target"
printf '{{"type":"assistant","message":"from profile"}}\n' >> "$target"
echo claude-ok
"#
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let spawner = CliSpawner::new().with_bin("claude", bin);
        let mut r = req("claude", "msg", repo.clone());
        r.mode = AgentRunMode::ClaudeResume {
            mobile_uuid: session_id.to_string(),
        };

        let outcome = spawner
            .spawn(&ctx("claude", profile.clone()), &r)
            .await
            .unwrap();

        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(
            std::fs::read_to_string(
                profile
                    .join("projects")
                    .join(&project_hash)
                    .join(format!("{session_id}.jsonl"))
            )
            .unwrap()
            .matches("from profile")
            .count(),
            1
        );
        assert!(std::fs::read_to_string(&desktop)
            .unwrap()
            .contains("from profile"));

        std::env::remove_var("HOME");
    }

    #[tokio::test]
    async fn spawn_env_is_isolated() {
        // Parent has SECRET_TOKEN; child must NOT see it because env_clear()
        // is applied and SECRET_TOKEN is not in the whitelist.
        std::env::set_var("SECRET_TOKEN", "sk-should-not-leak");
        let tmp = tempfile::TempDir::new().unwrap();
        let script_dir = tempfile::TempDir::new().unwrap();

        // Inline script that echoes SECRET_TOKEN if present.
        let script = script_dir.path().join("echo_secret.sh");
        std::fs::write(
            &script,
            "#!/usr/bin/env bash\ncat -\necho \"leak=${SECRET_TOKEN:-none}\"\nexit 0\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::File::open(&script).unwrap().sync_all().unwrap();

        let spawner = CliSpawner::new().with_bin("claude", script);
        let outcome = spawner
            .spawn(
                &ctx("claude", tmp.path().to_path_buf()),
                &req("claude", "x", tmp.path().to_path_buf()),
            )
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
        std::fs::write(&script, "#!/usr/bin/env bash\ncat - >/dev/null\nsleep 10\n").unwrap();
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
            .spawn(
                &ctx("gemini", tmp.path().to_path_buf()),
                &req("gemini", "hi", tmp.path().to_path_buf()),
            )
            .await
            .unwrap_err();
        assert!(err.contains("gemini"), "err: {err}");
    }
}
