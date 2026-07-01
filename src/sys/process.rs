//! Process-control primitives: liveness, termination, and session/group setup.
//!
//! Unix uses `libc`/`nix` signals and `setsid`; Windows uses the Win32 process
//! and job-object APIs. See the module-level docs in [`crate::sys`].

use std::process::Command;

/// Whether a process with the given PID is currently alive.
///
/// Unix: `kill(pid, 0)`, treating `EPERM` (the process exists but is owned by
/// another user) as alive. Windows: `OpenProcess` followed by
/// `GetExitCodeProcess` — a live handle alone isn't proof of life, since the
/// process object stays valid as long as any handle (ours, or a job object's)
/// is open even after the process has exited. A failure to open with
/// `ERROR_ACCESS_DENIED` also means the process exists.
pub fn is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: kill(pid, 0) sends no signal; it only checks for existence.
        let ret = unsafe { libc::kill(pid as i32, 0) };
        if ret == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            CloseHandle, ERROR_ACCESS_DENIED, GetLastError, STILL_ACTIVE,
        };
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: query-only access mask; the handle is closed before returning.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if !handle.is_null() {
                let mut exit_code = 0u32;
                let ok = GetExitCodeProcess(handle, &mut exit_code) != 0;
                CloseHandle(handle);
                return ok && exit_code == STILL_ACTIVE as u32;
            }
            GetLastError() == ERROR_ACCESS_DENIED
        }
    }
}

/// Forcefully terminate a process by PID. Best-effort; returns whether the
/// request was delivered.
///
/// Unix: `SIGKILL`. Windows: `TerminateProcess`.
pub fn kill(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: standard kill(2) with a valid signal number.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) == 0 }
    }
    #[cfg(windows)]
    {
        terminate_win(pid)
    }
}

/// Request graceful termination of a process by PID. Best-effort; returns
/// whether the request was delivered. Like Unix `SIGTERM`, this only *asks* the
/// process to exit — it does not force it. Callers that need a guarantee must
/// wait and then escalate to [`kill`].
///
/// Unix: `SIGTERM`. Windows: `CTRL_BREAK_EVENT` to the target's process group
/// (see [`request_shutdown_win`]), which mirrors the SIGTERM-sets-a-flag model.
pub fn terminate(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: standard kill(2) with a valid signal number.
        unsafe { libc::kill(pid as i32, libc::SIGTERM) == 0 }
    }
    #[cfg(windows)]
    {
        request_shutdown_win(pid)
    }
}

/// Outcome of signalling a process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupSignal {
    /// The signal was delivered.
    Sent,
    /// No such process/group exists (already gone).
    NotFound,
    /// The caller lacks permission to signal the group.
    PermissionDenied,
    /// Any other failure.
    Other,
}

/// Send a graceful termination request to a process group by PID.
///
/// Unix: `killpg(SIGTERM)`. Windows has no process-group signal and no graceful
/// per-tree signal for an unrelated process, so this maps to a forceful
/// process-tree termination ([`kill_tree_win`]) — the same as [`kill_group`].
pub fn terminate_group(pid: u32) -> GroupSignal {
    #[cfg(unix)]
    {
        signal_group_unix(pid, libc::SIGTERM)
    }
    #[cfg(windows)]
    {
        kill_tree_win(pid)
    }
}

/// Forcefully kill a process group by PID.
///
/// Unix: `killpg(SIGKILL)`. Windows has no process groups in the POSIX sense, so
/// this terminates the target PID **and all of its descendants** via a process
/// snapshot ([`kill_tree_win`]). This matters because hcom records the PID of
/// the launcher (e.g. the background `powershell` host), and the real agent runs
/// as its child; killing only the recorded PID would orphan the agent.
pub fn kill_group(pid: u32) -> GroupSignal {
    #[cfg(unix)]
    {
        signal_group_unix(pid, libc::SIGKILL)
    }
    #[cfg(windows)]
    {
        kill_tree_win(pid)
    }
}

