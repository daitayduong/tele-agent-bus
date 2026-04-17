pub mod config;
pub mod init;
pub mod repo;
pub mod blacklist;

use std::path::PathBuf;

pub fn get_bus_home() -> PathBuf {
    if let Ok(home) = std::env::var("AGENT_BUS_HOME") {
        PathBuf::from(home)
    } else {
        let home = std::env::var("HOME").expect("HOME env var must be set");
        PathBuf::from(home).join(".agent-bus")
    }
}
