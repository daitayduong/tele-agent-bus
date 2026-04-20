use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_bus_core::auth_context::AuthContextsConfig;
use agent_bus_core::classifier::ResultKind;
use agent_bus_core::state::{spawn_state_actor, AuthContextStatusKind};
use tempfile::TempDir;

use super::cli_spawner::CliSpawner;
use super::runner::{AgentRunMode, AgentRunRequest, AgentRunner, EventLog, RunnerError};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-cli")
}

fn cfg_with_real_dirs(
    home: &Path,
    agent: &str,
    ids: &[&str],
    auto_rotate: bool,
    require_approval: bool,
) -> AuthContextsConfig {
    let mut contexts_yaml = String::new();
    for id in ids {
        let dir = home.join(format!(".agent-bus/auth/{agent}/{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        contexts_yaml.push_str(&format!(
            "      - id: {id}\n        profile_dir: {}\n        auto_rotate: {auto_rotate}\n        require_owner_approval: {require_approval}\n",
            dir.display()
        ));
    }
    let yaml = format!(
        "version: 1\ndefaults:\n  auto_rotate: {auto_rotate}\n  require_owner_approval: {require_approval}\nagents:\n  {agent}:\n    contexts:\n{contexts_yaml}"
    );
    AuthContextsConfig::parse(&yaml, home).unwrap()
}

fn request(agent: &str, repo_path: PathBuf, prompt: &str) -> AgentRunRequest {
    AgentRunRequest {
        agent: agent.to_string(),
        repo_id: "rallyup".to_string(),
        repo_path,
        prompt: prompt.to_string(),
        mode: AgentRunMode::Fresh,
        preferred_context: None,
        timeout: Duration::from_secs(5),
        request_id: "req-runner-cli-test".to_string(),
        chat_id: Some(123),
    }
}

fn project_hash_for_repo(repo_path: &Path) -> String {
    repo_path.to_string_lossy().replace('/', "-")
}

async fn state_and_events(tmp: &TempDir) -> (agent_bus_core::state::StateHandle, EventLog) {
    let state = spawn_state_actor(tmp.path().join("state.json"))
        .await
        .unwrap();
    let events = EventLog::new(tmp.path().join("events.jsonl"));
    (state, events)
}

#[tokio::test]
async fn cli_quota_classified_and_state_updated() {
    let tmp = TempDir::new().unwrap();
    let cfg = cfg_with_real_dirs(tmp.path(), "claude", &["john"], false, false);
    let (state, events) = state_and_events(&tmp).await;
    let spawner = CliSpawner::new().with_bin("claude", fixture_dir().join("claude_quota.sh"));
    let runner = AgentRunner::new(spawner, cfg, state.clone(), events);

    let resp = runner
        .run(request("claude", tmp.path().to_path_buf(), "hello"))
        .await
        .unwrap();

    assert_eq!(resp.final_kind, ResultKind::QuotaExhausted);
    let snap = state.snapshot().await;
    assert_eq!(
        snap.auth_context_status["claude"]["john"].status,
        AuthContextStatusKind::QuotaExhausted
    );
    let log = std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap();
    assert!(log.contains("quota_exhausted"), "events.jsonl: {log}");
}

