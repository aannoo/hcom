//! Process-control primitives: liveness, termination, and session/group setup.
//!
//! Unix uses `libc`/`nix` signals and `setsid`; Windows uses the Win32 process
//! and job-object APIs. See the module-level docs in [`crate::sys`].

use std::process::Command;

/// Stable identity for one PID incarnation.
///
/// PIDs are reusable, so liveness checks for persisted ownership records must
/// compare the process creation time as well as the numeric PID.
pub fn identity(pid: u32) -> Option<String> {
    process_identity_platform(pid)
}

#[cfg(any(target_os = "android", target_os = "linux"))]
fn process_identity_platform(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let boot_id = boot_id.trim();
    if boot_id.is_empty() {
        return None;
    }
    // Fields after the final `) ` begin at field 3 (`state`). Linux's process
    // start time is field 22, hence index 19 in this slice. Boot ID keeps the
    // boot-relative tick count unique across restarts.
    let start_ticks = stat.rsplit_once(") ")?.1.split_whitespace().nth(19)?;
    Some(format!("linux:{boot_id}:{start_ticks}"))
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
fn process_identity_platform(pid: u32) -> Option<String> {
    // SAFETY: `info` is a correctly sized writable proc_bsdinfo buffer and
    // proc_pidinfo only fills it for the queried PID.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (read == size).then(|| format!("apple:{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec))
}

#[cfg(windows)]
fn process_identity_platform(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: the handle is opened query-only, every FILETIME pointer is
    // valid, and the handle is closed before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut created: FILETIME = std::mem::zeroed();
        let mut exited: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let ok = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0;
        CloseHandle(handle);
        ok.then(|| {
            let ticks = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
            format!("windows:{ticks}")
        })
    }
}

#[cfg(all(
    unix,
    not(any(
        target_os = "android",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos"
    ))
))]
fn process_identity_platform(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let started = String::from_utf8(output.stdout).ok()?;
    let started = started.trim();
    (!started.is_empty()).then(|| format!("unix:{started}"))
}

/// Whether `pid` still names the same process incarnation as `identity`.
pub fn has_identity(pid: u32, expected: &str) -> bool {
    identity(pid).as_deref() == Some(expected)
}

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
    #[cfg(unix)]
    PermissionDenied,
    /// Any other failure.
    #[cfg(unix)]
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
        // kill_tree_win_checked terminates `child` itself (with the hcom-kill
        // sentinel exit code 130, via terminate_win) and its whole descendant
        // tree. If root's own termination is confirmed, return rather than
        // falling through to child.kill() below: TerminateProcess only
        // requests termination asynchronously, so a second call racing the
        // first can overwrite the sentinel exit code with a different one,
        // corrupting the EXIT_WAS_KILLED check that reads it back.
        //
        // If root's termination was NOT confirmed, it may just be that the
        // fresh-by-PID OpenProcess inside terminate_win failed for a reason
        // other than "already gone" (a handle-table limit, or AV/EDR hooking
        // around freshly-opened handles, for instance) — fall through to
        // child.kill() as a retry via the handle Command already has open,
        // which a failing OpenProcess-by-PID wouldn't affect.
        let (_, root_terminated) = kill_tree_win_checked(child.id());
        if root_terminated {
            return;
        }
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

/// Snapshot every live process's pid -> parent_pid link via
/// `CreateToolhelp32Snapshot`, retrying once on transient failure.
///
/// Returns `None` if the snapshot could not be taken even after the retry.
#[cfg(windows)]
fn snapshot_parents() -> Option<std::collections::HashMap<u32, u32>> {
    use std::collections::HashMap;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

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
            return None;
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
    Some(parents)
}

/// Spawn a detached child without leaking the caller's captured stdio handles.
///
/// On Windows, `Command` must enable handle inheritance for explicitly supplied
/// child stdio (for example a background log file). Without an explicit handle
/// list, that can also inherit the hcom CLI's own stdout/stderr pipes. A caller
/// using `Command::output()` then waits forever for EOF after hcom exits because
/// the long-lived agent still owns duplicate pipe handles.
pub fn spawn_detached(command: &mut Command) -> std::io::Result<std::process::Child> {
    detach_session(command);

    #[cfg(not(windows))]
    {
        command.spawn()
    }

    #[cfg(windows)]
    {
        use std::sync::{Mutex, OnceLock};
        use windows_sys::Win32::Foundation::{
            GetHandleInformation, HANDLE_FLAG_INHERIT, SetHandleInformation,
        };
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };

        static SPAWN_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = SPAWN_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());

        let mut restored = Vec::new();
        for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: GetStdHandle returns a borrowed process handle. We only
            // query/change its inheritance flag and restore it before returning.
            unsafe {
                let handle = GetStdHandle(kind);
                if handle.is_null() {
                    continue;
                }
                let mut flags = 0;
                if GetHandleInformation(handle, &mut flags) != 0
                    && flags & HANDLE_FLAG_INHERIT != 0
                    && SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) != 0
                {
                    restored.push(handle);
                }
            }
        }

        let child = command.spawn();
        for handle in restored {
            // SAFETY: these are the same live borrowed standard handles whose
            // inheritance flag was cleared above.
            unsafe {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
            }
        }
        child
    }
}

