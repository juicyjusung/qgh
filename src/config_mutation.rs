use crate::error::QghError;
use crate::paths::ensure_private_dir;
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const CONFIG_MUTATION_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const CONFIG_MUTATION_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(25);

pub(crate) struct ConfigMutation {
    config_path: PathBuf,
    durability_directories: Vec<PathBuf>,
    lock_file: File,
}

impl ConfigMutation {
    pub(crate) fn begin(config_path: PathBuf) -> Result<Self, QghError> {
        Self::begin_with_timeout(config_path, CONFIG_MUTATION_LOCK_TIMEOUT)
    }

    fn begin_with_timeout(config_path: PathBuf, timeout: Duration) -> Result<Self, QghError> {
        Self::begin_with_open_hook(config_path, timeout, || {})
    }

    fn begin_with_open_hook(
        config_path: PathBuf,
        timeout: Duration,
        after_lock_open: impl FnOnce(),
    ) -> Result<Self, QghError> {
        let parent = config_path
            .parent()
            .ok_or_else(|| QghError::storage("The profile config path has no parent directory."))?;
        ensure_private_dir(parent)?;
        let canonical_parent = fs::canonicalize(parent)
            .map_err(|_| QghError::storage("Could not resolve the profile config directory."))?;
        let durability_directories = directory_ancestry(&canonical_parent);
        let file_name = config_path
            .file_name()
            .ok_or_else(|| QghError::storage("The profile config path has no file name."))?;
        let config_path = canonical_parent.join(file_name);
        let lock_path = canonical_parent.join("config.toml.lock");
        let lock_file = open_private_lock_file(&lock_path)?;
        after_lock_open();
        acquire_lock_with_timeout(&lock_file, timeout)?;
        validate_open_lock_file(&lock_path, &lock_file)?;
        validate_mutation_target(&config_path)?;
        Ok(Self {
            config_path,
            durability_directories,
            lock_file,
        })
    }

    pub(crate) fn read_optional(&self) -> Result<Option<Vec<u8>>, QghError> {
        validate_mutation_target(&self.config_path)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(platform_private_file_open_flags());
        }
        let mut file = match options.open(&self.config_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(QghError::storage(
                    "Could not safely open the profile config for mutation.",
                ));
            }
        };
        validate_open_regular_file(&self.config_path, &file)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|_| QghError::storage("Could not read the profile config for mutation."))?;
        Ok(Some(bytes))
    }

    pub(crate) fn commit(&self, bytes: &[u8]) -> Result<(), QghError> {
        self.commit_with_pre_publish(bytes, |_| Ok(()))
    }

    fn commit_with_pre_publish(
        &self,
        bytes: &[u8],
        before_publish: impl FnOnce(&Path) -> Result<(), QghError>,
    ) -> Result<(), QghError> {
        self.commit_with_hooks(bytes, before_publish, sync_directory)
    }

    fn commit_with_hooks(
        &self,
        bytes: &[u8],
        before_publish: impl FnOnce(&Path) -> Result<(), QghError>,
        mut sync_parent: impl FnMut(&Path) -> std::io::Result<()>,
    ) -> Result<(), QghError> {
        validate_mutation_target(&self.config_path)?;
        let parent = self
            .config_path
            .parent()
            .ok_or_else(|| QghError::storage("The profile config path has no parent directory."))?;
        let (staging_path, mut staging_file) = create_private_staging_file(parent)?;
        let mut published = false;
        let result = (|| -> Result<(), QghError> {
            staging_file
                .write_all(bytes)
                .map_err(|_| QghError::storage("Could not write the staged profile config."))?;
            staging_file
                .sync_all()
                .map_err(|_| QghError::storage("Could not sync the staged profile config."))?;
            before_publish(&staging_path)?;
            validate_mutation_target(&self.config_path)?;
            drop(staging_file);
            fs::rename(&staging_path, &self.config_path)
                .map_err(|_| QghError::storage("Could not publish the profile config."))?;
            published = true;
            for directory in &self.durability_directories {
                sync_parent(directory).map_err(|_| config_directory_sync_error())?;
            }
            Ok(())
        })();
        if result.is_err() && !published {
            let _ = fs::remove_file(&staging_path);
        }
        result
    }
}