#[cfg(unix)]
fn signal_group_unix(pid: u32, sig: libc::c_int) -> GroupSignal {
    // SAFETY: killpg with a valid signal number; return value is checked.
    let ret = unsafe { libc::killpg(pid as i32, sig) };
    if ret == 0 {
        return GroupSignal::Sent;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => GroupSignal::NotFound,
        Some(libc::EPERM) => GroupSignal::PermissionDenied,
        _ => GroupSignal::Other,
    }
}

/// Replace the current process with the given command.
///
/// Unix uses `exec()` and only returns (an error) on failure. Windows has no
/// `exec`, so it spawns the command, waits, and exits with the child's status
/// code — likewise not returning on success.
pub fn exec_replace(mut cmd: Command) -> std::io::Error {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.exec()
    }
    #[cfg(windows)]
    {
        match cmd.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(e) => e,
        }
    }
}

/// Kill a child process together with its process group.
///
/// Unix: `killpg(SIGKILL)` on the child's group (set up via [`detach_session`]),
/// falling back to `Child::kill` if the group signal fails. Windows: terminates
/// the child's whole process tree ([`kill_tree_win`]).
pub fn kill_child_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;

        if let Ok(raw_pid) = i32::try_from(child.id())
            && killpg(Pid::from_raw(raw_pid), Signal::SIGKILL).is_ok()
        {
            return;
        }
    }

    #[cfg(windows)]
    {
        // kill_tree_win already terminates `child` itself (with the hcom-kill
        // sentinel exit code 130, via terminate_win) and its whole descendant
        // tree — return here rather than falling through to child.kill()
        // below. TerminateProcess only requests termination asynchronously, so
        // a second call racing the first can overwrite the sentinel exit code
        // with a different one, corrupting the EXIT_WAS_KILLED check that
        // reads it back.
        kill_tree_win(child.id());
        return;
    }

    let _ = child.kill();
}

/// Put a not-yet-spawned [`Command`] into its own session / process group, so
/// the resulting child can be signalled as a group and is detached from the
/// parent's controlling terminal.
///
/// Unix: `setsid()` via a `pre_exec` hook. Windows: `CREATE_NEW_PROCESS_GROUP`
/// combined with `CREATE_NO_WINDOW`, so the child neither shares the parent's
/// console nor dies when that console closes.
pub fn detach_session(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid() runs in the child between fork and exec and is
        // async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
}

/// Best-effort graceful shutdown request on Windows, the analogue of Unix
/// `SIGTERM`: it asks the target to exit but does not force it.
///
/// Sends `CTRL_BREAK_EVENT` to the target's process group. Processes spawned via
/// [`detach_session`] use `CREATE_NEW_PROCESS_GROUP`, so their process-group id
/// equals their PID, and a process that has registered a handler (see
/// `sys::signal::register_term`) sets its shutdown flag in response.
///
/// Delivery only reaches a process that shares the caller's console. A relay
/// worker spawned from a different console (the common case) does not receive
/// it, and this returns `false`; the caller's wait + [`kill`] fallback then
/// terminates it. This matches Unix, where `SIGTERM` likewise only *requests*
/// shutdown and callers escalate to `SIGKILL`.
#[cfg(windows)]
fn request_shutdown_win(pid: u32) -> bool {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    // SAFETY: plain FFI call, no pointers; the process-group id is the target
    // PID (valid because detach_session uses CREATE_NEW_PROCESS_GROUP).
    unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0 }
}

#[cfg(windows)]
fn terminate_win(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    // SAFETY: opens a terminate-only handle, closes it before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return false;
        }
        // Exit code 130 (128 + SIGINT) is the hcom sentinel for "externally
        // killed via hcom kill". The pty proxy reads this back from child.wait()
        // to set EXIT_WAS_KILLED before the delivery thread records exit status.
        let ok = TerminateProcess(handle, 130) != 0;
        CloseHandle(handle);
        ok
    }
}