/// Number of extra re-scan rounds run after the initial kill pass, to catch
/// descendants spawned after the first snapshot (see [`kill_tree_win_checked`]).
#[cfg(windows)]
const RESCAN_ROUNDS: u32 = 2;

/// Delay between re-scan rounds, giving the OS a moment to start tearing down
/// what was just killed before the next snapshot is taken.
#[cfg(windows)]
const RESCAN_DELAY: std::time::Duration = std::time::Duration::from_millis(75);

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
    kill_tree_win_checked(root).0
}

/// Same as [`kill_tree_win`], but also reports whether `root`'s own
/// `terminate_win` call specifically succeeded (as opposed to just "root was
/// found in the snapshot"). `kill_tree_win`'s `Sent` doesn't distinguish
/// these — every descendant's individual termination result is discarded —
/// so [`kill_child_group`] uses this directly to decide whether a retry via
/// its own already-open `Child` handle is worthwhile.
///
/// Re-scan for stragglers: a single snapshot is inherently a point-in-time
/// view, so a process spawned by any tree member *after* the snapshot but
/// *before* that member is terminated is invisible to the first pass and would
/// otherwise survive. Unix's `killpg` doesn't have this problem for
/// already-existing group members — the kernel delivers the signal to the
/// whole group atomically — but Windows has no equivalent primitive, so after
/// the first kill pass this takes a couple of follow-up snapshots, each time
/// looking for new processes parented by *any* previously-known tree member
/// (not just `root`, since mid-tree descendants can keep spawning children of
/// their own even after `root` is gone) and killing those too. This narrows
/// the race window considerably but, being fundamentally snapshot-based, does
/// not eliminate it: a straggler spawned after the very last re-scan's
/// snapshot can still escape.
#[cfg(windows)]
fn kill_tree_win_checked(root: u32) -> (GroupSignal, bool) {
    let Some(parents) = snapshot_parents() else {
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
        let ok = terminate_win(root);
        return (
            if ok {
                GroupSignal::Sent
            } else {
                GroupSignal::NotFound
            },
            ok,
        );
    };

    if !parents.contains_key(&root) {
        return (GroupSignal::NotFound, false);
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
    let mut root_terminated = false;
    for &pid in tree.iter().rev() {
        let ok = terminate_win(pid);
        if pid == root {
            root_terminated = ok;
        }
    }

    // Re-scan for stragglers spawned between the snapshot and their parent's
    // termination (see doc comment above).
    //
    // This widens (but does not introduce in kind) the PID-reuse race already
    // called out on `kill_tree_win_checked`: each extra round is another
    // `terminate_win` -> sleep -> re-snapshot cycle, so there's more elapsed
    // time in which a PID this function just terminated could be recycled by
    // an unrelated process that happens to be parented under another PID
    // still in `tree`. `KillOnDropJob` (see `pty::win::job`) remains as a
    // backstop that reaps the real tree regardless of this race, so this is
    // treated as an acceptable, documented widening rather than something to
    // engineer away here — doing so would need per-candidate parent-identity
    // confirmation (e.g. `GetProcessTimes` creation-time checks) for a
    // vanishingly unlikely window (tens of milliseconds).
    //
    // Always run all RESCAN_ROUNDS —
    // round N finding nothing new doesn't mean a straggler can't still appear
    // before round N+1's snapshot, so stopping early would defeat the point of
    // having more than one round. Within a round, expand `tree` to a fixed
    // point (mirroring the initial BFS above) rather than a single pass, so a
    // straggler and *its own* child that both first appear in the same
    // snapshot are both caught in that round regardless of HashMap iteration
    // order.
    for _ in 0..RESCAN_ROUNDS {
        std::thread::sleep(RESCAN_DELAY);
        let Some(parents) = snapshot_parents() else {
            break;
        };
        let before = tree.len();
        loop {
            let start_len = tree.len();
            for (&pid, &ppid) in &parents {
                if tree.contains(&ppid) && !tree.contains(&pid) {
                    tree.push(pid);
                }
            }
            if tree.len() == start_len {
                break;
            }
        }
        // Kill only the newly discovered stragglers (deepest-first isn't
        // load-bearing here since these are freshly discovered leaves relative
        // to the known tree, but iterating in reverse discovery order costs
        // nothing and keeps the same "children before parents" spirit).
        for &pid in tree[before..].iter().rev() {
            terminate_win(pid);
        }
    }

    (GroupSignal::Sent, root_terminated)
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

    #[test]
    fn test_process_identity_is_stable_for_current_process() {
        let pid = std::process::id();
        let first = identity(pid).expect("current process must have a start identity");
        assert_eq!(identity(pid).as_deref(), Some(first.as_str()));
        assert!(has_identity(pid, &first));
        assert!(!has_identity(pid, "a different process incarnation"));
    }

    #[test]
    fn test_process_identity_is_absent_for_dead_pid() {
        assert!(identity(u32::MAX).is_none());
    }

    #[cfg(any(target_os = "android", target_os = "linux"))]
    #[test]
    fn test_linux_process_identity_includes_boot_id() {
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        let identity = identity(std::process::id()).unwrap();
        assert!(identity.contains(boot_id.trim()));
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
