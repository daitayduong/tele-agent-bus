use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use clap::Subcommand;
use anyhow::{Result, Context, anyhow};
use regex::Regex;
use agent_bus_core::blacklist_integrity;
use rand::{RngCore, thread_rng};
use std::os::unix::fs::PermissionsExt;

const DEFAULT_ETC_DIR: &str = "/etc/agent-bus";

#[derive(Subcommand, Debug)]
pub enum BlacklistCommands {
    /// Initialize /etc/agent-bus/ with key and empty signed blacklist (requires sudo)
    Init,
    /// Add a regex pattern to the blacklist (requires sudo)
    Add { pattern: String },
    /// Remove a regex pattern from the blacklist (requires sudo)
    Remove { pattern: String },
    /// List current blacklist patterns
    List,
    /// Verify HMAC signature of the blacklist
    Verify,
    /// Generate new HMAC key and re-sign the blacklist (requires sudo)
    #[command(name = "rotate-key")]
    RotateKey,
}

pub fn handle(command: BlacklistCommands) -> Result<()> {
    handle_inner(command, Path::new(DEFAULT_ETC_DIR), get_euid)
}

#[allow(unsafe_code)]
fn get_euid() -> u32 {
    unsafe { libc::geteuid() }
}

fn handle_inner(command: BlacklistCommands, etc_dir: &Path, euid: fn() -> u32) -> Result<()> {
    match command {
        BlacklistCommands::Init => init_inner(etc_dir, euid),
        BlacklistCommands::Add { pattern } => add_inner(&pattern, etc_dir, euid),
        BlacklistCommands::Remove { pattern } => remove_inner(&pattern, etc_dir, euid),
        BlacklistCommands::List => list_inner(etc_dir),
        BlacklistCommands::Verify => verify_inner(etc_dir),
        BlacklistCommands::RotateKey => rotate_key_inner(etc_dir, euid),
    }
}

fn check_root(euid: fn() -> u32) -> Result<()> {
    if euid() != 0 {
        return Err(anyhow!("this subcommand requires sudo (effective UID 0)"));
    }
    Ok(())
}

fn init_inner(etc_dir: &Path, euid: fn() -> u32) -> Result<()> {
    check_root(euid)?;

    if !etc_dir.exists() {
        fs::create_dir_all(etc_dir)?;
        fs::set_permissions(etc_dir, fs::Permissions::from_mode(0o750))?;
    }

    let key_path = etc_dir.join("blacklist.key");
    if !key_path.exists() {
        let mut key = [0u8; 32];
        thread_rng().fill_bytes(&mut key);
        write_protected(&key_path, &key)?;
    }

    let conf_path = etc_dir.join("blacklist.conf");
    if !conf_path.exists() {
        mutate_blacklist(etc_dir, |_| Ok(vec![]))?;
    }

    println!("Initialized blacklist in {}", etc_dir.display());
    Ok(())
}

fn add_inner(pattern: &str, etc_dir: &Path, euid: fn() -> u32) -> Result<()> {
    check_root(euid)?;
    Regex::new(pattern).with_context(|| format!("invalid regex: {}", pattern))?;

    mutate_blacklist(etc_dir, |mut patterns| {
        if !patterns.contains(&pattern.to_string()) {
            patterns.push(pattern.to_string());
        }
        Ok(patterns)
    })?;

    println!("Added pattern: {}", pattern);
    Ok(())
}

fn remove_inner(pattern: &str, etc_dir: &Path, euid: fn() -> u32) -> Result<()> {
    check_root(euid)?;

    mutate_blacklist(etc_dir, |mut patterns| {
        let before = patterns.len();
        patterns.retain(|p| p != pattern);
        if patterns.len() == before {
            return Err(anyhow!("pattern not found: {}", pattern));
        }
        Ok(patterns)
    })?;

    println!("Removed pattern: {}", pattern);
    Ok(())
}

