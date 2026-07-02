//! Socket readiness waiting, used by the notify servers to block until a
//! wake-up connection arrives instead of busy-polling.

use std::net::TcpListener;
use std::time::Duration;

/// Block until `listener` has an incoming connection ready to accept, or
/// `timeout` elapses. Returns true if readable, false on timeout.
///
/// Unix: `poll(POLLIN)`. Windows: `select` over the socket.
pub fn wait_readable(listener: &TcpListener, timeout: Duration) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single valid pollfd, nfds=1, bounded timeout.
        let ret = unsafe { libc::poll(&mut pfd as *mut _, 1, timeout_ms) };
        ret > 0
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{FD_SET, SOCKET, TIMEVAL, select};

        let mut set: FD_SET = unsafe { std::mem::zeroed() };
        set.fd_count = 1;
        set.fd_array[0] = listener.as_raw_socket() as SOCKET;
        let tv = TIMEVAL {
            tv_sec: timeout.as_secs().min(i32::MAX as u64) as i32,
            tv_usec: timeout.subsec_micros() as i32,
        };
        // SAFETY: read set holds one valid socket; nfds is ignored on Windows.
        let ret = unsafe { select(0, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), &tv) };
        ret > 0
    }
}
