use std::fs;
use std::path::{Path, PathBuf};

use agent_bus_core::path_validate::{validate_repo_path, PathPolicy};
use agent_bus_core::repo_id::compute_repo_id;
use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

use crate::cli::get_bus_home;

const DEFAULT_AGENTS: &[&str] = &["claude", "gemini", "codex"];

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
}
