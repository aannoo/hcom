//! Filesystem primitives that differ across platforms: Unix permission bits and
//! stable on-disk file identity.

use std::fs::File;
use std::io;
use std::path::Path;

/// Create a new file restricted to the owner (`0o600` on Unix), failing if it
/// already exists. On Windows the file is created with default ACLs (no Unix
/// mode); profile-local files are already private. This is a no-op gap if
/// `HCOM_DIR`/`HOME` is redirected to a location shared with other accounts —
/// secrets written there won't be owner-restricted on Windows. Real ACL
/// restriction (`SetNamedSecurityInfo`) is deferred to a later batch; this is
/// a deliberate, tracked gap, not an oversight.
pub fn create_private_new(path: &Path) -> io::Result<File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Acquire a blocking exclusive lock on an open file. The lock is released when
/// the file handle is closed (dropped).
///
/// Unix: `flock(LOCK_EX)`, retrying on `EINTR`. Windows: `LockFileEx` with
/// `LOCKFILE_EXCLUSIVE_LOCK` over the whole file.
pub fn lock_exclusive(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        loop {
            // SAFETY: flock on a valid fd; return value is checked.
            let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if ret == 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LockFileEx};
        use windows_sys::Win32::System::IO::OVERLAPPED;

        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        // SAFETY: valid handle for the file's lifetime; whole-file range.
        let ok = unsafe {
            LockFileEx(
                file.as_raw_handle() as HANDLE,
                LOCKFILE_EXCLUSIVE_LOCK,
                0,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Whether `path` is a Unix-domain socket. Always false on Windows, which has
/// no filesystem socket node type.
pub fn is_socket(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(path)
            .map(|m| m.file_type().is_socket())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Restrict a file to owner-only read/write (`0o600` on Unix).
///
/// No-op on Windows, where Unix mode bits do not apply and files created under
/// the user's profile are already private by default. Same caveat as
/// `create_private_new`: a shared `HCOM_DIR`/`HOME` location isn't actually
/// locked down on Windows until real ACL restriction is implemented.
pub fn set_private(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Mark a file as executable (`0o755` on Unix).
///
/// No-op on Windows, where executability is determined by file extension rather
/// than a mode bit.
pub fn set_executable(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Stable identity of a file on disk, used to detect replacement (atomic
/// rename/swap) of a path that keeps the same name.
///
/// Unix: the inode number. Windows: the `nFileIndex` from
/// `GetFileInformationByHandle`. Returns 0 when the file cannot be inspected.
pub fn file_id(path: &Path) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).map(|m| m.ino()).unwrap_or(0)
    }
    #[cfg(windows)]
    {
        file_id_win(path).unwrap_or(0)
    }
}

#[cfg(windows)]
fn file_id_win(path: &Path) -> Option<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let file = std::fs::File::open(path).ok()?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `file` owns a valid handle for the duration of the call and
    // `info` is a properly sized output buffer.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
    if ok == 0 {
        return None;
    }
    Some(((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64)
}
