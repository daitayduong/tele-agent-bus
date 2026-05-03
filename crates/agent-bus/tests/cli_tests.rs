use assert_cmd::prelude::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

#[test]
fn test_init_creates_config_and_repos() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let bus_home = tmp.path().join(".agent-bus");

    let mut cmd = Command::cargo_bin("agent-bus").unwrap();
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

fn setup_test_env(bus_home: &Path, etc_dir: &Path) -> anyhow::Result<()> {
    // bus_home is the ".agent-bus" dir itself (matches AGENT_BUS_HOME convention).
    fs::create_dir_all(bus_home.join("repos"))?;

    fs::create_dir_all(etc_dir)?;
    fs::write(
        etc_dir.join("approval-gate.key"),
        "01234567890123456789012345678901",
    )?;
    fs::set_permissions(
        etc_dir.join("approval-gate.key"),
        fs::Permissions::from_mode(0o640),
    )?;

    // repos.yaml wraps the list under schema_version + repos.
    let yaml =
        "schema_version: 1\nrepos:\n  - id: sample_repo\n    path: /tmp/sample_repo\n    agents: []\n";
    fs::write(bus_home.join("repos.yaml"), yaml)?;
    Ok(())
}

#[test]
fn test_cli_add_per_repo_writes_user_owned_file() -> anyhow::Result<()> {
    let home_tmp = tempdir()?;
    let bus_home = home_tmp.path().join(".agent-bus");
    let etc_tmp = tempdir()?;
    setup_test_env(&bus_home, etc_tmp.path())?;

    let mut cmd = Command::cargo_bin("agent-bus").unwrap();
    cmd.env("AGENT_BUS_HOME", &bus_home);
    cmd.env("AGENT_BUS_ETC_DIR", etc_tmp.path()); // Custom env var for testing
    cmd.args(["gate", "add", "--repo", "sample_repo", "^git push"]);
    cmd.assert().success();

    let gate_path = bus_home.join("repos/sample_repo/approval-gate.conf");
    let hmac_path = bus_home.join("repos/sample_repo/approval-gate.conf.hmac");

    assert!(gate_path.exists());
    assert!(hmac_path.exists());

    // On Unix, check ownership
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(&gate_path)?;
        assert_eq!(meta.uid(), nix::unistd::geteuid().as_raw());
        assert_eq!(meta.gid(), nix::unistd::getegid().as_raw());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o640);
    }

    Ok(())
}

#[test]
fn test_cli_add_per_repo_fails_when_key_unreadable() -> anyhow::Result<()> {
    let home_tmp = tempdir()?;
    let bus_home = home_tmp.path().join(".agent-bus");
    let etc_tmp = tempdir()?;
    setup_test_env(&bus_home, etc_tmp.path())?;

    // Make key unreadable
    let key_path = etc_tmp.path().join("approval-gate.key");
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o000))?;

    let mut cmd = Command::cargo_bin("agent-bus").unwrap();
    cmd.env("AGENT_BUS_HOME", &bus_home);
    cmd.env("AGENT_BUS_ETC_DIR", etc_tmp.path());
    cmd.args(["gate", "add", "--repo", "sample_repo", "^git push"]);

    cmd.assert()
        .failure()
        .stderr(predicates::str::contains(
            "cannot read /etc/agent-bus/approval-gate.key",
        ))
        .stderr(predicates::str::contains(
            "add current user to the agent-bus group",
        ));

    // Restore permissions for cleanup
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o640))?;

    Ok(())
}

#[test]
fn test_cli_list_per_repo_no_sudo_required() -> anyhow::Result<()> {
    let home_tmp = tempdir()?;
    let bus_home = home_tmp.path().join(".agent-bus");
    let etc_tmp = tempdir()?;
    setup_test_env(&bus_home, etc_tmp.path())?;

    // Add a rule first
    let mut cmd = Command::cargo_bin("agent-bus").unwrap();
    cmd.env("AGENT_BUS_HOME", &bus_home);
    cmd.env("AGENT_BUS_ETC_DIR", etc_tmp.path());
    cmd.args(["gate", "add", "--repo", "sample_repo", "secret-pattern"]);
    cmd.assert().success();

    // Now list it
    let mut cmd = Command::cargo_bin("agent-bus").unwrap();
    cmd.env("AGENT_BUS_HOME", &bus_home);
    cmd.env("AGENT_BUS_ETC_DIR", etc_tmp.path());
    cmd.args(["gate", "list", "--repo", "sample_repo"]);

    cmd.assert()
        .success()
        .stdout(predicates::str::contains("secret-pattern"));

    Ok(())
}

#[test]
fn test_cli_add_rejects_unknown_repo() -> anyhow::Result<()> {
    let home_tmp = tempdir()?;
    let bus_home = home_tmp.path().join(".agent-bus");
    let etc_tmp = tempdir()?;
    setup_test_env(&bus_home, etc_tmp.path())?;

    let mut cmd = Command::cargo_bin("agent-bus").unwrap();
    cmd.env("AGENT_BUS_HOME", &bus_home);
    cmd.env("AGENT_BUS_ETC_DIR", etc_tmp.path());
    cmd.args(["gate", "add", "--repo", "nonexistent-repo", "pattern"]);

    cmd.assert()
        .failure()
        .stderr(predicates::str::contains("unknown repo: nonexistent-repo"));

    Ok(())
}
