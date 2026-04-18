use std::path::Path;
use std::fmt;

use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RepoIdError {
    #[error("repo path canonicalization failed for {path}: {source}")]
    Canonicalize {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid repo id: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoId(String);

impl RepoId {
    pub fn new(id: String) -> Result<Self, RepoIdError> {
        // Validation logic from AC-R4
        let is_valid = id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            && !id.is_empty()
            && id.len() <= 64
            && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());

        if is_valid {
            Ok(Self(id))
        } else {
            Err(RepoIdError::Invalid(id))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for RepoId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}


pub fn compute_repo_id(display_slug: &str, path: impl AsRef<Path>) -> Result<String, RepoIdError> {
    let path_ref = path.as_ref();
    let canonical = path_ref
        .canonicalize()
        .map_err(|source| RepoIdError::Canonicalize {
            path: path_ref.display().to_string(),
            source,
        })?;
    let slug = slugify(display_slug);
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let hash = hex::encode(digest);

    Ok(format!("{slug}_{}", &hash[..8]))
}

fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;

    for byte in input.bytes() {
        let ch = byte.to_ascii_lowercase() as char;
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }

    while out.ends_with('-') {
        out.pop();
    }

    if out.is_empty() {
        "repo".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_for_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let first = compute_repo_id("Rally Up!", dir.path()).unwrap();
        let second = compute_repo_id("Rally Up!", dir.path()).unwrap();

        assert_eq!(first, second);
        assert!(first.starts_with("rally-up_"));
        assert_eq!(first.rsplit_once('_').unwrap().1.len(), 8);
    }

    #[test]
    fn different_paths_get_different_hashes() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();

        let a_id = compute_repo_id("app", a.path()).unwrap();
        let b_id = compute_repo_id("app", b.path()).unwrap();

        assert_ne!(a_id, b_id);
    }

    #[test]
    fn unicode_slug_is_normalized_to_ascii_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let id = compute_repo_id("工程", dir.path()).unwrap();

        assert!(id.starts_with("repo_"));
        assert_eq!(id.rsplit_once('_').unwrap().1.len(), 8);
    }

    #[test]
    fn test_repo_id_validation() {
        assert!(RepoId::new("good-id_123".to_string()).is_ok());
        assert!(RepoId::new("bad/id".to_string()).is_err());
        assert!(RepoId::new("".to_string()).is_err());
        let long_id = "a".repeat(65);
        assert!(RepoId::new(long_id).is_err());
        assert!(RepoId::new("-bad-start".to_string()).is_err());
    }
}
