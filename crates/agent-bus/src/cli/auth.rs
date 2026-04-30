//! `agent-bus auth` subcommands.
//!
//! Manage the `~/.agent-bus/auth-contexts.yaml` file and related profile
//! directories used by the Phase-4a auth-rotation system.
//!
//! Commands are pure file operations (no daemon IPC) so they work whether
//! the daemon is running or not. The daemon reads the file on startup and
//! applies any changes on next request.
//!
//! Commands:
//!   register   Add a new context and create its profile_dir (0700).
//!   login      Spawn the provider's CLI with `CLAUDE_CONFIG_DIR` /
//!              `CODEX_HOME` pointed at the context's profile_dir.
//!   list       Print contexts (optionally filtered by agent).
//!   use        Mark a context as the persistent active one.
//!   pause      Set enabled=false.
//!   resume     Set enabled=true.
//!   recheck    Run a lightweight `<binary> --version` under the context
//!              env to confirm the profile_dir is usable.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use agent_bus_core::auth_context::{AuthContextsFile, RawContext, SUPPORTED_AGENTS};
use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;

use crate::cli::get_bus_home;

pub const AUTH_CONTEXTS_FILE: &str = "auth-contexts.yaml";
pub const AUTH_CONTEXTS_VERSION: u32 = 1;

fn id_rx() -> Regex {
    Regex::new(r"^[a-z][a-z0-9_-]{0,31}$").expect("static regex")
}

fn check_agent(agent: &str) -> Result<()> {
    if !SUPPORTED_AGENTS.contains(&agent) {
        bail!(
            "unsupported agent '{}': must be one of {}",
            agent,
            SUPPORTED_AGENTS.join(", ")
        );
    }
    Ok(())
}

fn check_id(id: &str) -> Result<()> {
    if !id_rx().is_match(id) {
        bail!("invalid id '{}': must match ^[a-z][a-z0-9_-]{{0,31}}$", id);
    }
    Ok(())
}

fn auth_contexts_path(bus: &Path) -> PathBuf {
    bus.join(AUTH_CONTEXTS_FILE)
}

fn load_or_new(path: &Path) -> Result<AuthContextsFile> {
    if !path.exists() {
        return Ok(AuthContextsFile {
            version: AUTH_CONTEXTS_VERSION,
            defaults: Default::default(),
            agents: BTreeMap::new(),
            lead: None,
            mobile_context: None,
        });
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let raw: AuthContextsFile =
        serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(raw)
}

fn save_atomic(path: &Path, file: &AuthContextsFile) -> Result<()> {
    let yaml = serde_yaml::to_string(file).context("serializing auth-contexts.yaml")?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("auth-contexts.yaml has no parent dir"))?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = path.with_extension("yaml.tmp");
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .with_context(|| format!("opening {}", tmp.display()))?;
        f.write_all(yaml.as_bytes())?;
        f.sync_all().ok();
    }
    #[cfg(unix)]
    {
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&tmp, perms).ok();
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

fn default_profile_dir(home: &Path, agent: &str, id: &str) -> PathBuf {
    home.join(".agent-bus").join("auth").join(agent).join(id)
}

fn create_profile_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        let perms = fs::Permissions::from_mode(0o700);
        fs::set_permissions(dir, perms).with_context(|| format!("chmod 0700 {}", dir.display()))?;
    }
    Ok(())
}

/// Arguments for the [`register`] command.
pub struct RegisterArgs {
    pub agent: String,
    pub id: String,
    pub label: Option<String>,
    pub require_owner_approval: Option<bool>,
}

// ── Public CLI entry points (use default bus home) ────────────────────────

pub fn register(args: RegisterArgs) -> Result<()> {
    let home = resolve_home()?;
    let bus = get_bus_home();
    register_inner(&home, &bus, args)
}

pub fn list(agent: Option<String>) -> Result<()> {
    let bus = get_bus_home();
    list_inner(&bus, agent.as_deref(), &mut std::io::stdout())
}

pub fn set_use(agent: String, id: String) -> Result<()> {
    let bus = get_bus_home();
    use_inner(&bus, &agent, &id)
}

pub fn pause(agent: String, id: String) -> Result<()> {
    let bus = get_bus_home();
    enabled_inner(&bus, &agent, &id, false)
}

pub fn resume(agent: String, id: String) -> Result<()> {
    let bus = get_bus_home();
    enabled_inner(&bus, &agent, &id, true)
}

