pub mod auth;
pub mod blacklist;
pub mod config;
pub mod init;
pub mod repo;

use std::path::PathBuf;

pub fn get_bus_home() -> PathBuf {
    if let Ok(home) = std::env::var("AGENT_BUS_HOME") {
        PathBuf::from(home)
    } else {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .expect("HOME or USERPROFILE env var must be set");
        PathBuf::from(home).join(".agent-bus")
    }
}
