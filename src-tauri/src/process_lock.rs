//! Small cross-process file lock shared by app-owned installers.
//!
//! The lock file remains on disk and the operating system owns lock release
//! when the guard is dropped or a process exits. Paths are opened without
//! following symlinks/reparse points so a user-writable app-data directory
//! cannot redirect lock acquisition outside its intended parent.

use std::fs::{self, File, OpenOptions};
use std::path::Path;

#[cfg(windows)]
use std::time::Duration;

pub(crate) struct CrossProcessFileLock {
    _file: File,
}

impl Drop for CrossProcessFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            // SAFETY: `file` owns a valid descriptor for this guard's entire
            // lifetime. Unlocking during Drop cannot outlive that descriptor.
            let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn validate_lock_path(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(format!(
            "Installer lock {} is not a regular file",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "Failed to inspect installer lock {}: {error}",
            path.display()
        )),
    }
}

#[cfg(unix)]
pub(crate) fn acquire_cross_process_lock(path: &Path) -> Result<CrossProcessFileLock, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    validate_lock_path(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("Failed to open installer lock {}: {error}", path.display()))?;
    loop {
        // SAFETY: `file` owns a valid descriptor. `flock` does not retain the
        // pointer or access Rust memory; the descriptor stays open in guard.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(CrossProcessFileLock { _file: file });
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(format!(
                "Failed to acquire installer lock {}: {error}",
                path.display()
            ));
        }
    }
}

#[cfg(unix)]
pub(crate) fn try_acquire_cross_process_lock(
    path: &Path,
) -> Result<Option<CrossProcessFileLock>, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    validate_lock_path(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("Failed to open installer lock {}: {error}", path.display()))?;
    // SAFETY: `file` owns a valid descriptor and remains alive in the returned guard.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(Some(CrossProcessFileLock { _file: file }));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(None);
    }
    Err(format!(
        "Failed to probe installer lock {}: {error}",
        path.display()
    ))
}

#[cfg(windows)]
pub(crate) fn acquire_cross_process_lock(path: &Path) -> Result<CrossProcessFileLock, String> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    validate_lock_path(path)?;
    loop {
        let result = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path);
        match result {
            Ok(file) => return Ok(CrossProcessFileLock { _file: file }),
            Err(error) if matches!(error.raw_os_error(), Some(32) | Some(33)) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                return Err(format!(
                    "Failed to acquire installer lock {}: {error}",
                    path.display()
                ))
            }
        }
    }
}

#[cfg(windows)]
pub(crate) fn try_acquire_cross_process_lock(
    path: &Path,
) -> Result<Option<CrossProcessFileLock>, String> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    validate_lock_path(path)?;
    let result = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path);
    match result {
        Ok(file) => Ok(Some(CrossProcessFileLock { _file: file })),
        Err(error) if matches!(error.raw_os_error(), Some(32) | Some(33)) => Ok(None),
        Err(error) => Err(format!(
            "Failed to probe installer lock {}: {error}",
            path.display()
        )),
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn acquire_cross_process_lock(path: &Path) -> Result<CrossProcessFileLock, String> {
    validate_lock_path(path)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
        .map(|file| CrossProcessFileLock { _file: file })
        .map_err(|error| format!("Failed to open installer lock {}: {error}", path.display()))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn try_acquire_cross_process_lock(
    path: &Path,
) -> Result<Option<CrossProcessFileLock>, String> {
    acquire_cross_process_lock(path).map(Some)
}
