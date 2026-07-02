//! Low-level stdio inspection used for orphan detection.

/// Whether this process's stdin appears broken/invalid (a heuristic for
/// detecting that the launching parent is gone).
///
/// Unix: a non-blocking `poll` of fd 0 that reports `POLLERR`/`POLLNVAL` (but
/// *not* `POLLHUP`, which is the normal end of a piped payload). Windows: always
/// false — pipe close semantics differ and orphan detection there relies on
/// other signals.
pub fn stdin_appears_broken() -> bool {
    #[cfg(unix)]
    {
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single valid pollfd, nfds=1, timeout=0 (non-blocking).
        let ret = unsafe { libc::poll(&mut pfd as *mut _, 1, 0) };
        if ret < 0 {
            return true;
        }
        (pfd.revents & (libc::POLLERR | libc::POLLNVAL)) != 0
    }
    #[cfg(not(unix))]
    {
        false
    }
}
