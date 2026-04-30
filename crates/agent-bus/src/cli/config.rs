use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context};
use serde::Deserialize;

use crate::cli::get_bus_home;

#[derive(Debug, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    schema_version: Option<u32>,
    telegram: TelegramFile,
}

#[derive(Debug, Deserialize)]
struct TelegramFile {
    bot_token: String,
    #[serde(default)]
    allowed_chats: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ReposFile {
    #[serde(default)]
    schema_version: Option<u32>,
    #[serde(default)]
    repos: Vec<RepoEntry>,
}

#[derive(Debug, Deserialize)]
struct RepoEntry {
    id: String,
    display: String,
    path: String,
    #[serde(default)]
    agents: Vec<String>,
}

pub fn show() -> anyhow::Result<()> {
    show_inner(&get_bus_home())
}

pub fn validate() -> anyhow::Result<()> {
    validate_inner(&get_bus_home())
}

fn show_inner(bus: &Path) -> anyhow::Result<()> {
    let config_path = bus.join("config.yaml");
    let repos_path = bus.join("repos.yaml");

    println!("# {}", config_path.display());
    match fs::read_to_string(&config_path) {
        Ok(text) => println!("{text}"),
        Err(err) => println!("(unable to read: {err})"),
    }

    println!("# {}", repos_path.display());
    match fs::read_to_string(&repos_path) {
        Ok(text) => println!("{text}"),
        Err(err) => println!("(unable to read: {err})"),
    }
    Ok(())
}

fn validate_inner(bus: &Path) -> anyhow::Result<()> {
    let config_path = bus.join("config.yaml");
    let repos_path = bus.join("repos.yaml");

    let config = load_config(&config_path)?;
    let repos = load_repos(&repos_path)?;

    if config.telegram.bot_token.trim().is_empty() {
        return Err(anyhow!("config.yaml: telegram.bot_token is empty"));
    }
    if config.telegram.allowed_chats.is_empty() {
        return Err(anyhow!(
            "config.yaml: telegram.allowed_chats must have at least one entry"
        ));
    }

    let mut seen_ids = std::collections::HashSet::new();
    for repo in &repos.repos {
        if repo.id.trim().is_empty() {
            return Err(anyhow!("repos.yaml: repo with empty id"));
        }
        if !seen_ids.insert(repo.id.clone()) {
            return Err(anyhow!("repos.yaml: duplicate repo id {}", repo.id));
        }
        if repo.display.trim().is_empty() {
            return Err(anyhow!("repos.yaml: repo {} has empty display", repo.id));
        }
        if repo.path.trim().is_empty() {
            return Err(anyhow!("repos.yaml: repo {} has empty path", repo.id));
        }
        if repo.agents.is_empty() {
            return Err(anyhow!("repos.yaml: repo {} has no agents", repo.id));
        }
    }

    println!("OK");
    println!(
        "  config.yaml: schema_version={}, allowed_chats={}",
        config.schema_version.unwrap_or(1),
        config.telegram.allowed_chats.len()
    );
    println!(
        "  repos.yaml:  schema_version={}, repos={}",
        repos.schema_version.unwrap_or(1),
        repos.repos.len()
    );
    Ok(())
}

fn load_config(path: &Path) -> anyhow::Result<ConfigFile> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

fn load_repos(path: &Path) -> anyhow::Result<ReposFile> {
    if !path.exists() {
        return Ok(ReposFile {
            schema_version: Some(1),
            repos: Vec::new(),
        });
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_minimal_config() {
        let bus = tempfile::tempdir().unwrap();
        std::fs::write(
            bus.path().join("config.yaml"),
            "schema_version: 1\ntelegram:\n  bot_token: abc\n  allowed_chats: [\"123\"]\n",
        )
        .unwrap();
        std::fs::write(
            bus.path().join("repos.yaml"),
            "schema_version: 1\nrepos: []\n",
        )
        .unwrap();

        validate_inner(bus.path()).unwrap();
    }

    #[test]
    fn validate_rejects_empty_bot_token() {
        let bus = tempfile::tempdir().unwrap();
        std::fs::write(
            bus.path().join("config.yaml"),
            "telegram:\n  bot_token: \"\"\n  allowed_chats: [\"1\"]\n",
        )
        .unwrap();
        std::fs::write(bus.path().join("repos.yaml"), "repos: []\n").unwrap();

        let err = validate_inner(bus.path()).unwrap_err();
        assert!(err.to_string().contains("bot_token"));
    }

    #[test]
    fn validate_rejects_duplicate_repo_ids() {
        let bus = tempfile::tempdir().unwrap();
        std::fs::write(
            bus.path().join("config.yaml"),
            "telegram:\n  bot_token: abc\n  allowed_chats: [\"1\"]\n",
        )
        .unwrap();
        std::fs::write(
            bus.path().join("repos.yaml"),
            "repos:\n  - id: a\n    display: A\n    path: /tmp/a\n    agents: [claude]\n  - id: a\n    display: B\n    path: /tmp/b\n    agents: [claude]\n",
        )
        .unwrap();

        let err = validate_inner(bus.path()).unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }
}
