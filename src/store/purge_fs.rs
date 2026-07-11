use super::purge_error;
use crate::error::QghError;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AnchoredFileFingerprint {
    pub byte_len: u64,
    pub sha256: String,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FilesystemIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
pub(super) fn filesystem_identity(path: &Path) -> Result<FilesystemIdentity, QghError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path).map_err(|_| purge_error())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(purge_error());
    }
    Ok(FilesystemIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(unix)]
pub(super) fn filesystem_identity_from_file(
    file: &fs::File,
) -> Result<FilesystemIdentity, QghError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata().map_err(|_| purge_error())?;
    if !metadata.is_dir() {
        return Err(purge_error());
    }
    Ok(FilesystemIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(unix)]
impl FilesystemIdentity {
    pub(super) fn durable_key(&self) -> String {
        format!("unix:{}:{}", self.device, self.inode)
    }
}

#[cfg(not(unix))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FilesystemIdentity {
    created: Option<std::time::SystemTime>,
    modified: Option<std::time::SystemTime>,
}

#[cfg(not(unix))]
pub(super) fn filesystem_identity(path: &Path) -> Result<FilesystemIdentity, QghError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| purge_error())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(purge_error());
    }
    Ok(FilesystemIdentity {
        created: metadata.created().ok(),
        modified: metadata.modified().ok(),
    })
}

#[cfg(not(unix))]
pub(super) fn filesystem_identity_from_file(
    file: &fs::File,
) -> Result<FilesystemIdentity, QghError> {
    let metadata = file.metadata().map_err(|_| purge_error())?;
    if !metadata.is_dir() {
        return Err(purge_error());
    }
    Ok(FilesystemIdentity {
        created: metadata.created().ok(),
        modified: metadata.modified().ok(),
    })
}

#[cfg(not(unix))]
impl FilesystemIdentity {
    pub(super) fn durable_key(&self) -> String {
        fn nanos(value: Option<std::time::SystemTime>) -> u128 {
            value
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_nanos())
        }
        format!("portable:{}:{}", nanos(self.created), nanos(self.modified))
    }
}