fn directory_ancestry(path: &Path) -> Vec<PathBuf> {
    let mut current = path.to_path_buf();
    let mut directories = Vec::new();
    loop {
        directories.push(current.clone());
        if !current.pop() {
            break;
        }
    }
    directories
}

fn config_directory_sync_error() -> QghError {
    QghError::storage(
        "The profile config was replaced, but directory durability was not confirmed.",
    )
    .with_details(serde_json::json!({
        "reason": "config_directory_sync_failed",
        "publication_state": "visible_durability_unconfirmed"
    }))
    .with_hint("Verify the visible config, then rerun qgh init to confirm directory durability.")
    .with_retryable(true)
}

fn acquire_lock_with_timeout(file: &File, timeout: Duration) -> Result<(), QghError> {
    let started = Instant::now();
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                let elapsed = started.elapsed();
                if elapsed >= timeout {
                    return Err(QghError::new(
                        "config.busy",
                        "Another qgh process is updating the profile config.",
                        5,
                    )
                    .with_hint("Wait for the active qgh init operation to finish, then retry.")
                    .with_retryable(true));
                }
                std::thread::sleep(
                    CONFIG_MUTATION_LOCK_POLL_INTERVAL.min(timeout.saturating_sub(elapsed)),
                );
            }
            Err(_) => {
                return Err(QghError::storage(
                    "Could not acquire the profile config mutation lock.",
                ));
            }
        }
    }
}

impl Drop for ConfigMutation {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock_file);
    }
}

fn validate_mutation_target(path: &Path) -> Result<(), QghError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(QghError::storage(
            "The profile config must not be a symbolic link during mutation.",
        )),
        Ok(metadata) if !metadata.is_file() => Err(QghError::storage(
            "The profile config must be a regular file during mutation.",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn open_private_lock_file(path: &Path) -> Result<File, QghError> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(platform_private_file_open_flags());
    }
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(QghError::storage(
            "The profile config mutation lock must not be a symbolic link.",
        ));
    }
    let file = options
        .open(path)
        .map_err(|_| QghError::storage("Could not open the profile config mutation lock."))?;
    validate_open_lock_file(path, &file)?;
    tighten_private_file(&file)?;
    Ok(file)
}

