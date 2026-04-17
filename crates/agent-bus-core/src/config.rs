use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub allowed_chats: Vec<String>,
}

impl Config {
    pub fn load_from_str(s: &str) -> Result<Self, ConfigError> {
        let config: Config = serde_yaml::from_str(s)?;
        // RED: Does not resolve env: prefix yet
        Ok(config)
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
";
        let config = Config::load_from_str(yaml).unwrap();
        // This will fail because it won't resolve "env:TEST_TOKEN"
        assert_eq!(config.telegram.bot_token, "secret-value");
    }
}
