use crate::config::parse_repo_policy_bytes;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::config_mutation::platform_private_file_open_flags;
use crate::config_mutation::sync_directory;
use crate::error::QghError;
use serde_json::json;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepoPolicyPublication {
    Created,
    Overwritten,
}

impl RepoPolicyPublication {
    fn action(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Overwritten => "overwritten",
        }
    }
}

#[derive(Debug, Clone)]
enum RepoPolicySnapshot {
    Missing,
    Existing {
        bytes: Vec<u8>,
        repo: Result<String, QghError>,
    },
}

#[derive(Debug)]
pub(crate) struct RepoPolicyMutationPlan {
    path: PathBuf,
    requested_repo: String,
    force: bool,
    allow_existing_same_repo: bool,
    initial: Option<RepoPolicySnapshot>,
}

impl RepoPolicyMutationPlan {
    pub(crate) fn prepare(
        path: &Path,
        requested_repo: &str,
        write_repo_policy: bool,
        force: bool,
        allow_existing_same_repo: bool,
    ) -> Result<Self, QghError> {
        if !write_repo_policy {
            return Ok(Self {
                path: path.to_path_buf(),
                requested_repo: requested_repo.to_string(),
                force,
                allow_existing_same_repo,
                initial: None,
            });
        }

        let initial = inspect_repo_policy(path)?;
        if let RepoPolicySnapshot::Existing { repo, .. } = &initial {
            if !force {
                let repo = repo.clone()?;
                if !allow_existing_same_repo || repo != requested_repo {
                    return Err(repo_policy_exists_error(path, &repo, requested_repo));
                }
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            requested_repo: requested_repo.to_string(),
            force,
            allow_existing_same_repo,
            initial: Some(initial),
        })
    }

    pub(crate) fn commit(self, candidate: &[u8]) -> Result<&'static str, QghError> {
        let Some(initial) = self.initial.clone() else {
            return Ok("skipped");
        };
        let candidate_policy = parse_repo_policy_bytes(&self.path, candidate)?;
        if candidate_policy.repo.full_name() != self.requested_repo {
            return Err(QghError::config(
                "Generated repo policy does not match the requested repository.",
            ));
        }

        match initial {
            RepoPolicySnapshot::Missing => self.commit_after_missing(candidate),
            RepoPolicySnapshot::Existing { bytes, .. } => {
                self.commit_after_existing(&bytes, candidate)
            }
        }
    }

    fn commit_after_missing(self, candidate: &[u8]) -> Result<&'static str, QghError> {
        if publish_new(&self.path, candidate)? {
            return Ok("created");
        }
        let current = inspect_repo_policy(&self.path)?;
        self.resolve_concurrent_entry(current, candidate)
    }

    fn commit_after_existing(
        self,
        expected_bytes: &[u8],
        candidate: &[u8],
    ) -> Result<&'static str, QghError> {
        let current = inspect_repo_policy(&self.path)?;
        match current {
            RepoPolicySnapshot::Missing => {
                if publish_new(&self.path, candidate)? {
                    Ok("created")
                } else {
                    let raced = inspect_repo_policy(&self.path)?;
                    self.resolve_concurrent_entry(raced, candidate)
                }
            }
            RepoPolicySnapshot::Existing { bytes, repo } => {
                if self.force {
                    return Ok(publish_replace(&self.path, candidate)?.action());
                }
                let repo = repo?;
                if bytes != expected_bytes
                    && (!self.allow_existing_same_repo || repo != self.requested_repo)
                {
                    return Err(repo_policy_exists_error(
                        &self.path,
                        &repo,
                        &self.requested_repo,
                    ));
                }
                if self.allow_existing_same_repo && repo == self.requested_repo {
                    Ok("already_exists")
                } else {
                    Err(repo_policy_exists_error(
                        &self.path,
                        &repo,
                        &self.requested_repo,
                    ))
                }
            }
        }
    }

    fn resolve_concurrent_entry(
        self,
        current: RepoPolicySnapshot,
        candidate: &[u8],
    ) -> Result<&'static str, QghError> {
        match current {
            RepoPolicySnapshot::Missing => Err(QghError::storage(
                "Repo policy changed repeatedly during publication.",
            )
            .with_retryable(true)),
            RepoPolicySnapshot::Existing { .. } if self.force => {
                Ok(publish_replace(&self.path, candidate)?.action())
            }
            RepoPolicySnapshot::Existing { repo, .. } => {
                let repo = repo?;
                if self.allow_existing_same_repo && repo == self.requested_repo {
                    Ok("already_exists")
                } else {
                    Err(repo_policy_exists_error(
                        &self.path,
                        &repo,
                        &self.requested_repo,
                    ))
                }
            }
        }
    }
}

fn inspect_repo_policy(path: &Path) -> Result<RepoPolicySnapshot, QghError> {
    let path_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(RepoPolicySnapshot::Missing);
        }
        Err(_) => return Err(QghError::storage("Could not inspect the repo policy.")),
    };
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(QghError::storage(
            "The repo policy must be a regular file during mutation.",
        ));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(platform_private_file_open_flags());
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(RepoPolicySnapshot::Missing);
        }
        Err(_) => return Err(QghError::storage("Could not safely open the repo policy.")),
    };
    validate_open_repo_policy(path, &path_metadata, &file)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| QghError::storage("Could not read the repo policy for mutation."))?;
    let repo = parse_repo_policy_bytes(path, &bytes).map(|policy| policy.repo.full_name());
    Ok(RepoPolicySnapshot::Existing { bytes, repo })
}

