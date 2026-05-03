use agent_bus_core::approval_gate_integrity;
use agent_bus_core::repo_id::RepoId;
use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use fs2::FileExt;
use rand::{thread_rng, RngCore};
use regex::Regex;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[cfg(unix)]
const DEFAULT_ETC_DIR: &str = "/etc/agent-bus";
const DEFAULT_HOME_DIR: &str = "~/.agent-bus";

#[derive(Subcommand, Debug)]
pub enum GateCommands {
    /// Initialize /etc/agent-bus/ with key and empty signed gate (requires sudo)
    Init,
    /// Add a regex pattern to the gate
    Add {
        pattern: String,
        #[clap(long)]
        repo: Option<String>,
        /// Mark the rule as destructive: when daemon is unreachable, fail-closed (deny) instead of auto-approve
        #[clap(long)]
        destructive: bool,
    },
    /// Remove a regex pattern from the gate
    Remove {
        pattern: String,
        #[clap(long)]
        repo: Option<String>,
    },
    /// List current gate patterns
    List {
        #[clap(long)]
        repo: Option<String>,
    },
    /// Verify HMAC signature of the gate
    Verify {
        #[clap(long)]
        repo: Option<String>,
    },
    /// Generate new HMAC key and re-sign the gate (requires sudo)
    #[command(name = "rotate-key")]
    RotateKey,
}

