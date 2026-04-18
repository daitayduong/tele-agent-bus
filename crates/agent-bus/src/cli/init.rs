use std::fs;
use std::os::unix::fs::PermissionsExt;
use crate::cli::get_bus_home;

pub fn run() -> anyhow::Result<()> {
    let bus_home = get_bus_home();
    
    if !bus_home.exists() {
        fs::create_dir_all(&bus_home)?;
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&bus_home)?.permissions();
            perms.set_mode(0o700);
            fs::set_permissions(&bus_home, perms)?;
        }
        println!("Created {}", bus_home.display());
    }

    let config_path = bus_home.join("config.yaml");
    if !config_path.exists() {
        let default_config = r#"schema_version: 1
telegram:
  bot_token: env:TELE_BUS_BOT_TOKEN
  allowed_chats:
    - env:TELE_BUS_CHAT_ID
fail_mode: hybrid
log_level: info
permissions:
  timeout_seconds: 30
  fail_mode: hybrid
  blacklist_file: ~/.agent-bus/blacklist.txt
agents: {}
repos: []
"#;
        fs::write(&config_path, default_config)?;
        println!("Created {}", config_path.display());
    }

    let repos_path = bus_home.join("repos.yaml");
    if !repos_path.exists() {
        let default_repos = r#"schema_version: 1
repos: []
"#;
        fs::write(&repos_path, default_repos)?;
        println!("Created {}", repos_path.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_init_writes_bot_token_env_var() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("AGENT_BUS_HOME", dir.path());
        
        run().unwrap();

        let config_path = dir.path().join("config.yaml");
        let config = fs::read_to_string(config_path).unwrap();

        assert!(config.contains("bot_token: env:TELE_BUS_BOT_TOKEN"));
        assert!(config.contains("timeout_seconds: 30"));
    }
}