pub fn login(agent: String, id: String) -> Result<()> {
    let home = resolve_home()?;
    let bus = get_bus_home();
    login_inner(&home, &bus, &agent, &id)
}

pub fn recheck(agent: String, id: String) -> Result<()> {
    let home = resolve_home()?;
    let bus = get_bus_home();
    recheck_inner(&home, &bus, &agent, &id)
}

fn resolve_home() -> Result<PathBuf> {
    let h = std::env::var("HOME").context("HOME env var must be set")?;
    Ok(PathBuf::from(h))
}

// ── Testable core (inject home/bus) ───────────────────────────────────────

fn register_inner(home: &Path, bus: &Path, args: RegisterArgs) -> Result<()> {
    check_agent(&args.agent)?;
    check_id(&args.id)?;

    let path = auth_contexts_path(bus);
    let mut file = load_or_new(&path)?;
    let entry = file.agents.entry(args.agent.clone()).or_default();

    if entry.contexts.iter().any(|c| c.id == args.id) {
        bail!("context {}/{} already exists", args.agent, args.id);
    }

    let dir = default_profile_dir(home, &args.agent, &args.id);
    create_profile_dir(&dir)?;

    entry.contexts.push(RawContext {
        id: args.id.clone(),
        label: args.label,
        owner: None,
        profile_dir: format!("~/.agent-bus/auth/{}/{}", args.agent, args.id),
        enabled: true,
        auto_rotate: None,
        require_owner_approval: args.require_owner_approval,
    });

    save_atomic(&path, &file)?;

    println!(
        "registered {}/{} (profile_dir: {})",
        args.agent,
        args.id,
        dir.display()
    );
    println!("next: agent-bus auth login {} {}", args.agent, args.id);
    Ok(())
}

fn list_inner(bus: &Path, agent_filter: Option<&str>, out: &mut dyn Write) -> Result<()> {
    if let Some(a) = agent_filter {
        check_agent(a)?;
    }
    let path = auth_contexts_path(bus);
    if !path.exists() {
        writeln!(out, "(no auth-contexts.yaml yet)")?;
        return Ok(());
    }
    let file = load_or_new(&path)?;
    for (agent, block) in &file.agents {
        if let Some(a) = agent_filter {
            if agent != a {
                continue;
            }
        }
        let active = block.active.as_deref();
        writeln!(out, "[{}] active={}", agent, active.unwrap_or("(none)"))?;
        if block.contexts.is_empty() {
            writeln!(out, "  (no contexts)")?;
        }
        for c in &block.contexts {
            let marker = if active == Some(c.id.as_str()) {
                "*"
            } else {
                " "
            };
            let enabled = if c.enabled { "enabled " } else { "disabled" };
            let label = c.label.as_deref().unwrap_or("");
            writeln!(out, "  {} {:12} {} {}", marker, c.id, enabled, label)?;
        }
    }
    Ok(())
}

fn use_inner(bus: &Path, agent: &str, id: &str) -> Result<()> {
    check_agent(agent)?;
    check_id(id)?;
    let path = auth_contexts_path(bus);
    let mut file = load_or_new(&path)?;
    let block = file
        .agents
        .get_mut(agent)
        .ok_or_else(|| anyhow!("no contexts registered for agent {}", agent))?;
    if !block.contexts.iter().any(|c| c.id == id) {
        bail!("context {}/{} not found", agent, id);
    }
    block.active = Some(id.to_string());
    save_atomic(&path, &file)?;
    println!("active {}={}", agent, id);
    Ok(())
}

fn enabled_inner(bus: &Path, agent: &str, id: &str, enabled: bool) -> Result<()> {
    check_agent(agent)?;
    check_id(id)?;
    let path = auth_contexts_path(bus);
    let mut file = load_or_new(&path)?;
    let block = file
        .agents
        .get_mut(agent)
        .ok_or_else(|| anyhow!("no contexts registered for agent {}", agent))?;
    let ctx = block
        .contexts
        .iter_mut()
        .find(|c| c.id == id)
        .ok_or_else(|| anyhow!("context {}/{} not found", agent, id))?;
    ctx.enabled = enabled;
    save_atomic(&path, &file)?;
    println!(
        "{} {}/{}",
        if enabled { "resumed" } else { "paused" },
        agent,
        id
    );
    Ok(())
}

// ── login + recheck: shell out with isolated env ──────────────────────────

