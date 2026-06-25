//! Filesystem primitives that differ across platforms: Unix permission bits and
//! stable on-disk file identity.

use std::io;
use std::path::Path;

/// Restrict a file to owner-only read/write (`0o600` on Unix).
///
/// No-op on Windows, where Unix mode bits do not apply and files created under
/// the user's profile are already private by default.
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