pub fn handle(command: GateCommands) -> Result<()> {
    let etc_dir: PathBuf = std::env::var("AGENT_BUS_ETC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_etc_dir());
    let home_dir: PathBuf = crate::cli::get_bus_home();
    handle_inner(command, &etc_dir, &home_dir, get_euid)
}

fn default_etc_dir() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from(DEFAULT_ETC_DIR)
    }
    #[cfg(windows)]
    {
        std::env::var("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(r"C:\ProgramData"))
            .join("agent-bus")
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        crate::cli::get_bus_home().join("etc")
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn get_euid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn get_euid() -> u32 {
    0
}

fn handle_inner(
    command: GateCommands,
    etc_dir: &Path,
    home_dir: &Path,
    euid: fn() -> u32,
) -> Result<()> {
    match command {
        GateCommands::Init => init_inner(etc_dir, euid),
        GateCommands::Add {
            pattern,
            repo,
            destructive,
        } => add_inner(&pattern, repo, destructive, etc_dir, home_dir, euid),
        GateCommands::Remove { pattern, repo } => {
            remove_inner(&pattern, repo, etc_dir, home_dir, euid)
        }
        GateCommands::List { repo } => list_inner(repo, etc_dir, home_dir, euid),
        GateCommands::Verify { repo } => verify_inner(repo, etc_dir, home_dir, euid),
        GateCommands::RotateKey => rotate_key_inner(etc_dir, euid),
    }
}

fn check_root(euid: fn() -> u32) -> Result<()> {
    #[cfg(unix)]
    {
        if euid() != 0 {
            return Err(anyhow!("this subcommand requires sudo (effective UID 0)"));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = euid;
    }
    Ok(())
}

fn init_inner(etc_dir: &Path, euid: fn() -> u32) -> Result<()> {
    check_root(euid)?;

    if !etc_dir.exists() {
        fs::create_dir_all(etc_dir)?;
        #[cfg(unix)]
        fs::set_permissions(etc_dir, fs::Permissions::from_mode(0o750))?;
    }

    let key_path = etc_dir.join("approval-gate.key");
    if !key_path.exists() {
        let mut key = [0u8; 32];
        thread_rng().fill_bytes(&mut key);
        write_protected(&key_path, &key, true)?;
    }

    let conf_path = etc_dir.join("approval-gate.conf");
    if !conf_path.exists() {
        let home_dir = PathBuf::from(shellexpand::tilde(DEFAULT_HOME_DIR).into_owned());
        mutate_gate(etc_dir, &home_dir, None, |_| Ok(vec![]))?;
    }

    println!("Initialized gate in {}", etc_dir.display());
    Ok(())
}

fn add_inner(
    pattern: &str,
    repo: Option<String>,
    destructive: bool,
    etc_dir: &Path,
    home_dir: &Path,
    euid: fn() -> u32,
) -> Result<()> {
    if repo.is_none() {
        check_root(euid)?;
    }
    Regex::new(pattern).with_context(|| format!("invalid regex: {}", pattern))?;

    let repo_id = repo.map(RepoId::new).transpose()?;
    let line = if destructive {
        format!("{}\tdestructive", pattern)
    } else {
        pattern.to_string()
    };
    let line_for_msg = line.clone();

    mutate_gate(etc_dir, home_dir, repo_id.as_ref(), move |mut patterns| {
        // Replace any existing entry with same pattern (allows toggling destructive flag).
        patterns.retain(|p| p.split('\t').next().map(|s| s.trim()) != Some(pattern));
        patterns.push(line);
        Ok(patterns)
    })?;

    println!("Added pattern: {}", line_for_msg);
    Ok(())
}

fn remove_inner(
    pattern: &str,
    repo: Option<String>,
    etc_dir: &Path,
    home_dir: &Path,
    euid: fn() -> u32,
) -> Result<()> {
    if repo.is_none() {
        check_root(euid)?;
    }
    let repo_id = repo.map(RepoId::new).transpose()?;

    mutate_gate(etc_dir, home_dir, repo_id.as_ref(), |mut patterns| {
        let before = patterns.len();
        // Match on the regex part only (before any \tflags), so callers
        // don't need to know whether the rule was added with --destructive.
        patterns.retain(|p| p.split('\t').next().map(|s| s.trim()) != Some(pattern));
        if patterns.len() == before {
            return Err(anyhow!("pattern not found: {}", pattern));
        }
        Ok(patterns)
    })?;

    println!("Removed pattern: {}", pattern);
    Ok(())
}

fn list_inner(
    repo: Option<String>,
    etc_dir: &Path,
    home_dir: &Path,
    _euid: fn() -> u32,
) -> Result<()> {
    let (conf_path, _) = get_gate_paths(etc_dir, home_dir, repo.as_deref())?;
    if !conf_path.exists() {
        println!("ApprovalGate empty (file missing)");
        return Ok(());
    }
    let body = fs::read_to_string(conf_path)?;
    for line in body.lines() {
        if !line.trim().is_empty() {
            println!("{}", line);
        }
    }
    Ok(())
}

fn verify_inner(
    repo: Option<String>,
    etc_dir: &Path,
    home_dir: &Path,
    _euid: fn() -> u32,
) -> Result<()> {
    let repo_id = repo.as_ref().map(|r| RepoId::new(r.clone())).transpose()?;
    let (conf_path, hmac_path) =
        get_gate_paths(etc_dir, home_dir, repo_id.as_ref().map(|r| r.as_str()))?;
    let key_path = get_key_path(etc_dir, repo.is_some())?;

    approval_gate_integrity::load_and_verify(&conf_path, &hmac_path, &key_path)
        .map_err(|e| anyhow!("integrity check FAILED: {}", e))?;

    println!("Integrity verified.");
    Ok(())
}

fn rotate_key_inner(etc_dir: &Path, euid: fn() -> u32) -> Result<()> {
    check_root(euid)?;

    let home_dir = PathBuf::from(shellexpand::tilde(DEFAULT_HOME_DIR).into_owned());

    // 1. Verify and load with OLD key
    let patterns = load_and_verify(etc_dir, &home_dir, None)?;

    // 2. Generate NEW key
    let mut new_key = [0u8; 32];
    thread_rng().fill_bytes(&mut new_key);
    let key_path = etc_dir.join("approval-gate.key");

    // We need to lock during the whole process
    let lock = Lock::new(etc_dir)?;

    // 3. Write NEW key
    write_protected(&key_path, &new_key, true)?;

    // 4. Re-sign with NEW key
    save_gate(etc_dir, &home_dir, None, &patterns)?;

    drop(lock);
    println!("WARNING: HMAC key rotated and gate re-signed.");
    Ok(())
}

// Low-level helpers

fn get_repo_dir(home_dir: &Path, repo_id: &str) -> Result<PathBuf> {
    let repos_dir = home_dir.join("repos");
    let repo_dir = repos_dir.join(repo_id);
    let repos_conf_path = home_dir.join("repos.yaml");
    let repos = agent_bus_core::config::load_repos_from_path(&repos_conf_path)?;
    if !repos.iter().any(|r| r.id == repo_id) {
        return Err(anyhow!(
            "unknown repo: {} (register with: agent-bus repo add <path>)",
            repo_id
        ));
    }
    Ok(repo_dir)
}

fn get_gate_paths(
    etc_dir: &Path,
    home_dir: &Path,
    repo_id: Option<&str>,
) -> Result<(PathBuf, PathBuf)> {
    if let Some(repo_id) = repo_id {
        let repo_dir = get_repo_dir(home_dir, repo_id)?;
        if !repo_dir.exists() {
            fs::create_dir_all(&repo_dir)?;
        }
        Ok((
            repo_dir.join("approval-gate.conf"),
            repo_dir.join("approval-gate.conf.hmac"),
        ))
    } else {
        Ok((
            etc_dir.join("approval-gate.conf"),
            etc_dir.join("approval-gate.conf.hmac"),
        ))
    }
}

fn get_key_path(etc_dir: &Path, is_repo: bool) -> Result<PathBuf> {
    let key_path = etc_dir.join("approval-gate.key");
    if is_repo {
        // Per-repo operations run as the user; surface a friendly error if the
        // shared key is missing or unreadable (user not in agent-bus group).
        match fs::File::open(&key_path) {
            Ok(_) => {}
            Err(e) => {
                return Err(anyhow!(
                    "cannot read /etc/agent-bus/approval-gate.key ({}): add current user to the agent-bus group (sudo usermod -aG agent-bus $USER) and re-login",
                    e
                ));
            }
        }
    }
    Ok(key_path)
}

fn load_and_verify(etc_dir: &Path, home_dir: &Path, repo: Option<&RepoId>) -> Result<Vec<String>> {
    let (conf_path, hmac_path) = get_gate_paths(etc_dir, home_dir, repo.map(|r| r.as_str()))?;
    let key_path = get_key_path(etc_dir, repo.is_some())?;

    if !conf_path.exists() {
        return Ok(vec![]);
    }

    approval_gate_integrity::load_and_verify(&conf_path, &hmac_path, &key_path)
        .map_err(|e| anyhow!("failed to load gate: {}", e))
}

fn mutate_gate<F>(etc_dir: &Path, home_dir: &Path, repo: Option<&RepoId>, f: F) -> Result<()>
where
    F: FnOnce(Vec<String>) -> Result<Vec<String>>,
{
    let lock_dir = if let Some(repo) = repo {
        get_repo_dir(home_dir, repo.as_str())?
    } else {
        etc_dir.to_path_buf()
    };
    if !lock_dir.exists() {
        fs::create_dir_all(&lock_dir)?;
    }
    let _lock = Lock::new(&lock_dir)?;

    // 1. Verify current (if exists)
    let patterns = load_and_verify(etc_dir, home_dir, repo)?;

    // 2. Mutate
    let new_patterns = f(patterns)?;

    // 3. Save (atomic write + re-sign)
    save_gate(etc_dir, home_dir, repo, &new_patterns)
}

fn save_gate(
    etc_dir: &Path,
    home_dir: &Path,
    repo: Option<&RepoId>,
    patterns: &[String],
) -> Result<()> {
    let (conf_path, hmac_path) = get_gate_paths(etc_dir, home_dir, repo.map(|r| r.as_str()))?;
    let key_path = get_key_path(etc_dir, repo.is_some())?;
    let key =
        fs::read(&key_path).context(format!("missing or unreadable {}", key_path.display()))?;

    let body = patterns.join("\n");
    let hmac = approval_gate_integrity::compute_hmac(&key, body.as_bytes());

    let is_global = repo.is_none();
    write_atomic(&conf_path, body.as_bytes(), is_global)?;
    write_atomic(&hmac_path, hmac.as_bytes(), is_global)?;

    Ok(())
}

fn write_atomic(path: &Path, data: &[u8], is_global: bool) -> Result<()> {
    let tmp = path.with_extension("tmp");
    write_protected(&tmp, data, is_global)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn write_protected(path: &Path, data: &[u8], is_global: bool) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);

    let _ = is_global;
    #[cfg(unix)]
    options.mode(0o640);

    let mut f = options.open(path)?;

    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

struct Lock {
    _file: fs::File,
}

impl Lock {
    fn new(dir: &Path) -> Result<Self> {
        let path = dir.join(".lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        file.lock_exclusive()?;

        Ok(Self { _file: file })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn euid_root() -> u32 {
        0
    }
    fn euid_user() -> u32 {
        1000
    }

    #[test]
    fn test_init_as_root() {
        let d = tempdir().unwrap();
        init_inner(d.path(), euid_root).unwrap();
        assert!(d.path().join("approval-gate.key").exists());
        assert!(d.path().join("approval-gate.conf").exists());
        assert!(d.path().join("approval-gate.conf.hmac").exists());
    }

    #[test]
    fn test_init_as_user_fails() {
        let d = tempdir().unwrap();
        let res = init_inner(d.path(), euid_user);
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("requires sudo"));
    }

    #[test]
    fn test_add_remove_flow() {
        let d = tempdir().unwrap();
        let home = tempdir().unwrap();
        init_inner(d.path(), euid_root).unwrap();

        add_inner("^rm -rf", None, false, d.path(), home.path(), euid_root).unwrap();
        add_inner("^ls -R", None, false, d.path(), home.path(), euid_root).unwrap();

        let conf = fs::read_to_string(d.path().join("approval-gate.conf")).unwrap();
        assert!(conf.contains("^rm -rf"));
        assert!(conf.contains("^ls -R"));

        verify_inner(None, d.path(), home.path(), euid_root).unwrap();

        remove_inner("^rm -rf", None, d.path(), home.path(), euid_root).unwrap();
        let conf = fs::read_to_string(d.path().join("approval-gate.conf")).unwrap();
        assert!(!conf.contains("^rm -rf"));
        assert!(conf.contains("^ls -R"));
    }

    #[test]
    fn test_invalid_regex_rejected() {
        let d = tempdir().unwrap();
        let home = tempdir().unwrap();
        init_inner(d.path(), euid_root).unwrap();
        let res = add_inner("[[[", None, false, d.path(), home.path(), euid_root);
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("invalid regex"));
    }

    #[test]
    fn test_rotate_key() {
        let d = tempdir().unwrap();
        let home = tempdir().unwrap();
        init_inner(d.path(), euid_root).unwrap();
        add_inner("pattern1", None, false, d.path(), home.path(), euid_root).unwrap();

        let old_key = fs::read(d.path().join("approval-gate.key")).unwrap();
        rotate_key_inner(d.path(), euid_root).unwrap();
        let new_key = fs::read(d.path().join("approval-gate.key")).unwrap();

        assert_ne!(old_key, new_key);
        verify_inner(None, d.path(), home.path(), euid_root).unwrap();

        let conf = fs::read_to_string(d.path().join("approval-gate.conf")).unwrap();
        assert!(conf.contains("pattern1"));
    }

    #[test]
    fn test_tamper_detection() {
        let d = tempdir().unwrap();
        let home = tempdir().unwrap();
        init_inner(d.path(), euid_root).unwrap();
        add_inner("p1", None, false, d.path(), home.path(), euid_root).unwrap();

        // Tamper
        fs::write(d.path().join("approval-gate.conf"), "tampered").unwrap();

        let res = verify_inner(None, d.path(), home.path(), euid_root);
        assert!(res.is_err());
        assert!(res
            .unwrap_err()
            .to_string()
            .contains("integrity check FAILED"));
    }
}