/// Return (binary, env-var-name) for an agent.
pub fn provider_env(agent: &str) -> Result<(&'static str, &'static str)> {
    match agent {
        "claude" => Ok(("claude", "CLAUDE_CONFIG_DIR")),
        "codex" => Ok(("codex", "CODEX_HOME")),
        "gemini" => bail!("gemini auth isolation not supported in phase 4a"),
        other => bail!("unsupported agent '{}'", other),
    }
}

/// Argv dispatched to the provider binary to start its login flow.
/// Claude Code CLI uses `claude auth login`; Codex uses `codex login`.
fn login_args(agent: &str) -> &'static [&'static str] {
    match agent {
        "claude" => &["auth", "login"],
        "codex" => &["login"],
        _ => &["login"],
    }
}

fn resolve_profile_dir(home: &Path, bus: &Path, agent: &str, id: &str) -> Result<PathBuf> {
    let path = auth_contexts_path(bus);
    if !path.exists() {
        bail!(
            "no auth-contexts.yaml — run `agent-bus auth register {} {}` first",
            agent,
            id
        );
    }
    let file = load_or_new(&path)?;
    let block = file
        .agents
        .get(agent)
        .ok_or_else(|| anyhow!("no contexts registered for agent {}", agent))?;
    let ctx = block
        .contexts
        .iter()
        .find(|c| c.id == id)
        .ok_or_else(|| anyhow!("context {}/{} not found", agent, id))?;
    let expanded = if let Some(rest) = ctx.profile_dir.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(&ctx.profile_dir)
    };
    Ok(expanded)
}

fn login_inner(home: &Path, bus: &Path, agent: &str, id: &str) -> Result<()> {
    check_agent(agent)?;
    check_id(id)?;
    let (bin, env) = provider_env(agent)?;
    let dir = resolve_profile_dir(home, bus, agent, id)?;
    create_profile_dir(&dir)?;

    let bin_override = std::env::var(format!("AGENT_BUS_{}_BIN", agent.to_uppercase()))
        .unwrap_or_else(|_| bin.to_string());
    let argv = login_args(agent);

    eprintln!(
        "launching `{} {}` with {}={}",
        bin_override,
        argv.join(" "),
        env,
        dir.display()
    );

    let status = Command::new(&bin_override)
        .args(argv)
        .env(env, &dir)
        .status()
        .with_context(|| format!("spawning {}", bin_override))?;

    if !status.success() {
        bail!(
            "{} login exited with {}",
            bin_override,
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "<signal>".to_string())
        );
    }
    println!("logged in: {}/{}", agent, id);
    Ok(())
}

