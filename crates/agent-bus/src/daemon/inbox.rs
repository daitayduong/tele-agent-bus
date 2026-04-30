use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use time::OffsetDateTime;

static INBOX_LOCKS: Mutex<Vec<(PathBuf, Arc<Mutex<()>>)>> = Mutex::new(Vec::new());
pub fn append_inbox(repo_path: &Path, agent: &str, username: &str, body: &str) -> Result<()> {
    let inbox_dir = repo_path.join(".agents").join("inbox");
    fs::create_dir_all(&inbox_dir)
        .with_context(|| format!("failed to create {}", inbox_dir.display()))?;
    #[cfg(unix)]
    {
        fs::set_permissions(&inbox_dir, fs::Permissions::from_mode(0o750))
            .with_context(|| format!("failed to chmod {}", inbox_dir.display()))?;
    }

    let dest = inbox_dir.join(format!("{agent}.md"));
    let lock = lock_for(&dest);
    let _guard = lock.lock().expect("inbox lock poisoned");

    let mut contents = match fs::read(&dest) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", dest.display())),
    };
    if !contents.is_empty() && !contents.ends_with(b"\n") {
        contents.push(b'\n');
    }

    let timestamp = format_timestamp(OffsetDateTime::now_utc());
    let entry = format!("## {timestamp} — from @{username}\n{body}\n");
    contents.extend_from_slice(entry.as_bytes());

    let tmp = temp_path_for(&dest, std::process::id());
    {
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            options.mode(0o644);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        file.write_all(&contents)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", tmp.display()))?;
    }

    fs::rename(&tmp, &dest)
        .with_context(|| format!("failed to rename {} to {}", tmp.display(), dest.display()))?;
    fsync_dir(&inbox_dir)?;
    Ok(())
}

fn format_timestamp(ts: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        ts.year(),
        u8::from(ts.month()),
        ts.day(),
        ts.hour(),
        ts.minute(),
        ts.second()
    )
}

fn lock_for(dest: &Path) -> Arc<Mutex<()>> {
    let mut locks = INBOX_LOCKS.lock().expect("inbox locks lock poisoned");
    if let Some((_, lock)) = locks.iter().find(|(path, _)| path == dest) {
        return Arc::clone(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.push((dest.to_path_buf(), Arc::clone(&lock)));
    lock
}

pub(crate) fn temp_path_for(dest: &Path, pid: u32) -> PathBuf {
    let filename = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("inbox.md");
    dest.with_file_name(format!("{filename}.tmp.{pid}"))
}

fn fsync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::sync::Arc;
    use std::thread;

    use super::{append_inbox, temp_path_for};

    #[test]
    fn test_inbox_creates_dir_and_appends() {
        let dir = tempfile::tempdir().unwrap();

        append_inbox(dir.path(), "codex", "alice", "fix lint").unwrap();
        append_inbox(dir.path(), "codex", "alice", "run tests").unwrap();

        let inbox_dir = dir.path().join(".agents/inbox");
        let metadata = std::fs::metadata(&inbox_dir).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o750);
        let expected_uid = std::process::Command::new("id").arg("-u").output().unwrap();
        let expected_uid = String::from_utf8(expected_uid.stdout)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        assert_eq!(metadata.uid(), expected_uid);

        let contents = std::fs::read_to_string(inbox_dir.join("codex.md")).unwrap();
        assert!(contents.contains("from @alice"));
        assert!(contents.contains("fix lint"));
        assert!(contents.contains("run tests"));
        assert_eq!(contents.matches("## ").count(), 2);
    }

    #[test]
    fn test_inbox_tempfile_is_same_directory() {
        let dest = std::path::Path::new("/repo/.agents/inbox/codex.md");

        let tmp = temp_path_for(dest, 1234);

        assert_eq!(tmp.parent(), dest.parent());
        assert_eq!(
            tmp.file_name().and_then(|name| name.to_str()),
            Some("codex.md.tmp.1234")
        );
    }

    #[test]
    fn test_inbox_concurrent_writes_serialize() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Arc::new(dir.path().to_path_buf());
        let mut handles = Vec::new();

        for index in 0..10 {
            let repo = Arc::clone(&repo);
            handles.push(thread::spawn(move || {
                append_inbox(&repo, "codex", "alice", &format!("message-{index}")).unwrap();
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let contents = std::fs::read_to_string(dir.path().join(".agents/inbox/codex.md")).unwrap();
        assert_eq!(contents.matches("## ").count(), 10);
        for index in 0..10 {
            assert_eq!(contents.matches(&format!("message-{index}")).count(), 1);
        }

        let unique_lines = contents
            .lines()
            .filter(|line| line.starts_with("message-"))
            .collect::<HashSet<_>>();
        assert_eq!(unique_lines.len(), 10);
    }
}
