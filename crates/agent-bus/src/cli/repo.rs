use std::fs;
use std::path::{Path, PathBuf};

use agent_bus_core::path_validate::{validate_repo_path, PathPolicy};
use agent_bus_core::repo_id::compute_repo_id;
use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::cli::get_bus_home;
use crate::daemon::telegram::CodexMode;

const DEFAULT_AGENTS: &[&str] = &["claude", "gemini", "antigravity", "codex"];

#[derive(Debug, Deserialize, Serialize)]
struct ReposFile {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    #[serde(default)]
    repos: Vec<RepoEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RepoEntry {
    id: String,
    display: String,
    path: String,
    #[serde(default)]
    agents: Vec<String>,
    #[serde(default)]
    codex_mode: CodexMode,
}

fn default_schema_version() -> u32 {
    1
}

pub fn add(path: &str) -> anyhow::Result<()> {
    add_inner(path, &home_dir()?, &get_bus_home())
}

pub fn list() -> anyhow::Result<()> {
    list_inner(&get_bus_home())
}

pub fn remove(id: &str) -> anyhow::Result<()> {
    remove_inner(id, &get_bus_home())
}

pub fn install_hook(path: &str) -> anyhow::Result<()> {
    let hook_bin = resolve_hook_bin()?;
    install_hook_inner(path, &home_dir()?, &hook_bin)?;
    Ok(())
}

fn add_inner(path: &str, home: &Path, bus_home: &Path) -> anyhow::Result<()> {
    let policy = PathPolicy::for_home(home);
    let canonical =
        validate_repo_path(path, &policy).with_context(|| format!("invalid repo path: {path}"))?;

    let display = derive_display(&canonical);
    let id = compute_repo_id(&display, &canonical)
        .with_context(|| format!("failed to compute repo id for {}", canonical.display()))?;

    let repos_path = bus_home.join("repos.yaml");
    let mut file = load_repos(&repos_path)?;

    if file.repos.iter().any(|r| r.id == id) {
        return Err(anyhow!("repo already registered: {id} ({display})"));
    }
    if let Some(existing) = file
        .repos
        .iter()
        .find(|r| Path::new(&r.path) == canonical.as_path())
    {
        return Err(anyhow!(
            "path already registered under id {}: {}",
            existing.id,
            existing.path
        ));
    }

    file.repos.push(RepoEntry {
        id: id.clone(),
        display: display.clone(),
        path: canonical.to_string_lossy().into_owned(),
        agents: DEFAULT_AGENTS.iter().map(|s| s.to_string()).collect(),
        codex_mode: CodexMode::LiveBridge,
    });

    write_repos(&repos_path, &file)?;
    println!("Added {display} ({id}) -> {}", canonical.display());
    Ok(())
}

fn list_inner(bus_home: &Path) -> anyhow::Result<()> {
    let repos_path = bus_home.join("repos.yaml");
    let file = load_repos(&repos_path)?;

    if file.repos.is_empty() {
        println!("No repos registered. Use: agent-bus repo add <path>");
        return Ok(());
    }

    let id_w = file
        .repos
        .iter()
        .map(|r| r.id.len())
        .max()
        .unwrap_or(2)
        .max(2);
    let disp_w = file
        .repos
        .iter()
        .map(|r| r.display.len())
        .max()
        .unwrap_or(7)
        .max(7);

    println!("{:<id_w$}  {:<disp_w$}  PATH", "ID", "DISPLAY");
    for r in &file.repos {
        println!("{:<id_w$}  {:<disp_w$}  {}", r.id, r.display, r.path);
    }
    Ok(())
}

fn remove_inner(id: &str, bus_home: &Path) -> anyhow::Result<()> {
    let repos_path = bus_home.join("repos.yaml");
    let mut file = load_repos(&repos_path)?;
    let before = file.repos.len();
    file.repos.retain(|r| r.id != id);
    if file.repos.len() == before {
        return Err(anyhow!("no repo with id: {id}"));
    }
    write_repos(&repos_path, &file)?;
    println!("Removed {id}");
    Ok(())
}

fn install_hook_inner(path: &str, home: &Path, hook_bin: &Path) -> anyhow::Result<()> {
    let policy = PathPolicy::for_home(home);
    let canonical =
        validate_repo_path(path, &policy).with_context(|| format!("invalid repo path: {path}"))?;
    let hook_bin = absolutize_hook_bin(hook_bin)?;

    let claude_dir = canonical.join(".claude");
    fs::create_dir_all(&claude_dir)
        .with_context(|| format!("failed to create {}", claude_dir.display()))?;
    let settings_path = claude_dir.join("settings.json");

    let mut settings = load_claude_settings(&settings_path)?;
    let changed = install_pretooluse_bash_hook(&mut settings, &hook_bin)?;
    if !changed {
        println!(
            "Claude Code hook already installed for {} -> {}",
            canonical.display(),
            hook_bin.display()
        );
        return Ok(());
    }

    let backup = backup_existing_file(&settings_path)?;
    write_json_atomic(&settings_path, &settings)?;
    match backup {
        Some(path) => println!(
            "Installed Claude Code hook for {} -> {} (backup: {})",
            canonical.display(),
            hook_bin.display(),
            path.display()
        ),
        None => println!(
            "Installed Claude Code hook for {} -> {}",
            canonical.display(),
            hook_bin.display()
        ),
    }
    Ok(())
}

fn load_repos(path: &Path) -> anyhow::Result<ReposFile> {
    if !path.exists() {
        return Ok(ReposFile {
            schema_version: 1,
            repos: Vec::new(),
        });
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let file: ReposFile = serde_yaml::from_str(&text)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(file)
}

fn load_claude_settings(path: &Path) -> anyhow::Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

fn install_pretooluse_bash_hook(settings: &mut Value, hook_bin: &Path) -> anyhow::Result<bool> {
    let hook_command = hook_bin.to_string_lossy().into_owned();
    let root = as_object_mut(settings, ".claude/settings.json root")?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks_obj = as_object_mut(hooks, "hooks")?;
    let pretool = hooks_obj
        .entry("PreToolUse")
        .or_insert_with(|| Value::Array(Vec::new()));
    let entries = pretool
        .as_array_mut()
        .ok_or_else(|| anyhow!("hooks.PreToolUse must be an array"))?;

    let bash_idx = entries
        .iter()
        .position(|entry| entry.get("matcher").and_then(Value::as_str) == Some("Bash"));
    let idx = match bash_idx {
        Some(idx) => idx,
        None => {
            entries.push(json!({"matcher": "Bash", "hooks": []}));
            entries.len() - 1
        }
    };

    let bash_entry = entries
        .get_mut(idx)
        .ok_or_else(|| anyhow!("internal error: missing Bash PreToolUse entry"))?;
    let bash_obj = as_object_mut(bash_entry, "hooks.PreToolUse Bash entry")?;
    let hook_values = bash_obj
        .entry("hooks")
        .or_insert_with(|| Value::Array(Vec::new()));
    let hook_arr = hook_values
        .as_array_mut()
        .ok_or_else(|| anyhow!("hooks.PreToolUse[].hooks must be an array"))?;

    let before = hook_arr.clone();
    hook_arr.retain(|hook| {
        let Some(command) = hook.get("command").and_then(Value::as_str) else {
            return true;
        };
        command != hook_command && command != ".agents/perm-gate.sh"
    });

    hook_arr.push(json!({
        "type": "command",
        "command": hook_command
    }));

    Ok(*hook_arr != before || bash_idx.is_none())
}

fn as_object_mut<'a>(
    value: &'a mut Value,
    label: &str,
) -> anyhow::Result<&'a mut Map<String, Value>> {
    value
        .as_object_mut()
        .ok_or_else(|| anyhow!("{label} must be a JSON object"))
}