fn recheck_inner(home: &Path, bus: &Path, agent: &str, id: &str) -> Result<()> {
    check_agent(agent)?;
    check_id(id)?;
    let (bin, env) = provider_env(agent)?;
    let dir = resolve_profile_dir(home, bus, agent, id)?;
    if !dir.exists() {
        bail!(
            "profile_dir {} missing — run `agent-bus auth login {} {}`",
            dir.display(),
            agent,
            id
        );
    }
    let bin_override = std::env::var(format!("AGENT_BUS_{}_BIN", agent.to_uppercase()))
        .unwrap_or_else(|_| bin.to_string());

    let output = Command::new(&bin_override)
        .arg("--version")
        .env(env, &dir)
        .output()
        .with_context(|| format!("spawning {}", bin_override))?;

    if !output.status.success() {
        bail!(
            "{} --version failed (exit {}): {}",
            bin_override,
            output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "<signal>".to_string()),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let version = String::from_utf8_lossy(&output.stdout);
    println!("{}/{} OK: {}", agent, id, version.trim());
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn fake_env() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let td = tempdir().unwrap();
        let home = td.path().join("home");
        let bus = home.join(".agent-bus");
        fs::create_dir_all(&bus).unwrap();
        (td, home, bus)
    }

    fn args(agent: &str, id: &str) -> RegisterArgs {
        RegisterArgs {
            agent: agent.to_string(),
            id: id.to_string(),
            label: None,
            require_owner_approval: None,
        }
    }

    // ── register ────────────────────────────────────────────────────────

    #[test]
    fn register_creates_profile_dir_and_yaml_entry() {
        let (_td, home, bus) = fake_env();
        register_inner(&home, &bus, args("claude", "john")).unwrap();

        let yaml_path = bus.join(AUTH_CONTEXTS_FILE);
        assert!(yaml_path.exists());
        let file: AuthContextsFile =
            serde_yaml::from_str(&fs::read_to_string(&yaml_path).unwrap()).unwrap();
        assert_eq!(file.version, 1);
        let claude = file.agents.get("claude").unwrap();
        assert_eq!(claude.contexts.len(), 1);
        assert_eq!(claude.contexts[0].id, "john");
        assert!(claude.contexts[0].enabled);
        assert_eq!(
            claude.contexts[0].profile_dir,
            "~/.agent-bus/auth/claude/john"
        );

        let dir = home.join(".agent-bus/auth/claude/john");
        assert!(dir.is_dir());
    }

    #[test]
    fn register_sets_profile_dir_mode_0700() {
        let (_td, home, bus) = fake_env();
        register_inner(&home, &bus, args("claude", "john")).unwrap();
        let dir = home.join(".agent-bus/auth/claude/john");
        let meta = fs::metadata(&dir).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "profile_dir must be 0700");
    }

    #[test]
    fn register_rejects_duplicate_id() {
        let (_td, home, bus) = fake_env();
        register_inner(&home, &bus, args("claude", "john")).unwrap();
        let err = register_inner(&home, &bus, args("claude", "john")).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn register_rejects_invalid_id() {
        let (_td, home, bus) = fake_env();
        let err = register_inner(&home, &bus, args("claude", "1bad")).unwrap_err();
        assert!(err.to_string().contains("invalid id"));
    }

    #[test]
    fn register_rejects_invalid_agent() {
        let (_td, home, bus) = fake_env();
        let err = register_inner(&home, &bus, args("openai", "john")).unwrap_err();
        assert!(err.to_string().contains("unsupported agent"));
    }

    // ── list ─────────────────────────────────────────────────────────────

    #[test]
    fn list_prints_contexts_with_active_marker() {
        let (_td, home, bus) = fake_env();
        register_inner(&home, &bus, args("claude", "john")).unwrap();
        register_inner(&home, &bus, args("claude", "partner")).unwrap();
        use_inner(&bus, "claude", "partner").unwrap();

        let mut buf: Vec<u8> = Vec::new();
        list_inner(&bus, None, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();

        assert!(out.contains("[claude] active=partner"), "out: {out}");
        assert!(out.contains("* partner"), "out: {out}");
        assert!(out.contains("  john"), "out: {out}");
    }

    #[test]
    fn list_empty_reports_no_file() {
        let (_td, _home, bus) = fake_env();
        let mut buf: Vec<u8> = Vec::new();
        list_inner(&bus, None, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("no auth-contexts.yaml"));
    }

    #[test]
    fn list_filters_by_agent() {
        let (_td, home, bus) = fake_env();
        register_inner(&home, &bus, args("claude", "c1")).unwrap();
        register_inner(&home, &bus, args("codex", "x1")).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        list_inner(&bus, Some("codex"), &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("[codex]"));
        assert!(!out.contains("[claude]"));
    }

    // ── use / pause / resume ─────────────────────────────────────────────

    #[test]
    fn use_sets_active_field() {
        let (_td, home, bus) = fake_env();
        register_inner(&home, &bus, args("claude", "john")).unwrap();
        use_inner(&bus, "claude", "john").unwrap();
        let file: AuthContextsFile =
            serde_yaml::from_str(&fs::read_to_string(bus.join(AUTH_CONTEXTS_FILE)).unwrap())
                .unwrap();
        assert_eq!(
            file.agents.get("claude").unwrap().active.as_deref(),
            Some("john")
        );
    }

    #[test]
    fn use_rejects_unknown_context() {
        let (_td, home, bus) = fake_env();
        register_inner(&home, &bus, args("claude", "john")).unwrap();
        let err = use_inner(&bus, "claude", "ghost").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn pause_and_resume_toggle_enabled() {
        let (_td, home, bus) = fake_env();
        register_inner(&home, &bus, args("claude", "john")).unwrap();

        enabled_inner(&bus, "claude", "john", false).unwrap();
        let file: AuthContextsFile =
            serde_yaml::from_str(&fs::read_to_string(bus.join(AUTH_CONTEXTS_FILE)).unwrap())
                .unwrap();
        assert!(!file.agents["claude"].contexts[0].enabled);

        enabled_inner(&bus, "claude", "john", true).unwrap();
        let file: AuthContextsFile =
            serde_yaml::from_str(&fs::read_to_string(bus.join(AUTH_CONTEXTS_FILE)).unwrap())
                .unwrap();
        assert!(file.agents["claude"].contexts[0].enabled);
    }

    #[test]
    fn pause_rejects_unknown_context() {
        let (_td, home, bus) = fake_env();
        register_inner(&home, &bus, args("claude", "john")).unwrap();
        let err = enabled_inner(&bus, "claude", "ghost", false).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    // ── provider_env helpers ─────────────────────────────────────────────

    #[test]
    fn provider_env_maps_agents() {
        assert_eq!(
            provider_env("claude").unwrap(),
            ("claude", "CLAUDE_CONFIG_DIR")
        );
        assert_eq!(provider_env("codex").unwrap(), ("codex", "CODEX_HOME"));
        assert!(provider_env("gemini").is_err());
        assert!(provider_env("unknown").is_err());
    }

    #[test]
    fn resolve_profile_dir_expands_tilde() {
        let (_td, home, bus) = fake_env();
        register_inner(&home, &bus, args("claude", "john")).unwrap();
        let got = resolve_profile_dir(&home, &bus, "claude", "john").unwrap();
        assert_eq!(got, home.join(".agent-bus/auth/claude/john"));
    }

    #[test]
    fn recheck_invokes_fake_bin_with_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Fake bin: a shell script that echoes the env var and exits 0.
        let (_td, home, bus) = fake_env();
        register_inner(&home, &bus, args("claude", "john")).unwrap();

        let script = home.join("fake_claude.sh");
        fs::write(
            &script,
            "#!/bin/sh\necho fake-claude-1.0.0 CLAUDE_CONFIG_DIR=$CLAUDE_CONFIG_DIR\nexit 0\n",
        )
        .unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();

        let prev = std::env::var("AGENT_BUS_CLAUDE_BIN").ok();
        std::env::set_var("AGENT_BUS_CLAUDE_BIN", &script);
        let result = recheck_inner(&home, &bus, "claude", "john");
        if let Some(p) = prev {
            std::env::set_var("AGENT_BUS_CLAUDE_BIN", p);
        } else {
            std::env::remove_var("AGENT_BUS_CLAUDE_BIN");
        }
        result.unwrap();
    }

    // ── login arg dispatch (phase4c bugfix) ──────────────────────────────

    fn write_argv_capture_script(path: &Path, argv_file: &Path) {
        let quoted_argv = argv_file.display().to_string().replace('\'', "'\\''");
        fs::write(
            path,
            format!(
                "#!/bin/sh\nmkdir -p \"$(dirname '{0}')\"\nprintf '%s\\n' \"$@\" > '{0}'\nexit 0\n",
                quoted_argv
            ),
        )
        .unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn login_spawns_auth_login_for_claude() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Claude Code CLI dispatches auth via `claude auth login`, not `claude login`.
        let (_td, home, bus) = fake_env();
        register_inner(&home, &bus, args("claude", "john")).unwrap();

        let script = home.join("fake_claude.sh");
        let argv = home.join("claude-argv.txt");
        write_argv_capture_script(&script, &argv);

        let prev = std::env::var("AGENT_BUS_CLAUDE_BIN").ok();
        std::env::set_var("AGENT_BUS_CLAUDE_BIN", &script);
        let result = login_inner(&home, &bus, "claude", "john");
        if let Some(p) = prev {
            std::env::set_var("AGENT_BUS_CLAUDE_BIN", p);
        } else {
            std::env::remove_var("AGENT_BUS_CLAUDE_BIN");
        }
        result.unwrap();

        let captured = fs::read_to_string(&argv).unwrap();
        let argv_lines: Vec<&str> = captured.lines().collect();
        assert_eq!(
            argv_lines,
            vec!["auth", "login"],
            "claude must be invoked as `claude auth login`, got: {:?}",
            argv_lines
        );
    }

    #[test]
    fn login_spawns_login_for_codex() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (_td, home, bus) = fake_env();
        register_inner(&home, &bus, args("codex", "john")).unwrap();

        let script = home.join("fake_codex.sh");
        let argv = home.join("codex-argv.txt");
        write_argv_capture_script(&script, &argv);

        let prev = std::env::var("AGENT_BUS_CODEX_BIN").ok();
        std::env::set_var("AGENT_BUS_CODEX_BIN", &script);
        let result = login_inner(&home, &bus, "codex", "john");
        if let Some(p) = prev {
            std::env::set_var("AGENT_BUS_CODEX_BIN", p);
        } else {
            std::env::remove_var("AGENT_BUS_CODEX_BIN");
        }
        result.unwrap();

        let captured = fs::read_to_string(&argv).unwrap();
        let argv_lines: Vec<&str> = captured.lines().collect();
        assert_eq!(
            argv_lines,
            vec!["login"],
            "codex must be invoked as `codex login`, got: {:?}",
            argv_lines
        );
    }
}
