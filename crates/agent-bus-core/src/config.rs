use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("Missing environment variable: {0}")]
    EnvVarMissing(String),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    pub version: u32,
    pub telegram: TelegramConfig,
    pub permissions: PermissionsConfig,
    pub agents: HashMap<String, AgentConfig>,
    pub repos: Vec<RepoConfig>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub allowed_chats: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PermissionsConfig {
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    pub fail_mode: FailMode,
    pub blacklist_file: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FailMode {
    Approve,
    Deny,
    Hybrid,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentConfig {
    pub mode: String,
    pub cli_path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RepoConfig {
    pub id: String,
    pub path: String,
    pub agents: Vec<String>,
}

impl Config {
    pub fn load_from_str(s: &str) -> Result<Self, ConfigError> {
        let mut config: Config = serde_yaml::from_str(s)?;
        config.resolve_env()?;
        Ok(config)
    }

    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let s = std::fs::read_to_string(path)?;
        Self::load_from_str(&s)
    }

    fn resolve_env(&mut self) -> Result<(), ConfigError> {
        self.telegram.bot_token = resolve_value(&self.telegram.bot_token)?;
        Ok(())
    }
}

fn resolve_value(val: &str) -> Result<String, ConfigError> {
    if let Some(var_name) = val.strip_prefix("env:") {
        std::env::var(var_name).map_err(|_| ConfigError::EnvVarMissing(var_name.to_string()))
    } else {
        Ok(val.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_env_resolution() {
        env::set_var("TEST_TOKEN", "secret-value");
        let yaml = "
version: 1
telegram:
  bot_token: env:TEST_TOKEN
  allowed_chats: [\"123\"]
permissions:
  timeout_seconds: 10
  fail_mode: hybrid
  blacklist_file: ~/.agent-bus/blacklist.txt
agents: {}
repos: []
";
        let config = Config::load_from_str(yaml).unwrap();
        assert_eq!(config.telegram.bot_token, "secret-value");
    }

    #[test]
    fn test_env_resolution_missing() {
        let yaml = "
version: 1
telegram:
  bot_token: env:NON_EXISTENT_VAR_XYZ
  allowed_chats: [\"123\"]
permissions:
  timeout_seconds: 10
  fail_mode: hybrid
  blacklist_file: ~/.agent-bus/blacklist.txt
agents: {}
repos: []
";
        let result = Config::load_from_str(yaml);
        assert!(matches!(result, Err(ConfigError::EnvVarMissing(_))));
    }

    #[test]
    fn test_full_config() {
        let yaml = "
version: 1
telegram:
  bot_token: plain-token
  allowed_chats: [\"123\"]
permissions:
  timeout_seconds: 10
  fail_mode: hybrid
  blacklist_file: ~/.agent-bus/blacklist.txt
agents:
  claude:
    mode: ide-first
repos:
  - id: rallyup
    path: ~/Projects/RallyUp
    agents: [claude]
";
        let config = Config::load_from_str(yaml).unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.telegram.bot_token, "plain-token");
        assert_eq!(config.permissions.fail_mode, FailMode::Hybrid);
        assert!(config.agents.contains_key("claude"));
        assert_eq!(config.repos[0].id, "rallyup");
    }

    #[test]
    fn test_default_timeout_is_30_seconds() {
        let yaml = "
version: 1
telegram:
  bot_token: plain-token
  allowed_chats: [\"123\"]
permissions:
  fail_mode: hybrid
  blacklist_file: ~/.agent-bus/blacklist.txt
agents: {}
repos: []
";
        let config = Config::load_from_str(yaml).unwrap();
        assert_eq!(config.permissions.timeout_seconds, 30);
    
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ReposFile {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    repos: Vec<RepoConfig>,
}

pub fn load_repos_from_path<P: AsRef<Path>>(path: P) -> Result<Vec<RepoConfig>, ConfigError> {
    let s = std::fs::read_to_string(path)?;
    let file: ReposFile = serde_yaml::from_str(&s)?;
    Ok(file.repos)
}

fn default_timeout_seconds() -> u64 {
    30
}