// FD-anchored purge safety invariants (macOS/Linux):
// - the root directory is opened with O_NOFOLLOW and its device/inode must
//   match the identity captured immediately after the no-replace quarantine
//   rename;
// - fdopendir owns only a dup of a live directory FD, and every readdir name
//   is passed to openat/unlinkat relative to that still-live parent FD;
// - entries are opened with O_NOFOLLOW and their device/inode/type is checked
//   again immediately before unlinkat; symlinks and special files fail closed;
// - the quarantine pathname itself is removed only if it still resolves to
//   the same, now-empty device/inode. A replacement foreign tree is untouched.
// These guarantees assume qgh's local single-user threat model. A same-user
// actor deliberately racing filesystem replacement between the final identity
// check and unlinkat is outside that model; this is not an adversarial
// multi-user filesystem deletion primitive.
// The platform dirent layouts and open/unlink flags below mirror the Darwin
// and Linux C ABIs selected by their cfg guards.
#[cfg(target_os = "macos")]
#[repr(C)]
struct PlatformDirent {
    inode: u64,
    seek_offset: u64,
    record_length: u16,
    name_length: u16,
    entry_type: u8,
    name: [std::os::raw::c_char; 1024],
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct PlatformDirent {
    inode: u64,
    offset: i64,
    record_length: u16,
    entry_type: u8,
    name: [std::os::raw::c_char; 256],
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn open_anchored_directory(path: &Path) -> Result<fs::File, QghError> {
    use std::os::unix::fs::OpenOptionsExt;

    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(platform_open_directory_flags())
        .open(path)
        .map_err(|_| purge_error())?;
    if !directory.metadata().map_err(|_| purge_error())?.is_dir() {
        return Err(purge_error());
    }
    Ok(directory)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn open_anchored_directory(_path: &Path) -> Result<fs::File, QghError> {
    Err(purge_error())
}

#[cfg(target_os = "macos")]
fn platform_open_directory_flags() -> std::os::raw::c_int {
    const O_NOFOLLOW: std::os::raw::c_int = 0x0000_0100;
    const O_DIRECTORY: std::os::raw::c_int = 0x0010_0000;
    const O_CLOEXEC: std::os::raw::c_int = 0x0100_0000;
    O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC
}

#[cfg(target_os = "linux")]
fn platform_open_directory_flags() -> std::os::raw::c_int {
    const O_NOFOLLOW: std::os::raw::c_int = 0o400000;
    const O_DIRECTORY: std::os::raw::c_int = 0o200000;
    const O_CLOEXEC: std::os::raw::c_int = 0o2000000;
    O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC
}

#[cfg(target_os = "macos")]
fn platform_open_entry_flags() -> std::os::raw::c_int {
    const O_NONBLOCK: std::os::raw::c_int = 0x0000_0004;
    const O_NOFOLLOW: std::os::raw::c_int = 0x0000_0100;
    const O_CLOEXEC: std::os::raw::c_int = 0x0100_0000;
    O_NONBLOCK | O_NOFOLLOW | O_CLOEXEC
}

#[cfg(target_os = "linux")]
fn platform_open_entry_flags() -> std::os::raw::c_int {
    const O_NONBLOCK: std::os::raw::c_int = 0o4000;
    const O_NOFOLLOW: std::os::raw::c_int = 0o400000;
    const O_CLOEXEC: std::os::raw::c_int = 0o2000000;
    O_NONBLOCK | O_NOFOLLOW | O_CLOEXEC
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn anchored_file_names(
    directory: &fs::File,
) -> Result<Vec<std::ffi::OsString>, QghError> {
    use std::os::fd::AsRawFd;

    anchored_directory_entry_names(directory.as_raw_fd())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn anchored_file_names(
    _directory: &fs::File,
) -> Result<Vec<std::ffi::OsString>, QghError> {
    Err(purge_error())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn anchored_regular_file_fingerprint(
    directory: &fs::File,
    name: &std::ffi::OsStr,
) -> Result<AnchoredFileFingerprint, QghError> {
    use std::os::fd::AsRawFd;

    let directory_fd = directory.as_raw_fd();
    let mut entry = open_anchored_entry(directory_fd, name)?;
    let metadata = entry.metadata().map_err(|_| purge_error())?;
    if !metadata.is_file() {
        return Err(purge_error());
    }
    let identity = anchored_entry_identity(&metadata);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = entry.read(&mut buffer).map_err(|_| purge_error())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    if current_anchored_entry_identity(directory_fd, name)? != identity {
        return Err(purge_error());
    }
    Ok(AnchoredFileFingerprint {
        byte_len: metadata.len(),
        sha256: format!("{:x}", hasher.finalize()),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn anchored_regular_file_fingerprint(
    _directory: &fs::File,
    _name: &std::ffi::OsStr,
) -> Result<AnchoredFileFingerprint, QghError> {
    Err(purge_error())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn unlink_anchored_regular_file(
    directory: &fs::File,
    name: &std::ffi::OsStr,
    expected: &AnchoredFileFingerprint,
) -> Result<(), QghError> {
    use std::os::fd::AsRawFd;

    let directory_fd = directory.as_raw_fd();
    let mut entry = open_anchored_entry(directory_fd, name)?;
    let metadata = entry.metadata().map_err(|_| purge_error())?;
    if !metadata.is_file() {
        return Err(purge_error());
    }
    let identity = anchored_entry_identity(&metadata);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = entry.read(&mut buffer).map_err(|_| purge_error())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let observed = AnchoredFileFingerprint {
        byte_len: metadata.len(),
        sha256: format!("{:x}", hasher.finalize()),
    };
    if observed != *expected {
        return Err(purge_error());
    }
    if current_anchored_entry_identity(directory_fd, name)? != identity {
        return Err(purge_error());
    }
    unlink_anchored_entry(directory_fd, name, false)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn remove_anchored_empty_directory(
    parent: &fs::File,
    name: &std::ffi::OsStr,
    expected_identity: &FilesystemIdentity,
) -> Result<(), QghError> {
    use std::os::fd::AsRawFd;

    let parent_fd = parent.as_raw_fd();
    let entry = open_anchored_entry(parent_fd, name)?;
    if filesystem_identity_from_file(&entry)? != *expected_identity {
        return Err(purge_error());
    }
    if !anchored_directory_entry_names(entry.as_raw_fd())?.is_empty() {
        return Err(purge_error());
    }
    let metadata = entry.metadata().map_err(|_| purge_error())?;
    let identity = anchored_entry_identity(&metadata);
    if current_anchored_entry_identity(parent_fd, name)? != identity {
        return Err(purge_error());
    }
    unlink_anchored_entry(parent_fd, name, true)?;
    parent.sync_all().map_err(|_| purge_error())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn remove_anchored_empty_directory(
    _parent: &fs::File,
    _name: &std::ffi::OsStr,
    _expected_identity: &FilesystemIdentity,
) -> Result<(), QghError> {
    Err(purge_error())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn unlink_anchored_regular_file(
    _directory: &fs::File,
    _name: &std::ffi::OsStr,
    _expected: &AnchoredFileFingerprint,
) -> Result<(), QghError> {
    Err(purge_error())
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnchoredEntryIdentity {
    device: u64,
    inode: u64,
    file_type: u32,
}

#[cfg(unix)]
fn anchored_entry_identity(metadata: &fs::Metadata) -> AnchoredEntryIdentity {
    use std::os::unix::fs::MetadataExt;

    AnchoredEntryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        file_type: metadata.mode() & 0o170000,
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn current_anchored_entry_identity(
    directory_fd: std::os::raw::c_int,
    name: &std::ffi::OsStr,
) -> Result<AnchoredEntryIdentity, QghError> {
    let entry = open_anchored_entry(directory_fd, name)?;
    let metadata = entry.metadata().map_err(|_| purge_error())?;
    Ok(anchored_entry_identity(&metadata))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn open_anchored_entry(
    directory_fd: std::os::raw::c_int,
    name: &std::ffi::OsStr,
) -> Result<fs::File, QghError> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn openat(
            directory_fd: std::os::raw::c_int,
            path: *const std::os::raw::c_char,
            flags: std::os::raw::c_int,
            ...
        ) -> std::os::raw::c_int;
    }

    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| purge_error())?;
    // SAFETY: `name` is NUL-terminated, `directory_fd` is kept alive by the
    // caller, and no mode argument is required without O_CREAT.
    let fd = unsafe { openat(directory_fd, name.as_ptr(), platform_open_entry_flags()) };
    if fd < 0 {
        return Err(purge_error());
    }
    // SAFETY: `openat` returned a new owned descriptor on success.
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn unlink_anchored_entry(
    directory_fd: std::os::raw::c_int,
    name: &std::ffi::OsStr,
    directory: bool,
) -> Result<(), QghError> {
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn unlinkat(
            directory_fd: std::os::raw::c_int,
            path: *const std::os::raw::c_char,
            flags: std::os::raw::c_int,
        ) -> std::os::raw::c_int;
    }

    #[cfg(target_os = "macos")]
    const AT_REMOVEDIR: std::os::raw::c_int = 0x0080;
    #[cfg(target_os = "linux")]
    const AT_REMOVEDIR: std::os::raw::c_int = 0x0200;
    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| purge_error())?;
    let flags = if directory { AT_REMOVEDIR } else { 0 };
    // SAFETY: `name` is NUL-terminated and `directory_fd` remains open.
    let result = unsafe { unlinkat(directory_fd, name.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(purge_error())
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn anchored_directory_entry_names(
    directory_fd: std::os::raw::c_int,
) -> Result<Vec<std::ffi::OsString>, QghError> {
    use std::os::unix::ffi::OsStringExt;

    unsafe extern "C" {
        fn dup(fd: std::os::raw::c_int) -> std::os::raw::c_int;
        fn close(fd: std::os::raw::c_int) -> std::os::raw::c_int;
        fn lseek(fd: std::os::raw::c_int, offset: i64, whence: std::os::raw::c_int) -> i64;
        fn fdopendir(fd: std::os::raw::c_int) -> *mut std::ffi::c_void;
        fn readdir(directory: *mut std::ffi::c_void) -> *mut PlatformDirent;
        fn closedir(directory: *mut std::ffi::c_void) -> std::os::raw::c_int;
    }

    // SAFETY: `directory_fd` remains owned by the caller. `fdopendir` takes
    // ownership only of the duplicate descriptor.
    let duplicate = unsafe { dup(directory_fd) };
    if duplicate < 0 {
        return Err(purge_error());
    }
    // `dup` shares the directory cursor with the original descriptor. Reset
    // it before every inventory pass so repeated crash-recovery validation
    // cannot mistake an exhausted cursor for an empty directory.
    const SEEK_SET: std::os::raw::c_int = 0;
    if unsafe { lseek(duplicate, 0, SEEK_SET) } < 0 {
        unsafe { close(duplicate) };
        return Err(purge_error());
    }
    // SAFETY: the duplicate is a valid open directory descriptor.
    let stream = unsafe { fdopendir(duplicate) };
    if stream.is_null() {
        // SAFETY: `fdopendir` did not take ownership on failure.
        unsafe { close(duplicate) };
        return Err(purge_error());
    }
    let result = (|| {
        let mut names = Vec::new();
        loop {
            // SAFETY: `stream` is valid until `closedir` below.
            let entry = unsafe { readdir(stream) };
            if entry.is_null() {
                break;
            }
            #[cfg(target_os = "macos")]
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    (*entry).name.as_ptr().cast::<u8>(),
                    usize::from((*entry).name_length),
                )
            };
            #[cfg(target_os = "linux")]
            let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).name.as_ptr()).to_bytes() };
            if bytes == b"." || bytes == b".." {
                continue;
            }
            if bytes.is_empty() || bytes.contains(&b'/') {
                return Err(purge_error());
            }
            names.push(std::ffi::OsString::from_vec(bytes.to_vec()));
        }
        names.sort();
        Ok(names)
    })();
    // SAFETY: `stream` is valid and owns the duplicated descriptor.
    let close_result = unsafe { closedir(stream) };
    if close_result != 0 {
        return Err(purge_error());
    }
    result
}

#[cfg(unix)]
pub(super) fn sync_directory(path: &Path) -> Result<(), QghError> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| purge_error())
}

#[cfg(not(unix))]
pub(super) fn sync_directory(_path: &Path) -> Result<(), QghError> {
    Ok(())
}
