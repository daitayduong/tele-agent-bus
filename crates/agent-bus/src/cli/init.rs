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
  bot_token: env:TELE_BUS_TOKEN
  allowed_chats:
    - env:TELE_BUS_CHAT_ID
fail_mode: hybrid
log_level: info
permissions:
  timeout_seconds: 10
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
