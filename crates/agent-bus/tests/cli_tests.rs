use assert_cmd::prelude::*;
use std::process::Command;
use tempfile::tempdir;

#[test]
fn test_init_creates_config_and_repos() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let bus_home = tmp.path().join(".agent-bus");

    let mut cmd = Command::cargo_bin("agent-bus")?;
    cmd.env("AGENT_BUS_HOME", &bus_home);
    cmd.arg("init");
    cmd.assert().success();

    assert!(bus_home.exists());
    assert!(bus_home.join("config.yaml").exists());
    assert!(bus_home.join("repos.yaml").exists());

    // Check directory permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(&bus_home)?;
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }

    Ok(())
}
