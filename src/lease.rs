use crate::error::QghError;
use crate::paths::{ensure_private_dir, set_private_file, ProfilePaths};
use fs2::FileExt;
use serde_json::json;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeaseAvailability {
    Available,
    Busy,
}

#[derive(Debug)]
pub(crate) struct FileLease {
    file: File,
}

impl FileLease {
    pub(crate) fn acquire_profile_sync(
        profile_id: &str,
        paths: &ProfilePaths,
    ) -> Result<Self, QghError> {
        ensure_private_dir(&paths.profile_dir)?;
        let path = paths.profile_dir.join("sync.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|_| QghError::storage("Could not open the profile sync lease."))?;
        set_private_file(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self { file }),
            Err(error) if error.kind() == ErrorKind::WouldBlock => Err(sync_busy(profile_id)),
            Err(_) => Err(QghError::storage(
                "Could not acquire the profile sync lease.",
            )),
        }
    }

    pub(crate) fn probe_profile_sync(paths: &ProfilePaths) -> Result<LeaseAvailability, QghError> {
        let path = paths.profile_dir.join("sync.lock");
        if !path.exists() {
            return Ok(LeaseAvailability::Available);
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| QghError::storage("Could not inspect the profile sync lease."))?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = FileExt::unlock(&file);
                Ok(LeaseAvailability::Available)
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(LeaseAvailability::Busy),
            Err(_) => Err(QghError::storage(
                "Could not inspect the profile sync lease.",
            )),
        }
    }

    pub(crate) fn try_acquire_schedule_host(path: &Path) -> Result<Option<Self>, QghError> {
        let Some(parent) = path.parent() else {
            return Err(QghError::storage("Schedule host lease path is invalid."));
        };
        ensure_private_dir(parent)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| QghError::storage("Could not open the schedule host lease."))?;
        set_private_file(path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { file })),
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(_) => Err(QghError::storage(
                "Could not acquire the schedule host lease.",
            )),
        }
    }

    pub(crate) fn acquire_schedule_lifecycle(path: &Path) -> Result<Self, QghError> {
        let Some(parent) = path.parent() else {
            return Err(QghError::new(
                "schedule.storage_failed",
                "Schedule lifecycle lease path is invalid.",
                6,
            ));
        };
        ensure_private_dir(parent)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| {
                QghError::new(
                    "schedule.storage_failed",
                    "Could not open the schedule lifecycle lease.",
                    6,
                )
            })?;
        set_private_file(path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self { file }),
            Err(error) if error.kind() == ErrorKind::WouldBlock => Err(QghError::new(
                "schedule.busy",
                "Another schedule lifecycle operation is already running.",
                5,
            )
            .with_hint("Wait for the active schedule start or stop to finish, then retry.")
            .with_retryable(true)),
            Err(_) => Err(QghError::new(
                "schedule.storage_failed",
                "Could not acquire the schedule lifecycle lease.",
                6,
            )),
        }
    }
}

impl Drop for FileLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn sync_busy(profile_id: &str) -> QghError {
    QghError::new(
        "sync.busy",
        "Another sync already owns this profile writer lease.",
        5,
    )
    .with_details(json!({
        "profile_id": profile_id
    }))
    .with_hint("Wait for the active sync to finish, then retry.")
    .with_retryable(true)
}