fn validate_open_lock_file(path: &Path, file: &File) -> Result<(), QghError> {
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|_| QghError::storage("Could not inspect the profile config mutation lock."))?;
    let file_metadata = file
        .metadata()
        .map_err(|_| QghError::storage("Could not inspect the profile config mutation lock."))?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || !file_metadata.is_file()
    {
        return Err(QghError::storage(
            "The profile config mutation lock must be a regular file.",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if path_metadata.dev() != file_metadata.dev()
            || path_metadata.ino() != file_metadata.ino()
            || file_metadata.nlink() != 1
        {
            return Err(QghError::storage(
                "The profile config mutation lock changed while it was opened.",
            ));
        }
    }
    Ok(())
}

fn validate_open_regular_file(path: &Path, file: &File) -> Result<(), QghError> {
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|_| QghError::storage("Could not inspect the profile config."))?;
    let file_metadata = file
        .metadata()
        .map_err(|_| QghError::storage("Could not inspect the profile config."))?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || !file_metadata.is_file()
    {
        return Err(QghError::storage(
            "The profile config must be a regular file during mutation.",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(QghError::storage(
                "The profile config changed while it was opened for mutation.",
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_private_file_open_flags() -> std::os::raw::c_int {
    const O_NONBLOCK: std::os::raw::c_int = 0x0000_0004;
    const O_NOFOLLOW: std::os::raw::c_int = 0x0000_0100;
    const O_CLOEXEC: std::os::raw::c_int = 0x0100_0000;
    O_NONBLOCK | O_NOFOLLOW | O_CLOEXEC
}

#[cfg(target_os = "linux")]
fn platform_private_file_open_flags() -> std::os::raw::c_int {
    const O_NONBLOCK: std::os::raw::c_int = 0o4000;
    const O_NOFOLLOW: std::os::raw::c_int = 0o400000;
    const O_CLOEXEC: std::os::raw::c_int = 0o2000000;
    O_NONBLOCK | O_NOFOLLOW | O_CLOEXEC
}

fn create_private_staging_file(parent: &Path) -> Result<(PathBuf, File), QghError> {
    loop {
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".config.toml.{}.{sequence}.tmp",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => {
                if let Err(error) = tighten_private_file(&file) {
                    drop(file);
                    let _ = fs::remove_file(&path);
                    return Err(error);
                }
                return Ok((path, file));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                return Err(QghError::storage(
                    "Could not create the staged profile config.",
                ));
            }
        }
    }
}

#[cfg(unix)]
fn tighten_private_file(file: &File) -> Result<(), QghError> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = file.metadata()?.permissions();
    permissions.set_mode(0o600);
    file.set_permissions(permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn tighten_private_file(_file: &File) -> Result<(), QghError> {
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn second_mutation_waits_then_observes_the_first_commit() {
        let root = temp_root("serialized-writers");
        let config_path = root.join("qgh/config.toml");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::write(&config_path, b"original").unwrap();
        let first = ConfigMutation::begin(config_path.clone()).unwrap();
        let (lock_open_tx, lock_open_rx) = mpsc::channel();
        let (observed_tx, observed_rx) = mpsc::channel();

        let second_path = config_path.clone();
        let second = thread::spawn(move || {
            let mutation = ConfigMutation::begin_with_open_hook(
                second_path.clone(),
                Duration::from_secs(5),
                || lock_open_tx.send(()).unwrap(),
            )
            .unwrap();
            let mut observed = mutation.read_optional().unwrap().unwrap();
            observed_tx.send(observed.clone()).unwrap();
            observed.extend_from_slice(b"+second");
            mutation.commit(&observed).unwrap();
        });

        lock_open_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(observed_rx
            .recv_timeout(Duration::from_millis(250))
            .is_err());
        first.commit(b"first").unwrap();
        drop(first);
        assert_eq!(
            observed_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            b"first"
        );
        second.join().unwrap();
        assert_eq!(fs::read(&config_path).unwrap(), b"first+second");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failure_before_publish_preserves_original_and_removes_staging_file() {
        let root = temp_root("pre-publish-failure");
        let config_path = root.join("qgh/config.toml");
        let parent = config_path.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        fs::write(&config_path, b"original").unwrap();
        let mutation = ConfigMutation::begin(config_path.clone()).unwrap();

        let error = mutation
            .commit_with_pre_publish(b"replacement", |_| {
                Err(QghError::storage("Fixture failure before publish."))
            })
            .unwrap_err();

        assert_eq!(error.code, "storage.failure");
        assert_eq!(fs::read(&config_path).unwrap(), b"original");
        let staging_files = fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect::<Vec<_>>();
        assert!(staging_files.is_empty(), "staging files: {staging_files:?}");

        drop(mutation);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn mutation_rejects_a_symlinked_lock_without_touching_its_target() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = temp_root("symlinked-lock");
        let parent = root.join("qgh");
        let config_path = parent.join("config.toml");
        fs::create_dir_all(&parent).unwrap();
        let target = root.join("lock-target");
        fs::write(&target, b"sentinel").unwrap();
        let mut permissions = fs::metadata(&target).unwrap().permissions();
        permissions.set_mode(0o640);
        fs::set_permissions(&target, permissions).unwrap();
        symlink(&target, parent.join("config.toml.lock")).unwrap();

        let result = ConfigMutation::begin(config_path);

        assert!(result.is_err());
        assert_eq!(result.err().unwrap().code, "storage.failure");
        assert_eq!(fs::read(&target).unwrap(), b"sentinel");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn mutation_read_rejects_a_config_entry_swapped_to_a_symlink() {
        use std::os::unix::fs::symlink;

        let root = temp_root("config-read-symlink-swap");
        let parent = root.join("qgh");
        let config_path = parent.join("config.toml");
        fs::create_dir_all(&parent).unwrap();
        fs::write(&config_path, b"original").unwrap();
        let mutation = ConfigMutation::begin(config_path.clone()).unwrap();
        let target = root.join("outside-config");
        fs::write(&target, b"outside").unwrap();
        fs::remove_file(&config_path).unwrap();
        symlink(&target, &config_path).unwrap();

        let result = mutation.read_optional();

        assert!(result.is_err());
        assert_eq!(result.err().unwrap().code, "storage.failure");
        assert_eq!(fs::read(&target).unwrap(), b"outside");

        drop(mutation);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mutation_lock_timeout_is_structured_and_retryable() {
        let root = temp_root("lock-timeout");
        let config_path = root.join("qgh/config.toml");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let first = ConfigMutation::begin(config_path.clone()).unwrap();

        let result = ConfigMutation::begin_with_timeout(config_path, Duration::from_millis(25));

        assert!(result.is_err());
        let error = result.err().unwrap();
        assert_eq!(error.code, "config.busy");
        assert_eq!(error.exit_code, 5);
        assert!(error.retryable);

        drop(first);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mutation_rejects_a_non_regular_config_entry() {
        let root = temp_root("non-regular-config");
        let config_path = root.join("qgh/config.toml");
        fs::create_dir_all(&config_path).unwrap();

        let result = ConfigMutation::begin(config_path);

        assert!(result.is_err());
        assert_eq!(result.err().unwrap().code, "storage.failure");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mutation_rejects_a_lock_entry_replaced_while_waiting() {
        let root = temp_root("lock-replaced-while-waiting");
        let parent = root.join("qgh");
        let config_path = parent.join("config.toml");
        fs::create_dir_all(&parent).unwrap();
        let lock_path = parent.join("config.toml.lock");
        let holder = open_private_lock_file(&lock_path).unwrap();
        holder.lock_exclusive().unwrap();
        let (opened_tx, opened_rx) = mpsc::channel();

        let waiter_path = config_path.clone();
        let waiter = thread::spawn(move || {
            ConfigMutation::begin_with_open_hook(waiter_path, Duration::from_secs(5), || {
                opened_tx.send(()).unwrap()
            })
        });

        opened_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        fs::rename(&lock_path, parent.join("retired.lock")).unwrap();
        let replacement = open_private_lock_file(&lock_path).unwrap();
        FileExt::unlock(&holder).unwrap();

        let result = waiter.join().unwrap();
        assert!(result.is_err());
        assert_eq!(result.err().unwrap().code, "storage.failure");

        drop(replacement);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn directory_sync_failure_leaves_a_complete_published_file_without_staging() {
        let root = temp_root("directory-sync-failure");
        let config_path = root.join("qgh/config.toml");
        let parent = config_path.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        fs::write(&config_path, b"original").unwrap();
        let mutation = ConfigMutation::begin(config_path.clone()).unwrap();

        let error = mutation
            .commit_with_hooks(
                b"complete replacement",
                |_| Ok(()),
                |_| Err(std::io::Error::other("Fixture directory sync failure.")),
            )
            .unwrap_err();

        assert_eq!(error.code, "storage.failure");
        assert_eq!(error.details["reason"], "config_directory_sync_failed");
        assert_eq!(
            error.details["publication_state"],
            "visible_durability_unconfirmed"
        );
        assert!(error.retryable);
        assert!(error.hint.is_some());
        assert_eq!(fs::read(&config_path).unwrap(), b"complete replacement");
        let staging_files = fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect::<Vec<_>>();
        assert!(staging_files.is_empty(), "staging files: {staging_files:?}");

        drop(mutation);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn newly_created_config_directory_entries_are_synced_after_publication() {
        let root = temp_root("new-directory-durability");
        let config_path = root.join("xdg/qgh/config.toml");
        let interrupted = ConfigMutation::begin(config_path.clone()).unwrap();
        drop(interrupted);
        let mutation = ConfigMutation::begin(config_path.clone()).unwrap();
        let synced = RefCell::new(Vec::<PathBuf>::new());

        mutation
            .commit_with_hooks(
                b"config",
                |_| Ok(()),
                |path| {
                    synced.borrow_mut().push(path.to_path_buf());
                    Ok(())
                },
            )
            .unwrap();

        let config_dir = fs::canonicalize(config_path.parent().unwrap()).unwrap();
        let mut expected = Vec::new();
        let mut current = config_dir;
        loop {
            expected.push(current.clone());
            if !current.pop() {
                break;
            }
        }
        assert_eq!(synced.into_inner(), expected);

        drop(mutation);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn post_publish_failure_does_not_delete_a_new_owner_of_the_staging_path() {
        let root = temp_root("staging-path-reused");
        let config_path = root.join("qgh/config.toml");
        let parent = config_path.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        fs::write(&config_path, b"original").unwrap();
        let mutation = ConfigMutation::begin(config_path.clone()).unwrap();
        let staging_path = RefCell::new(None::<PathBuf>);

        let error = mutation
            .commit_with_hooks(
                b"complete replacement",
                |path| {
                    staging_path.replace(Some(path.to_path_buf()));
                    Ok(())
                },
                |_| {
                    let path = staging_path
                        .borrow()
                        .as_ref()
                        .expect("pre-publish hook captures the staging path")
                        .clone();
                    fs::write(path, b"new owner").unwrap();
                    Err(std::io::Error::other("Fixture directory sync failure."))
                },
            )
            .unwrap_err();

        assert_eq!(error.code, "storage.failure");
        assert_eq!(fs::read(&config_path).unwrap(), b"complete replacement");
        let reused_path = staging_path.into_inner().unwrap();
        assert_eq!(fs::read(&reused_path).unwrap(), b"new owner");

        drop(mutation);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_reader_observes_only_complete_config_versions() {
        let root = temp_root("atomic-reader");
        let config_path = root.join("qgh/config.toml");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let original = vec![b'a'; 256 * 1024];
        let replacement = vec![b'b'; 256 * 1024];
        fs::write(&config_path, &original).unwrap();
        let mutation = ConfigMutation::begin(config_path.clone()).unwrap();
        let (observed_original_tx, observed_original_rx) = mpsc::channel();

        let reader_path = config_path.clone();
        let reader_original = original.clone();
        let reader_replacement = replacement.clone();
        let reader = thread::spawn(move || {
            let mut reported_original = false;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let bytes = fs::read(&reader_path).unwrap();
                assert!(
                    bytes == reader_original || bytes == reader_replacement,
                    "reader observed a partial config publication"
                );
                if bytes == reader_original && !reported_original {
                    observed_original_tx.send(()).unwrap();
                    reported_original = true;
                }
                if bytes == reader_replacement {
                    break;
                }
                assert!(Instant::now() < deadline, "replacement was not observed");
            }
        });

        observed_original_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        mutation.commit(&replacement).unwrap();
        reader.join().unwrap();

        drop(mutation);
        let _ = fs::remove_dir_all(root);
    }

    fn temp_root(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("qgh-config-mutation-{name}-{nonce}"))
    }
}
