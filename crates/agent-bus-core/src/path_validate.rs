use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PathValidationError {
    #[error("path does not exist: {path}")]
    DoesNotExist { path: String },
    #[error("path traversal is not allowed: {path}")]
    Traversal { path: String },
    #[error("symlinks are not allowed in repo paths: {path}")]
    Symlink { path: String },
    #[error("repo path is under forbidden root {root}: {path}")]
    ForbiddenRoot { path: String, root: String },
    #[error("repo path is outside home: {path}")]
    OutsideHome { path: String },
}

#[derive(Debug, Clone)]
pub struct PathPolicy {
    pub home: PathBuf,
    pub agent_bus_dir: PathBuf,
    pub allow_outside_home: bool,
}

impl PathPolicy {
    pub fn for_home(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self {
            agent_bus_dir: home.join(".agent-bus"),
            home,
            allow_outside_home: false,
        }
    }
}

pub fn validate_repo_path(
    _path: impl AsRef<Path>,
    _policy: &PathPolicy,
) -> Result<PathBuf, PathValidationError> {
    todo!("RED: implemented after tests")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_existing_directory_under_home() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let policy = PathPolicy::for_home(home.path());

        let validated = validate_repo_path(&repo, &policy).unwrap();

        assert_eq!(validated, repo.canonicalize().unwrap());
    }

    #[test]
    fn rejects_nonexistent_path_with_reason() {
        let home = tempfile::tempdir().unwrap();
        let policy = PathPolicy::for_home(home.path());
        let err = validate_repo_path(home.path().join("missing"), &policy).unwrap_err();

        assert!(matches!(err, PathValidationError::DoesNotExist { .. }));
        assert!(err.to_string().contains("path does not exist"));
    }

    #[test]
    fn rejects_parent_traversal_components() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let policy = PathPolicy::for_home(home.path());
        let err = validate_repo_path(home.path().join("repo/../repo"), &policy).unwrap_err();

        assert!(matches!(err, PathValidationError::Traversal { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_components() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("target");
        let link = home.path().join("link");
        std::fs::create_dir(&target).unwrap();
        symlink(&target, &link).unwrap();
        let policy = PathPolicy::for_home(home.path());

        let err = validate_repo_path(&link, &policy).unwrap_err();

        assert!(matches!(err, PathValidationError::Symlink { .. }));
    }

    #[test]
    fn rejects_agent_bus_directory() {
        let home = tempfile::tempdir().unwrap();
        let bus = home.path().join(".agent-bus");
        std::fs::create_dir(&bus).unwrap();
        let policy = PathPolicy::for_home(home.path());
        let err = validate_repo_path(&bus, &policy).unwrap_err();

        assert!(matches!(err, PathValidationError::ForbiddenRoot { .. }));
    }
}