#[tokio::test]
async fn cli_auto_rotate_on_quota() {
    let tmp = TempDir::new().unwrap();
    let cfg = cfg_with_real_dirs(tmp.path(), "claude", &["john", "partner"], true, false);
    let (state, events) = state_and_events(&tmp).await;
    let wrapper = tmp.path().join("claude_rotate_wrapper.sh");
    std::fs::write(
        &wrapper,
        r#"#!/usr/bin/env bash
stdin_content=$(cat -)
if [[ "$CLAUDE_CONFIG_DIR" == *"/john" ]]; then
  echo "Claude usage limit reached. Try again later." >&2
  exit 1
else
  echo "[config=$CLAUDE_CONFIG_DIR]"
  echo "ok: $stdin_content"
  exit 0
fi
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

    let spawner = CliSpawner::new().with_bin("claude", wrapper);
    let runner = AgentRunner::new(spawner, cfg, state, events);

    let resp = runner
        .run(request("claude", tmp.path().to_path_buf(), "rotate me"))
        .await
        .unwrap();

    assert_eq!(resp.final_kind, ResultKind::Success);
    assert_eq!(resp.auth_context, "partner");
    assert_eq!(resp.attempts.len(), 2);
    assert_eq!(resp.attempts[0].kind, ResultKind::QuotaExhausted);
    assert_eq!(resp.attempts[1].kind, ResultKind::Success);
}

#[tokio::test]
async fn cli_all_contexts_exhausted() {
    let tmp = TempDir::new().unwrap();
    let cfg = cfg_with_real_dirs(tmp.path(), "claude", &["john", "partner"], true, false);
    let (state, events) = state_and_events(&tmp).await;
    let spawner = CliSpawner::new().with_bin("claude", fixture_dir().join("claude_quota.sh"));
    let runner = AgentRunner::new(spawner, cfg, state, events);

    let result = runner
        .run(request("claude", tmp.path().to_path_buf(), "all quota"))
        .await;

    match result {
        Ok(resp) => {
            assert_eq!(resp.final_kind, ResultKind::QuotaExhausted);
            assert_eq!(resp.auth_context, "partner");
            assert_eq!(resp.attempts.len(), 2);
            assert!(resp
                .attempts
                .iter()
                .all(|attempt| attempt.kind == ResultKind::QuotaExhausted));
        }
        Err(RunnerError::NoUsableContexts { agent }) => {
            assert_eq!(agent, "claude");
        }
        Err(err) => panic!("unexpected runner error: {err}"),
    }
}

#[tokio::test]
async fn cli_codex_auth_expired_marks_reauth() {
    let tmp = TempDir::new().unwrap();
    let cfg = cfg_with_real_dirs(tmp.path(), "codex", &["john"], false, false);
    let (state, events) = state_and_events(&tmp).await;
    let spawner = CliSpawner::new().with_bin("codex", fixture_dir().join("codex_auth_expired.sh"));
    let runner = AgentRunner::new(spawner, cfg, state.clone(), events);

    let resp = runner
        .run(request("codex", tmp.path().to_path_buf(), "auth check"))
        .await
        .unwrap();

    assert_eq!(resp.final_kind, ResultKind::AuthExpired);
    let snap = state.snapshot().await;
    assert_eq!(
        snap.auth_context_status["codex"]["john"].status,
        AuthContextStatusKind::ManualReauthRequired
    );
}

#[tokio::test]
async fn cli_success_sets_active_context() {
    let tmp = TempDir::new().unwrap();
    let cfg = cfg_with_real_dirs(tmp.path(), "claude", &["john"], false, false);
    let (state, events) = state_and_events(&tmp).await;
    let spawner = CliSpawner::new().with_bin("claude", fixture_dir().join("claude_ok.sh"));
    let runner = AgentRunner::new(spawner, cfg, state.clone(), events);

    let resp = runner
        .run(request("claude", tmp.path().to_path_buf(), "happy path"))
        .await
        .unwrap();

    assert_eq!(resp.final_kind, ResultKind::Success);
    assert!(
        resp.stdout.contains("ok: happy path"),
        "stdout: {}",
        resp.stdout
    );
    let snap = state.snapshot().await;
    assert_eq!(snap.active_auth_context["claude"], "john");
}

#[tokio::test]
async fn cli_codex_resume_uses_selected_session_id() {
    let tmp = TempDir::new().unwrap();
    let cfg = cfg_with_real_dirs(tmp.path(), "codex", &["john"], false, false);
    let (state, events) = state_and_events(&tmp).await;
    let spawner = CliSpawner::new().with_bin("codex", fixture_dir().join("codex_ok.sh"));
    let runner = AgentRunner::new(spawner, cfg, state, events);

    let mut req = request("codex", tmp.path().to_path_buf(), "continue");
    req.mode = AgentRunMode::CodexResume {
        session_id: "codex-session-abc".to_string(),
    };

    let resp = runner.run(req).await.unwrap();

    assert_eq!(resp.final_kind, ResultKind::Success);
    assert!(resp.stdout.contains(
        "[args=exec resume --skip-git-repo-check codex-session-abc -]"
    ));
    assert!(resp.stdout.contains("codex-ok: continue"));
}

#[tokio::test]
async fn cli_env_config_dir_passed_to_child() {
    let tmp = TempDir::new().unwrap();
    let cfg = cfg_with_real_dirs(tmp.path(), "claude", &["john"], false, false);
    let profile_dir = cfg.context("claude", "john").unwrap().profile_dir.clone();
    let (state, events) = state_and_events(&tmp).await;
    let spawner = CliSpawner::new().with_bin("claude", fixture_dir().join("claude_ok.sh"));
    let runner = AgentRunner::new(spawner, cfg, state, events);

    let resp = runner
        .run(request("claude", tmp.path().to_path_buf(), "env check"))
        .await
        .unwrap();

    assert_eq!(resp.final_kind, ResultKind::Success);
    assert!(
        resp.stdout
            .contains(&format!("[config={}]", profile_dir.display())),
        "stdout should include profile dir {}: {}",
        profile_dir.display(),
        resp.stdout
    );
}

#[tokio::test]
async fn cli_jsonl_tail_upgrades_unknown_to_quota() {
    let tmp = TempDir::new().unwrap();
    let cfg = cfg_with_real_dirs(tmp.path(), "claude", &["john"], false, false);
    let profile_dir = cfg.context("claude", "john").unwrap().profile_dir.clone();
    let repo_path = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_path).unwrap();
    let mobile_uuid = "00000000-0000-4000-8000-000000000123";
    let jsonl_dir = profile_dir
        .join("projects")
        .join(project_hash_for_repo(&repo_path));
    std::fs::create_dir_all(&jsonl_dir).unwrap();
    std::fs::write(
        jsonl_dir.join(format!("{mobile_uuid}.jsonl")),
        r#"{"role":"assistant","content":[{"type":"text","text":"Claude usage limit reached"}]}
"#,
    )
    .unwrap();

    let wrapper = tmp.path().join("claude_unknown.sh");
    std::fs::write(
        &wrapper,
        r#"#!/usr/bin/env bash
cat - >/dev/null
exit 1
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (state, events) = state_and_events(&tmp).await;
    let spawner = CliSpawner::new().with_bin("claude", wrapper);
    let runner = AgentRunner::new(spawner, cfg, state, events);

    let mut req = request("claude", repo_path, "jsonl quota");
    req.mode = AgentRunMode::WithMobileContext {
        mobile_uuid: mobile_uuid.to_string(),
    };
    let resp = runner.run(req).await.unwrap();

    assert_eq!(resp.final_kind, ResultKind::QuotaExhausted);
    assert_eq!(resp.attempts[0].kind, ResultKind::QuotaExhausted);
    assert!(
        resp.attempts[0]
            .classifier
            .as_deref()
            .unwrap_or_default()
            .starts_with("jsonl_tail:"),
        "attempt: {:?}",
        resp.attempts[0]
    );
    let log = std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap();
    assert!(
        log.contains(r#""classifier":"jsonl_tail:"#),
        "events.jsonl: {log}"
    );
}

#[tokio::test]
async fn cli_injects_mobile_context_e2e() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let (state, events) = state_and_events(&tmp).await;
    
    let home = tmp.path();
    let claude_dir = home.join(".agent-bus/auth/claude/john");
    let codex_dir = home.join(".agent-bus/auth/codex/john");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::create_dir_all(&codex_dir).unwrap();
    
    let yaml = format!(r#"
version: 1
agents:
  claude:
    contexts:
      - id: john
        profile_dir: {}
  codex:
    contexts:
      - id: john
        profile_dir: {}
"#, claude_dir.display(), codex_dir.display());
    let cfg = AuthContextsConfig::parse(&yaml, home).unwrap();
    state.set_active_auth_context("claude", "john").await.unwrap();
    
    let repo_path = home.join("repo");
    std::fs::create_dir_all(&repo_path).unwrap();
    let jsonl_dir = claude_dir.join("projects").join(project_hash_for_repo(&repo_path));
    std::fs::create_dir_all(&jsonl_dir).unwrap();
    std::fs::write(jsonl_dir.join("m1.jsonl"), r#"{"type":"user","message":{"content":"mobile hi"}}"#).unwrap();

    let wrapper = home.join("codex_echo.sh");
    std::fs::write(&wrapper, "#!/usr/bin/env bash\ncat -\n").unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

    let spawner = CliSpawner::new().with_bin("codex", wrapper);
    let runner = AgentRunner::new(spawner, cfg, state, events);

    let mut req = request("codex", repo_path, "original prompt");
    req.mode = AgentRunMode::WithMobileContext { mobile_uuid: "m1".to_string() };
    
    let resp = runner.run(req).await.unwrap();
    assert!(resp.mobile_ctx_injected);
    assert!(resp.stdout.contains("<mobile_session_context>"));
    assert!(resp.stdout.contains("mobile hi"));
}

#[tokio::test]
async fn cli_mobile_context_persists_across_rotation_ac_l9() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let (state, events) = state_and_events(&tmp).await;
    let home = tmp.path();
    
    let codex_john = home.join(".agent-bus/auth/codex/john");
    let codex_partner = home.join(".agent-bus/auth/codex/partner");
    let claude_john = home.join(".agent-bus/auth/claude/john");
    for d in [&codex_john, &codex_partner, &claude_john] { std::fs::create_dir_all(d).unwrap(); }

    let yaml = format!(r#"
version: 1
defaults:
  auto_rotate: true
  require_owner_approval: false
agents:
  claude:
    contexts: [{{id: john, profile_dir: {}}}]
  codex:
    contexts:
      - id: john
        profile_dir: {}
      - id: partner
        profile_dir: {}
"#, claude_john.display(), codex_john.display(), codex_partner.display());
    let cfg = AuthContextsConfig::parse(&yaml, home).unwrap();
    state.set_active_auth_context("claude", "john").await.unwrap();

    let repo_path = home.join("repo");
    std::fs::create_dir_all(&repo_path).unwrap();
    let jsonl_dir = claude_john.join("projects").join(project_hash_for_repo(&repo_path));
    std::fs::create_dir_all(&jsonl_dir).unwrap();
    std::fs::write(jsonl_dir.join("m1.jsonl"), r#"{"type":"user","message":{"content":"mobile content"}}"#).unwrap();

    let wrapper = home.join("codex_rotate.sh");
    std::fs::write(&wrapper, r#"#!/usr/bin/env bash
stdin=$(cat -)
if [[ "$CODEX_HOME" == *"/john" ]]; then
  echo "quota exceeded" >&2
  exit 1
else
  echo "ok: $stdin"
  exit 0
fi
"#).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

    let spawner = CliSpawner::new().with_bin("codex", wrapper);
    let runner = AgentRunner::new(spawner, cfg, state, events);

    let mut req = request("codex", repo_path, "original");
    req.mode = AgentRunMode::WithMobileContext { mobile_uuid: "m1".to_string() };
    
    let resp = runner.run(req).await.unwrap();
    assert_eq!(resp.auth_context, "partner");
    assert!(resp.mobile_ctx_injected);
    assert!(resp.stdout.contains("mobile content"));
    assert!(resp.stdout.contains("original"));
}

#[tokio::test]
async fn cli_mobile_context_skipped_when_disabled() {
    let tmp = TempDir::new().unwrap();
    let (state, events) = state_and_events(&tmp).await;
    let home = tmp.path();
    let codex_dir = home.join(".agent-bus/auth/codex/john");
    std::fs::create_dir_all(&codex_dir).unwrap();
    
    let yaml = format!(r#"
version: 1
mobile_context:
  enabled: false
agents:
  codex:
    contexts:
      - id: john
        profile_dir: {}
"#, codex_dir.display());
    let cfg = AuthContextsConfig::parse(&yaml, home).unwrap();

    let wrapper = home.join("codex_echo.sh");
    std::fs::write(&wrapper, "#!/usr/bin/env bash\ncat -\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

    let spawner = CliSpawner::new().with_bin("codex", wrapper);
    let runner = AgentRunner::new(spawner, cfg, state, events);

    let mut req = request("codex", home.to_path_buf(), "hi");
    req.mode = AgentRunMode::WithMobileContext { mobile_uuid: "any".to_string() };
    
    let resp = runner.run(req).await.unwrap();
    assert!(!resp.mobile_ctx_injected);
    assert_eq!(resp.stdout, "hi");
}