/// Terminate `root` and all of its descendants.
///
/// Windows has no process groups, so the only general way to "kill the agent and
/// its children" by PID from an unrelated process is to walk the parent/child
/// links in a process snapshot. The full descendant set is collected from a
/// single snapshot *before* any termination, so killing a parent can't strand a
/// child behind a now-stale parent PID (Windows does not reparent orphans).
///
/// Returns `Sent` if the root was present (and termination was attempted),
/// `NotFound` if no live process had the root PID. Like `killpg`, individual
/// termination failures are best-effort and don't change the result.
///
/// Caveat: PID reuse can make a parent link stale; this shares the same
/// theoretical race as `taskkill /T`, which is the accepted Windows approach.
#[cfg(windows)]
fn kill_tree_win(root: u32) -> GroupSignal {
    use std::collections::HashMap;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    // Build pid -> parent_pid for every live process from one snapshot.
    let mut parents: HashMap<u32, u32> = HashMap::new();
    // SAFETY: snapshot handle is closed before returning; the PROCESSENTRY32W is
    // fully initialized (dwSize set) before the enumeration calls.
    unsafe {
        let mut snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            // Snapshot can fail transiently under high load; retry once.
            snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        }
        if snapshot == INVALID_HANDLE_VALUE {
            // Still can't enumerate descendants; kill only the root. Any
            // surviving descendants will be reaped when the job object
            // closes. This still reports `Sent` (not a distinct "partial"
            // result) — `GroupSignal`/`KillResult` are a flat success/
            // not-found/permission-denied/other set consumed by ~15 call
            // sites across the CLI kill-reporting path and the relay JSON
            // protocol (see commands/kill.rs, relay/control.rs); threading a
            // new "partial" variant through all of them is a larger, separate
            // change. Logging at least makes this rare degraded path visible
            // instead of leaving zero trace anywhere.
            crate::log::log_error(
                "native",
                "win.kill_tree",
                &format!(
                    "CreateToolhelp32Snapshot failed twice; killing root pid {root} only, \
                     descendants may be left running until the job object reaps them"
                ),
            );
            return if terminate_win(root) {
                GroupSignal::Sent
            } else {
                GroupSignal::NotFound
            };
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }

    if !parents.contains_key(&root) {
        return GroupSignal::NotFound;
    }

    // Collect root + all descendants (BFS over the parent links).
    let mut tree = vec![root];
    let mut i = 0;
    while i < tree.len() {
        let current = tree[i];
        for (&pid, &ppid) in &parents {
            if ppid == current && !tree.contains(&pid) {
                tree.push(pid);
            }
        }
        i += 1;
    }

    // Terminate children before parents (deepest first) so a parent can't spawn
    // a new child after we've passed it.
    for &pid in tree.iter().rev() {
        terminate_win(pid);
    }
    GroupSignal::Sent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_alive_current_process() {
        assert!(is_alive(std::process::id()));
    }

    #[test]
    fn test_is_alive_dead_process() {
        assert!(!is_alive(99_999_999));
    }

    // Reproduces the bug fixed above: the process object stays valid (and
    // OpenProcess succeeds) as long as any handle is open, even after the
    // process has exited. Holding `child` past `wait()` keeps its handle
    // open while we probe the same PID with our own OpenProcess call.
    #[cfg(windows)]
    #[test]
    fn test_is_alive_false_for_exited_process_with_handle_still_open() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(
            !is_alive(pid),
            "an open handle to an already-exited process must not count as alive"
        );
        drop(child);
    }

    // Reproduces the bug fixed above: kill_tree_win's terminate_win writes the
    // hcom-kill sentinel exit code 130. A trailing child.kill() (a second,
    // competing TerminateProcess on the same PID) could overwrite it before
    // the OS settles on a final exit code.
    #[cfg(windows)]
    #[test]
    fn test_kill_child_group_preserves_sentinel_exit_code() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "timeout /T 30"])
            .spawn()
            .unwrap();
        kill_child_group(&mut child);
        let status = child.wait().unwrap();
        assert_eq!(
            status.code(),
            Some(130),
            "kill_child_group must not let a second kill overwrite the hcom-kill sentinel"
        );
    }
}