fn validate_open_repo_policy(
    path: &Path,
    inspected_metadata: &fs::Metadata,
    file: &File,
) -> Result<(), QghError> {
    let current_metadata = fs::symlink_metadata(path)
        .map_err(|_| QghError::storage("Could not re-inspect the repo policy."))?;
    let file_metadata = file
        .metadata()
        .map_err(|_| QghError::storage("Could not inspect the open repo policy."))?;
    if current_metadata.file_type().is_symlink()
        || !current_metadata.is_file()
        || !file_metadata.is_file()
    {
        return Err(QghError::storage(
            "The repo policy must be a regular file during mutation.",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if inspected_metadata.dev() != file_metadata.dev()
            || inspected_metadata.ino() != file_metadata.ino()
            || current_metadata.dev() != file_metadata.dev()
            || current_metadata.ino() != file_metadata.ino()
        {
            return Err(QghError::storage(
                "The repo policy changed while it was opened for mutation.",
            )
            .with_retryable(true));
        }
    }
    Ok(())
}

fn publish_new(path: &Path, candidate: &[u8]) -> Result<bool, QghError> {
    let staging_path = stage_candidate(path, candidate)?;
    let published = match fs::hard_link(&staging_path, path) {
        Ok(()) => true,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => false,
        Err(_) => {
            let _ = fs::remove_file(&staging_path);
            return Err(QghError::storage("Could not publish the new repo policy."));
        }
    };
    let _ = fs::remove_file(&staging_path);
    if published {
        sync_policy_parent(path)?;
    }
    Ok(published)
}

fn publish_replace(path: &Path, candidate: &[u8]) -> Result<RepoPolicyPublication, QghError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(QghError::storage(
                "The repo policy must be a regular file during mutation.",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return if publish_new(path, candidate)? {
                Ok(RepoPolicyPublication::Created)
            } else {
                Err(
                    QghError::storage("Repo policy changed repeatedly during publication.")
                        .with_retryable(true),
                )
            };
        }
        Err(_) => return Err(QghError::storage("Could not inspect the repo policy.")),
    }

    let staging_path = stage_candidate(path, candidate)?;
    if fs::rename(&staging_path, path).is_err() {
        let _ = fs::remove_file(&staging_path);
        return Err(QghError::storage("Could not replace the repo policy."));
    }
    sync_policy_parent(path)?;
    Ok(RepoPolicyPublication::Overwritten)
}

fn stage_candidate(path: &Path, candidate: &[u8]) -> Result<PathBuf, QghError> {
    let parent = path
        .parent()
        .ok_or_else(|| QghError::storage("The repo policy path has no parent directory."))?;
    loop {
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let staging_path = parent.join(format!(".qgh.toml.{}.{sequence}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        match options.open(&staging_path) {
            Ok(mut file) => {
                let result = file
                    .write_all(candidate)
                    .and_then(|_| file.sync_all())
                    .map_err(|_| QghError::storage("Could not stage the repo policy."));
                if let Err(error) = result {
                    drop(file);
                    let _ = fs::remove_file(&staging_path);
                    return Err(error);
                }
                return Ok(staging_path);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(_) => {
                return Err(QghError::storage("Could not create a staged repo policy."));
            }
        }
    }
}

fn sync_policy_parent(path: &Path) -> Result<(), QghError> {
    let parent = path
        .parent()
        .ok_or_else(|| QghError::storage("The repo policy path has no parent directory."))?;
    sync_directory(parent).map_err(|_| {
        QghError::storage("The repo policy is visible, but directory durability was not confirmed.")
            .with_details(json!({
                "reason": "repo_policy_directory_sync_failed",
                "publication_state": "visible_durability_unconfirmed"
            }))
            .with_hint("Verify .qgh.toml, then rerun qgh init to confirm durability.")
            .with_retryable(true)
    })
}

fn repo_policy_exists_error(path: &Path, existing_repo: &str, requested_repo: &str) -> QghError {
    QghError::validation(
        "config.repo_policy_exists",
        "Repo policy already exists for a different or protected repository.",
    )
    .with_details(json!({
        "path": path.to_string_lossy(),
        "existing_repo": existing_repo,
        "requested_repo": requested_repo
    }))
    .with_hint("Use --force to overwrite the existing .qgh.toml.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_publication_reports_created_when_the_expected_entry_disappeared() {
        let root = std::env::temp_dir().join(format!(
            "qgh-repo-policy-force-fallback-{}-{}",
            std::process::id(),
            STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join(".qgh.toml");
        fs::write(&path, b"expected old policy").unwrap();
        fs::remove_file(&path).unwrap();

        let publication = publish_replace(&path, b"candidate").unwrap();

        assert_eq!(publication, RepoPolicyPublication::Created);
        assert_eq!(fs::read(&path).unwrap(), b"candidate");
        fs::remove_dir_all(root).unwrap();
    }
}