fn list_inner(etc_dir: &Path) -> Result<()> {
    let conf_path = etc_dir.join("blacklist.conf");
    if !conf_path.exists() {
        println!("Blacklist empty (file missing)");
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

fn verify_inner(etc_dir: &Path) -> Result<()> {
    let conf_path = etc_dir.join("blacklist.conf");
    let hmac_path = etc_dir.join("blacklist.conf.hmac");
    let key_path = etc_dir.join("blacklist.key");

    blacklist_integrity::load_and_verify(&conf_path, &hmac_path, &key_path)
        .map_err(|e| anyhow!("integrity check FAILED: {}", e))?;

    println!("Integrity verified.");
    Ok(())
}

fn rotate_key_inner(etc_dir: &Path, euid: fn() -> u32) -> Result<()> {
    check_root(euid)?;

    // 1. Verify and load with OLD key
    let patterns = load_and_verify(etc_dir)?;

    // 2. Generate NEW key
    let mut new_key = [0u8; 32];
    thread_rng().fill_bytes(&mut new_key);
    let key_path = etc_dir.join("blacklist.key");
    
    // We need to lock during the whole process
    let lock = Lock::new(etc_dir)?;

    // 3. Write NEW key
    write_protected(&key_path, &new_key)?;

    // 4. Re-sign with NEW key
    save_blacklist(etc_dir, &patterns)?;

    drop(lock);
    println!("WARNING: HMAC key rotated and blacklist re-signed.");
    Ok(())
}

// Low-level helpers

fn load_and_verify(etc_dir: &Path) -> Result<Vec<String>> {
    let conf_path = etc_dir.join("blacklist.conf");
    let hmac_path = etc_dir.join("blacklist.conf.hmac");
    let key_path = etc_dir.join("blacklist.key");

    if !conf_path.exists() {
        return Ok(vec![]);
    }

    blacklist_integrity::load_and_verify(&conf_path, &hmac_path, &key_path)
        .map_err(|e| anyhow!("failed to load blacklist: {}", e))
}

fn mutate_blacklist<F>(etc_dir: &Path, f: F) -> Result<()>
where
    F: FnOnce(Vec<String>) -> Result<Vec<String>>,
{
    let _lock = Lock::new(etc_dir)?;

    // 1. Verify current (if exists)
    let patterns = load_and_verify(etc_dir)?;

    // 2. Mutate
    let new_patterns = f(patterns)?;

    // 3. Save (atomic write + re-sign)
    save_blacklist(etc_dir, &new_patterns)
}

fn save_blacklist(etc_dir: &Path, patterns: &[String]) -> Result<()> {
    let body = patterns.join("\n");
    let key_path = etc_dir.join("blacklist.key");
    let key = fs::read(&key_path).context("missing blacklist.key")?;
    
    let hmac = blacklist_integrity::compute_hmac(&key, body.as_bytes());

    let conf_path = etc_dir.join("blacklist.conf");
    let hmac_path = etc_dir.join("blacklist.conf.hmac");

    write_atomic(&conf_path, body.as_bytes())?;
    write_atomic(&hmac_path, hmac.as_bytes())?;

    Ok(())
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    write_protected(&tmp, data)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn write_protected(path: &Path, data: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o640)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

struct Lock {
    _file: fs::File,
}

#[allow(unsafe_code)]
impl Lock {
    fn new(etc_dir: &Path) -> Result<Self> {
        let path = etc_dir.join(".lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;
        
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
        let res = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if res != 0 {
            return Err(io::Error::last_os_error().into());
        }

        Ok(Self { _file: file })
    }
}

#[allow(unsafe_code)]
impl Drop for Lock {
    fn drop(&mut self) {
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&self._file);
        unsafe { libc::flock(fd, libc::LOCK_UN); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn euid_root() -> u32 { 0 }
    fn euid_user() -> u32 { 1000 }

    #[test]
    fn test_init_as_root() {
        let d = tempdir().unwrap();
        init_inner(d.path(), euid_root).unwrap();
        assert!(d.path().join("blacklist.key").exists());
        assert!(d.path().join("blacklist.conf").exists());
        assert!(d.path().join("blacklist.conf.hmac").exists());
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
        init_inner(d.path(), euid_root).unwrap();

        add_inner("^rm -rf", d.path(), euid_root).unwrap();
        add_inner("^ls -R", d.path(), euid_root).unwrap();
        
        let conf = fs::read_to_string(d.path().join("blacklist.conf")).unwrap();
        assert!(conf.contains("^rm -rf"));
        assert!(conf.contains("^ls -R"));

        verify_inner(d.path()).unwrap();

        remove_inner("^rm -rf", d.path(), euid_root).unwrap();
        let conf = fs::read_to_string(d.path().join("blacklist.conf")).unwrap();
        assert!(!conf.contains("^rm -rf"));
        assert!(conf.contains("^ls -R"));
    }

    #[test]
    fn test_invalid_regex_rejected() {
        let d = tempdir().unwrap();
        init_inner(d.path(), euid_root).unwrap();
        let res = add_inner("[[[", d.path(), euid_root);
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("invalid regex"));
    }

    #[test]
    fn test_rotate_key() {
        let d = tempdir().unwrap();
        init_inner(d.path(), euid_root).unwrap();
        add_inner("pattern1", d.path(), euid_root).unwrap();

        let old_key = fs::read(d.path().join("blacklist.key")).unwrap();
        rotate_key_inner(d.path(), euid_root).unwrap();
        let new_key = fs::read(d.path().join("blacklist.key")).unwrap();

        assert_ne!(old_key, new_key);
        verify_inner(d.path()).unwrap();
        
        let conf = fs::read_to_string(d.path().join("blacklist.conf")).unwrap();
        assert!(conf.contains("pattern1"));
    }

    #[test]
    fn test_tamper_detection() {
        let d = tempdir().unwrap();
        init_inner(d.path(), euid_root).unwrap();
        add_inner("p1", d.path(), euid_root).unwrap();

        // Tamper
        fs::write(d.path().join("blacklist.conf"), "tampered").unwrap();
        
        let res = verify_inner(d.path());
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("integrity check FAILED"));
    }
}
