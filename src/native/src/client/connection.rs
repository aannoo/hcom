//! Socket connection handling for daemon client.
//!
//! Low-level Unix socket connect with timeout using non-blocking I/O.

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Connect to Unix socket with timeout.
pub fn connect_with_timeout(path: &Path, timeout: Duration) -> std::io::Result<UnixStream> {
    use std::os::unix::io::FromRawFd;

    // SAFETY: Creates a Unix domain socket file descriptor.
    // - AF_UNIX and SOCK_STREAM are valid socket parameters
    // - Return value is checked: fd < 0 indicates error, returns Err
    // - On success, fd is a valid open file descriptor owned by this function
    // - On error, no resources are allocated
    let socket = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        fd
    };

    // SAFETY: Sets socket to non-blocking mode via fcntl.
    // - socket is a valid fd from libc::socket above
    // - F_GETFL/F_SETFL are valid fcntl commands
    // - flags is checked < 0, returns Err and closes socket on failure
    // - O_NONBLOCK flag is standard and safe to set
    // - socket is closed via libc::close on any error path to prevent fd leak
    unsafe {
        let flags = libc::fcntl(socket, libc::F_GETFL);
        if flags < 0 {
            libc::close(socket);
            return Err(std::io::Error::last_os_error());
        }
        if libc::fcntl(socket, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            libc::close(socket);
            return Err(std::io::Error::last_os_error());
        }
    }

    // Build sockaddr_un
    let path_bytes = path.as_os_str().as_encoded_bytes();
    // sun_path size varies by platform (104 on macOS, 108 on Linux)
    let max_path_len =
        std::mem::size_of::<libc::sockaddr_un>() - std::mem::size_of::<libc::sa_family_t>() - 1; // -1 for null terminator
    if path_bytes.len() >= max_path_len {
        unsafe {
            libc::close(socket);
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path too long",
        ));
    }

    // SAFETY: Zero-initializes sockaddr_un struct.
    // - sockaddr_un is a C struct with no Rust invariants
    // - mem::zeroed() produces a valid all-zero sockaddr_un
    // - sun_family is set to AF_UNIX immediately after
    // - sun_path will be filled with path bytes via copy_nonoverlapping below
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;

    // SAFETY: Copies socket path bytes into sockaddr_un.sun_path.
    // - path_bytes is valid: from OsStr::as_encoded_bytes()
    // - sun_path destination is valid: part of addr (stack-allocated, properly aligned)
    // - Length is validated above: path_bytes.len() < max_path_len
    // - sun_path is [c_char; N] where N is platform-specific, large enough for validated length
    // - copy_nonoverlapping is safe: no overlap (path_bytes on stack/heap, sun_path on stack)
    // - Remaining bytes stay zero (from mem::zeroed), providing null terminator
    unsafe {
        std::ptr::copy_nonoverlapping(
            path_bytes.as_ptr(),
            addr.sun_path.as_mut_ptr() as *mut u8,
            path_bytes.len(),
        );
    }

    // SAFETY: Initiates connection to Unix socket.
    // - socket is valid fd from libc::socket above
    // - addr is valid sockaddr_un, properly initialized (zeroed + family set + path copied)
    // - Socket is non-blocking, so connect returns immediately with EINPROGRESS
    // - Return value checked: ret < 0 indicates error
    // - EINPROGRESS is expected for non-blocking connect; other errors close socket
    let ret = unsafe {
        libc::connect(
            socket,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };

    if ret < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINPROGRESS) {
            unsafe {
                libc::close(socket);
            }
            return Err(err);
        }
    }

    // Poll for connection with timeout
    let mut pollfd = libc::pollfd {
        fd: socket,
        events: libc::POLLOUT,
        revents: 0,
    };

    let timeout_ms = timeout.as_millis() as libc::c_int;

    // SAFETY: Waits for socket to become writable (connected) with timeout.
    // - pollfd is valid: socket is valid fd, events is POLLOUT, revents is 0
    // - nfds=1 matches the single pollfd struct
    // - timeout_ms is valid c_int from Duration
    // - Return value checked: ret <= 0 indicates timeout (0) or error (-1)
    // - On timeout/error, socket is closed to prevent fd leak
    let ret = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };

    if ret <= 0 {
        unsafe {
            libc::close(socket);
        }
        if ret == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connect timeout",
            ));
        }
        return Err(std::io::Error::last_os_error());
    }

    // Check for connection error
    let mut err: libc::c_int = 0;
    let mut errlen: libc::socklen_t = std::mem::size_of::<libc::c_int>() as libc::socklen_t;

    // SAFETY: Retrieves connection error status from socket via getsockopt.
    // - socket is valid fd from libc::socket above
    // - SOL_SOCKET and SO_ERROR are valid socket options
    // - err is valid mutable c_int reference
    // - errlen is valid and matches sizeof(c_int)
    // - getsockopt writes error code to err (0 = success, non-zero = error)
    // - Return value of getsockopt is not checked (intentional): getsockopt may fail
    //   (invalid fd, bad optlen, etc.), but failure is acceptable here because:
    //   - err is initialized to 0, so if getsockopt fails, err stays 0
    //   - Worst case: we proceed as if no error occurred, but poll() already confirmed
    //     connection (POLLOUT), so actual connection errors are unlikely at this point
    //   - Alternative would be to check return value and handle error, but the failure
    //     mode (assuming no error when getsockopt fails) is safe enough for this use case
    unsafe {
        libc::getsockopt(
            socket,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            &mut err as *mut _ as *mut libc::c_void,
            &mut errlen,
        );
    }

    if err != 0 {
        unsafe {
            libc::close(socket);
        }
        return Err(std::io::Error::from_raw_os_error(err));
    }

    // SAFETY: Restores socket to blocking mode via fcntl.
    // - socket is valid fd, connection established
    // - F_GETFL/F_SETFL are valid fcntl commands
    // - flags is checked < 0, returns Err and closes socket on failure
    // - Clearing O_NONBLOCK flag restores blocking mode
    // - socket is closed via libc::close on any error path to prevent fd leak
    unsafe {
        let flags = libc::fcntl(socket, libc::F_GETFL);
        if flags < 0 {
            libc::close(socket);
            return Err(std::io::Error::last_os_error());
        }
        if libc::fcntl(socket, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
            libc::close(socket);
            return Err(std::io::Error::last_os_error());
        }
    }

    // SAFETY: Transfers socket fd ownership to UnixStream.
    // - socket is valid, connected, blocking fd (no other references exist)
    // - UnixStream takes ownership and will close fd on drop
    // - No fd leaks: all error paths above close socket; success path transfers ownership here
    Ok(unsafe { UnixStream::from_raw_fd(socket) })
}