fn backup_existing_file(path: &Path) -> anyhow::Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let backup = path.with_file_name(format!("settings.json.bak-{ts}"));
    fs::copy(path, &backup)
        .with_context(|| format!("failed to create backup {}", backup.display()))?;
    Ok(Some(backup))
}

fn write_json_atomic(path: &Path, value: &Value) -> anyhow::Result<()> {
    let mut text = serde_json::to_string_pretty(value).context("failed to serialize settings")?;
    text.push('\n');
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text.as_bytes())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    if let Ok(f) = fs::File::open(&tmp) {
        let _ = f.sync_all();
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn resolve_hook_bin() -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var("AGENT_BUS_HOOK_BIN") {
        return Ok(PathBuf::from(path));
    }

    let current = std::env::current_exe().context("failed to resolve current executable")?;
    let candidate = current.with_file_name("agent-bus-hook");
    if candidate.exists() {
        return Ok(candidate);
    }

    Ok(PathBuf::from("agent-bus-hook"))
}

fn absolutize_hook_bin(path: &Path) -> anyhow::Result<PathBuf> {
    if path.is_absolute() || path.components().count() == 1 {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .context("failed to resolve current directory")
}

fn write_repos(path: &Path, file: &ReposFile) -> anyhow::Result<()> {
    let yaml = serde_yaml::to_string(file).context("failed to serialize repos.yaml")?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).ok();
    let tmp = path.with_extension("yaml.tmp");
    fs::write(&tmp, yaml.as_bytes())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    if let Ok(f) = fs::File::open(&tmp) {
        let _ = f.sync_all();
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn derive_display(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "repo".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_appends_entry_with_computed_id() {
        let home = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let repo = home.path().join("MyApp");
        std::fs::create_dir(&repo).unwrap();

        add_inner(repo.to_str().unwrap(), home.path(), bus.path()).unwrap();

        let text = std::fs::read_to_string(bus.path().join("repos.yaml")).unwrap();
        assert!(text.contains("display: MyApp"), "yaml: {text}");
        assert!(text.contains("id: myapp_"), "yaml: {text}");
    }

    #[test]
    fn add_rejects_duplicate_path() {
        let home = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let repo = home.path().join("Proj");
        std::fs::create_dir(&repo).unwrap();

        add_inner(repo.to_str().unwrap(), home.path(), bus.path()).unwrap();
        let err = add_inner(repo.to_str().unwrap(), home.path(), bus.path()).unwrap_err();
        assert!(err.to_string().contains("already registered"));
    }

    #[test]
    fn remove_deletes_matching_id() {
        let home = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let repo = home.path().join("Alpha");
        std::fs::create_dir(&repo).unwrap();
        add_inner(repo.to_str().unwrap(), home.path(), bus.path()).unwrap();

        let text = std::fs::read_to_string(bus.path().join("repos.yaml")).unwrap();
        let parsed: ReposFile = serde_yaml::from_str(&text).unwrap();
        let id = parsed.repos[0].id.clone();

        remove_inner(&id, bus.path()).unwrap();

        let text = std::fs::read_to_string(bus.path().join("repos.yaml")).unwrap();
        let parsed: ReposFile = serde_yaml::from_str(&text).unwrap();
        assert!(parsed.repos.is_empty());
    }

    #[test]
    fn remove_errors_when_id_missing() {
        let bus = tempfile::tempdir().unwrap();
        let err = remove_inner("nonexistent_deadbeef", bus.path()).unwrap_err();
        assert!(err.to_string().contains("no repo with id"));
    }

    #[test]
    fn install_hook_creates_pretooluse_bash_entry() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("SampleRepo");
        std::fs::create_dir(&repo).unwrap();
        let hook = home.path().join("bin/agent-bus-hook");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, "").unwrap();

        install_hook_inner(repo.to_str().unwrap(), home.path(), &hook).unwrap();

        let text = std::fs::read_to_string(repo.join(".claude/settings.json")).unwrap();
        let json: Value = serde_json::from_str(&text).unwrap();
        let pretool = json["hooks"]["PreToolUse"].as_array().unwrap();
        let bash = pretool
            .iter()
            .find(|entry| entry["matcher"] == "Bash")
            .unwrap();
        let hook_command = hook.to_string_lossy().to_string();
        assert_eq!(bash["hooks"][0]["type"], "command");
        assert_eq!(bash["hooks"][0]["command"], hook_command);
    }

    #[test]
    fn install_hook_preserves_other_hooks_and_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("SampleRepo");
        let claude = repo.join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{
  "permissions": {"allow": ["Bash(git status:*)"]},
  "hooks": {
    "UserPromptSubmit": [
      {"matcher": "", "hooks": [{"type": "command", "command": "bash .claude/hooks/user-prompt-submit.sh"}]}
    ]
  }
}"#,
        )
        .unwrap();
        let hook = home.path().join("bin/agent-bus-hook");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, "").unwrap();

        install_hook_inner(repo.to_str().unwrap(), home.path(), &hook).unwrap();
        install_hook_inner(repo.to_str().unwrap(), home.path(), &hook).unwrap();

        let text = std::fs::read_to_string(claude.join("settings.json")).unwrap();
        let json: Value = serde_json::from_str(&text).unwrap();
        let hook_command = hook.to_string_lossy().to_string();
        assert_eq!(json["permissions"]["allow"][0], "Bash(git status:*)");
        assert_eq!(
            json["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            "bash .claude/hooks/user-prompt-submit.sh"
        );

        let pretool = json["hooks"]["PreToolUse"].as_array().unwrap();
        let installed = pretool
            .iter()
            .flat_map(|entry| entry["hooks"].as_array().into_iter().flatten())
            .filter(|hook_value| hook_value["command"] == hook_command)
            .count();
        assert_eq!(installed, 1);
    }

    #[test]
    fn install_hook_replaces_legacy_perm_gate_hook() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("SampleRepo");
        let claude = repo.join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{
  "hooks": {
    "PreToolUse": [
      {"matcher": "Bash", "hooks": [{"type": "command", "command": ".agents/perm-gate.sh"}]}
    ]
  }
}"#,
        )
        .unwrap();
        let hook = home.path().join("bin/agent-bus-hook");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, "").unwrap();

        install_hook_inner(repo.to_str().unwrap(), home.path(), &hook).unwrap();

        let text = std::fs::read_to_string(claude.join("settings.json")).unwrap();
        assert!(!text.contains(".agents/perm-gate.sh"), "settings: {text}");
        assert!(text.contains(&hook.to_string_lossy().to_string()));
        assert!(std::fs::read_dir(&claude).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("settings.json.bak-")));
    }
}
