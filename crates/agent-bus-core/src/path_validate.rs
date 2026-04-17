use std::path::{Component, Path, PathBuf};

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
    path: impl AsRef<Path>,
    policy: &PathPolicy,
) -> Result<PathBuf, PathValidationError> {
    let path = path.as_ref();
    let display = path.display().to_string();

    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(PathValidationError::Traversal { path: display });
    }

    reject_symlink_components(path)?;

    let canonical = path
        .canonicalize()
        .map_err(|_| PathValidationError::DoesNotExist {
            path: path.display().to_string(),
        })?;

    for root in forbidden_roots(policy) {
        if canonical == root || canonical.starts_with(&root) {
            return Err(PathValidationError::ForbiddenRoot {
                path: canonical.display().to_string(),
                root: root.display().to_string(),
            });
        }
    }

    let home = policy
        .home
        .canonicalize()
        .unwrap_or_else(|_| policy.home.clone());
    if !policy.allow_outside_home && !canonical.starts_with(&home) {
        return Err(PathValidationError::OutsideHome {
            path: canonical.display().to_string(),
        });
    }

    Ok(canonical)
}

fn reject_symlink_components(path: &Path) -> Result<(), PathValidationError> {
    let mut current = PathBuf::new();

    for component in path.components() {
        current.push(component.as_os_str());
        if let Ok(meta) = std::fs::symlink_metadata(&current) {
            if meta.file_type().is_symlink() {
                return Err(PathValidationError::Symlink {
                    path: current.display().to_string(),
                });
            }
        }
    }

    Ok(())
}

fn forbidden_roots(policy: &PathPolicy) -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/etc"),
        PathBuf::from("/root"),
        PathBuf::from("/proc"),
        PathBuf::from("/sys"),
        PathBuf::from("/dev"),
        policy.home.join(".ssh"),
        policy.home.join(".gnupg"),
        policy.agent_bus_dir.clone(),
    ];

    for root in &mut roots {
        if let Ok(canonical) = root.canonicalize() {
            *root = canonical;
        }
    }

    roots
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
