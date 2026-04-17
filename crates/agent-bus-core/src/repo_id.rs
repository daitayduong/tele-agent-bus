use std::path::Path;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RepoIdError {
    #[error("repo path canonicalization failed for {path}: {source}")]
    Canonicalize {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

pub fn compute_repo_id(_display_slug: &str, _path: impl AsRef<Path>) -> Result<String, RepoIdError> {
    todo!("RED: implemented after tests")
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
}
