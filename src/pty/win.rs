//! Windows ConPTY proxy — the Windows-native equivalent of the Unix PTY wrapper.
//!
//! The Unix proxy (`super`) is built on `openpty` + `nix::poll`. Windows has no
//! such primitives, so this spawns the tool under a **ConPTY** (via
//! `portable-pty`) and drives it with blocking IO threads instead of a poll
//! loop. The upper layers are reused unchanged: [`ScreenTracker`] for vt100
//! screen tracking, [`InjectServer`] for TCP text injection, and
//! [`run_delivery_loop`] for notify-driven message delivery. This is what lets
//! an **idle** agent be woken on Windows (the M1 limitation): the delivery loop
//! injects `<hcom>` text into the ConPTY input when a message arrives.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use super::ProxyConfig;
use super::inject::{InjectResult, InjectServer};
use super::screen::ScreenTracker;

use crate::db::HcomDb;
use crate::delivery::{DeliveryState, EXIT_WAS_KILLED, ScreenState, ToolConfig, run_delivery_loop};
use crate::log::{log_error, log_warn};
use crate::notify::NotifyServer;

/// Windows ConPTY-backed PTY proxy.
pub struct Proxy {
    config: ProxyConfig,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    screen_state: Arc<RwLock<ScreenState>>,
    launch_phase_active: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    notify_port: Arc<AtomicU16>,
    current_name: Arc<RwLock<String>>,
    current_status: Arc<RwLock<String>>,
    rows: u16,
    cols: u16,
    delivery_handle: Option<JoinHandle<()>>,
    /// Job object the child is assigned to (`KILL_ON_JOB_CLOSE`). Reaps the
    /// child's whole tree even if this proxy dies abnormally and `Drop` never
    /// runs. `None` if the child couldn't be assigned (falls back to the
    /// snapshot-based kill in `Drop`).
    _job: Option<job::KillOnDropJob>,
}

impl Proxy {
    /// Spawn `command` under a ConPTY and prepare the proxy.
    pub fn spawn(command: &str, args: &[&str], config: ProxyConfig) -> Result<Self> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty (ConPTY) failed")?;

        let mut cmd = CommandBuilder::new(command);
        cmd.args(args);
        for (k, v) in &config.env_vars {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("ConPTY spawn failed")?;
        // The parent does not need the slave handle once the child holds it.
        drop(pair.slave);

        let writer = pair.master.take_writer().context("take_writer failed")?;

        // Persist PID so `hcom kill` can target the agent.
        if let Some(ref instance_name) = config.instance_name
            && let Ok(db) = HcomDb::open()
            && let Some(pid) = child.process_id()
        {
            let _ = db.update_instance_pid(instance_name, pid);
        }

        // Tie the child to a kill-on-close job so its whole tree is reaped if we
        // die abnormally (the explicit snapshot-kill in Drop covers clean exit).
        let job = child.process_id().and_then(job::KillOnDropJob::assign);

        let initial_name = config.instance_name.clone().unwrap_or_default();

        Ok(Self {
            config,
            child,
            master: pair.master,
            writer: Arc::new(Mutex::new(writer)),
            screen_state: Arc::new(RwLock::new(ScreenState::default())),
            launch_phase_active: Arc::new(AtomicBool::new(true)),
            running: Arc::new(AtomicBool::new(true)),
            notify_port: Arc::new(AtomicU16::new(0)),
            current_name: Arc::new(RwLock::new(initial_name)),
            current_status: Arc::new(RwLock::new(String::new())),
            rows,
            cols,
            delivery_handle: None,
            _job: job,
        })
    }

    /// Run the proxy until the child exits, returning its exit code.
    pub fn run(&mut self) -> Result<i32> {
        // Put our console into raw + VT passthrough so the tool's TUI renders
        // and keystrokes flow through unbuffered. Restored on drop.
        let _console = console::RawConsoleGuard::enable();

        let inject_server = InjectServer::new()?;
        let inject_port = inject_server.port();

        self.spawn_reader_thread();
        self.spawn_stdin_thread();
        self.spawn_inject_thread(inject_server);
        self.spawn_delivery_thread(inject_port);

        // Block until the child exits.
        let status = self.child.wait().context("waiting for ConPTY child")?;
        let exit_code = status.exit_code() as i32;

        // Commit the exit reason before setting running=false. The delivery
        // thread reads EXIT_WAS_KILLED in cleanup; if we set running=false first
        // (or allow the reader thread to do so), the delivery loop can enter
        // cleanup before this store — recording exit:closed for a kill.
        // Exit code 130 is the sentinel written by terminate_win().
        EXIT_WAS_KILLED.store(exit_code == 130, Ordering::Release);

        // Signal threads to stop and wake the delivery loop's notify select.
        self.running.store(false, Ordering::Release);
        let port = self.notify_port.load(Ordering::Acquire);
        if port != 0 {
            let _ = std::net::TcpStream::connect(("127.0.0.1", port));
        }
        if let Some(handle) = self.delivery_handle.take() {
            let _ = handle.join();
        }

        Ok(exit_code)
    }

    /// PTY output → our stdout, feeding the screen tracker and the shared
    /// screen state the delivery loop reads. portable-pty's reader blocks, so a
    /// dedicated thread replaces the Unix poll loop.
    fn spawn_reader_thread(&self) {
        let reader = match self.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                log_error("native", "win.reader", &format!("try_clone_reader: {e}"));
                return;
            }
        };
        let running = self.running.clone();
        let screen_state = self.screen_state.clone();
        let launch_phase = self.launch_phase_active.clone();
        let tool_name = self.config.target.name().to_string();
        let ready_pattern = self.config.ready_pattern.clone();
        let instance = self.config.instance_name.clone();
        let (rows, cols) = (self.rows, self.cols);

        thread::spawn(move || {
            let mut reader = reader;
            let mut screen =
                ScreenTracker::new_with_instance(rows, cols, &ready_pattern, instance.as_deref());
            let mut stdout = std::io::stdout();
            let mut filter = OutputModeFilter::default();
            let mut buf = [0u8; 8192];
            let mut scratch: Vec<u8> = Vec::with_capacity(8192);
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // child exited / PTY closed
                    Ok(n) => {
                        let data = &buf[..n];
                        // Strip the child's Win32-input/focus mode-set sequences
                        // before they reach the *outer* terminal (see
                        // OutputModeFilter); otherwise the outer terminal answers
                        // the child's DSR query in Win32 input-record encoding,
                        // which the child can't parse, and startup hangs.
                        scratch.clear();
                        filter.filter(data, &mut scratch);
                        let _ = stdout.write_all(&scratch);
                        let _ = stdout.flush();
                        screen.process(data);
                        update_screen_state(&screen_state, &screen, &tool_name, &launch_phase);
                    }
                    Err(_) => break,
                }
            }
            // Do NOT store running=false here. Letting run() be the sole writer
            // ensures EXIT_WAS_KILLED is committed before the delivery thread
            // sees running=false and enters cleanup. If the reader set it first,
            // the delivery loop could read EXIT_WAS_KILLED=false and record
            // exit:closed even when the child was killed via `hcom kill`.
        });
    }

    /// Our stdin → PTY input. Intentionally detached and never joined.
    ///
    /// The `running` check at the loop top only catches shutdown *between*
    /// reads; a `stdin.read()` already blocked when the child exits cannot be
    /// interrupted and outlives the child. This does not leak: `main` calls
    /// `std::process::exit` immediately after `run` returns (and `Proxy::drop`),
    /// which terminates the process and reaps this thread even mid-read. The
    /// thread holds no lock across the blocking read, so it cannot wedge
    /// cleanup. If `Proxy` ever gains a caller that keeps running after `run`
    /// returns, this read would need an explicit interrupt (e.g.
    /// `CancelSynchronousIo`).
    fn spawn_stdin_thread(&self) {
        let writer = self.writer.clone();
        let running = self.running.clone();
        thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 4096];
            loop {
                if !running.load(Ordering::Acquire) {
                    break;
                }
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut w) = writer.lock() {
                            let _ = w.write_all(&buf[..n]);
                            let _ = w.flush();
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    /// InjectServer → PTY input. Polls for inject connections (the delivery loop
    /// and `hcom term inject` connect here) and writes the text to the ConPTY.
    fn spawn_inject_thread(&self, mut inject_server: InjectServer) {
        let writer = self.writer.clone();
        let running = self.running.clone();
        thread::spawn(move || {
            while running.load(Ordering::Acquire) {
                // Drain the accept queue.
                while matches!(inject_server.accept(), Ok(true)) {}
                // Process clients high-to-low so completed-client removal inside
                // read_client doesn't shift indices we haven't visited.
                for i in (0..inject_server.client_count()).rev() {
                    match inject_server.read_client(i) {
                        Ok(InjectResult::Inject(text)) => {
                            if let Ok(mut w) = writer.lock() {
                                let _ = w.write_all(text.as_bytes());
                                let _ = w.flush();
                            }
                        }
                        // Screen queries (`hcom term`) are not served by the
                        // ConPTY proxy yet; answer empty so the client unblocks.
                        Ok(InjectResult::Query(q)) => q.respond(""),
                        _ => {}
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
    }

    /// Notify-driven delivery loop: wakes on `hcom send` and injects the message
    /// into the ConPTY (via `inject_port`), which is how an idle agent reacts.
    fn spawn_delivery_thread(&mut self, inject_port: u16) {
        let running = self.running.clone();
        let screen_state = self.screen_state.clone();
        let launch_phase = self.launch_phase_active.clone();
        let notify_port_shared = self.notify_port.clone();
        let shared_name = self.current_name.clone();
        let shared_status = self.current_status.clone();
        let instance_name = self.config.instance_name.clone().unwrap_or_default();
        let delivery_tool = self.config.target.delivery_tool();
        let user_activity_cooldown_ms = 500u64;

        let handle = thread::spawn(move || {
            let mut db = match HcomDb::open() {
                Ok(db) => db,
                Err(e) => {
                    log_error("native", "win.delivery.init", &format!("db open: {e}"));
                    return;
                }
            };
            let notify = match NotifyServer::new() {
                Ok(n) => n,
                Err(e) => {
                    log_error("native", "win.delivery.init", &format!("notify: {e}"));
                    return;
                }
            };
            notify_port_shared.store(notify.port(), Ordering::Release);
            if let Err(e) = db.register_inject_port(&instance_name, inject_port) {
                log_warn("native", "win.inject.register", &format!("{e}"));
            }
            let state = DeliveryState {
                screen: screen_state,
                launch_phase_active: launch_phase,
                inject_port,
                user_activity_cooldown_ms,
            };
            let config = ToolConfig::for_tool(delivery_tool);

            run_delivery_loop(
                running,
                &mut db,
                &notify,
                &state,
                &instance_name,
                &config,
                Some(shared_name),
                Some(shared_status),
            );
        });
        self.delivery_handle = Some(handle);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        // Reap the child and any descendants it spawned (race-free snapshot
        // walk). The `_job` field's kill-on-close is the backstop for the case
        // where Drop never runs.
        if let Some(pid) = self.child.process_id() {
            // Drop running kill_group means run() exited early (error path) and
            // we are force-killing the child. Mark as killed so the delivery
            // thread records exit:killed if it is still running.
            EXIT_WAS_KILLED.store(true, Ordering::Release);
            let _ = crate::sys::process::kill_group(pid);
        }
        let _ = self.child.kill();
        if let Some(ref instance_name) = self.config.instance_name
            && let Ok(db) = HcomDb::open()
        {
            let _ = db.delete_notify_endpoint(instance_name, "inject");
        }
    }
}

/// Strips the child's DEC private-mode **sets** for Win32 input mode (`?9001`)
/// and focus reporting (`?1004`) from the output stream, so the *outer*
/// terminal is never switched into them.
///
/// A ConPTY wrapper sits between the child and the real terminal. If the child's
/// `ESC[?9001h` reaches the outer terminal, that terminal starts encoding its
/// input — including its automatic `ESC[6n` (cursor position) reply — as Win32
/// input records. The child, which only understands a plain `ESC[15;1R`, then
/// waits forever for a reply it can parse. Dropping these mode-sets keeps the
/// outer terminal in normal VT mode so the DSR reply round-trips correctly.
///
/// Only complete `CSI ? 9001/1004 h|l` sequences are dropped; the parser is
/// stateful so sequences split across reads are handled, and every other byte
/// (including all other escape sequences) passes through unchanged.
#[derive(Default)]
struct OutputModeFilter {
    state: FilterState,
    buf: Vec<u8>,
}

#[derive(Default, PartialEq)]
enum FilterState {
    #[default]
    Ground,
    Esc,
    Csi,
}

impl OutputModeFilter {
    fn filter(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            match self.state {
                FilterState::Ground => {
                    if b == 0x1b {
                        self.buf.clear();
                        self.buf.push(b);
                        self.state = FilterState::Esc;
                    } else {
                        out.push(b);
                    }
                }
                FilterState::Esc => {
                    self.buf.push(b);
                    if b == b'[' {
                        self.state = FilterState::Csi;
                    } else {
                        // Not a CSI sequence — pass through untouched.
                        out.extend_from_slice(&self.buf);
                        self.buf.clear();
                        self.state = FilterState::Ground;
                    }
                }
                FilterState::Csi => {
                    self.buf.push(b);
                    if (0x40..=0x7e).contains(&b) {
                        if !self.is_blocked() {
                            out.extend_from_slice(&self.buf);
                        }
                        self.buf.clear();
                        self.state = FilterState::Ground;
                    } else if self.buf.len() > 32 {
                        // Malformed/overlong — give up filtering, emit as-is.
                        out.extend_from_slice(&self.buf);
                        self.buf.clear();
                        self.state = FilterState::Ground;
                    }
                }
            }
        }
    }

    fn is_blocked(&self) -> bool {
        self.buf.starts_with(b"\x1b[?9001") || self.buf.starts_with(b"\x1b[?1004")
    }
}

#[cfg(test)]
mod tests {
    use super::OutputModeFilter;

    fn run(chunks: &[&[u8]]) -> Vec<u8> {
        let mut f = OutputModeFilter::default();
        let mut out = Vec::new();
        for c in chunks {
            f.filter(c, &mut out);
        }
        out
    }

    #[test]
    fn drops_win32_and_focus_mode_sets() {
        // ESC[?9001h ESC[?1004h "hi" ESC[6n
        let input = b"\x1b[?9001h\x1b[?1004h hi \x1b[6n";
        assert_eq!(run(&[input]), b" hi \x1b[6n");
    }

    #[test]
    fn passes_other_sequences_and_text() {
        let input = b"\x1b[31mred\x1b[0m\x1b[2J plain";
        assert_eq!(run(&[input]), input);
    }

    #[test]
    fn handles_sequence_split_across_reads() {
        // ESC[?9001h split mid-sequence must still be dropped.
        assert_eq!(run(&[b"\x1b[?90", b"01h", b"X"]), b"X");
    }

    #[test]
    fn drops_mode_reset_too() {
        assert_eq!(run(&[b"\x1b[?9001l\x1b[?1004lY"]), b"Y");
    }
}

/// Refresh the shared [`ScreenState`] from the screen tracker so the delivery
/// loop's gates (idle / ready / prompt-empty) see current screen contents.
/// A trimmed Windows counterpart to the Unix proxy's `update_delivery_state`;
/// approval-scrape latching (Codex/Cursor) is deferred.
fn update_screen_state(
    screen_state: &Arc<RwLock<ScreenState>>,
    screen: &ScreenTracker,
    tool_name: &str,
    launch_phase: &Arc<AtomicBool>,
) {
    if let Ok(mut state) = screen_state.write() {
        state.ready = screen.is_ready();
        let input_text = screen.get_input_box_text(tool_name);
        let new_prompt_empty = input_text.as_ref().is_some_and(|t| t.is_empty());
        // Stamp the submit-edge cooldown when input goes from a known non-empty
        // value to empty/undetected, mirroring the Unix proxy so the gate does
        // not double-deliver during the hook's status-flip lag.
        let was_non_empty = state.input_text.as_deref().is_some_and(|t| !t.is_empty());
        let now_empty = input_text.as_deref().map(|t| t.is_empty()).unwrap_or(true);
        if was_non_empty && now_empty {
            state.last_prompt_submit = Some(Instant::now());
        }
        state.prompt_empty = new_prompt_empty;
        state.input_text = input_text;
        state.visible_tail = if launch_phase.load(Ordering::Acquire) {
            screen.visible_tail(5, 500)
        } else {
            None
        };
        state.last_output = screen.last_output_instant();
        state.cols = screen.cols();
    }
}

/// A job object whose assigned processes are killed when the handle closes.
mod job {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    pub struct KillOnDropJob {
        /// HANDLE stored as `isize` (matches the console module) so the field
        /// stays `Send` and doesn't infect the proxy with a raw pointer.
        handle: isize,
    }

    impl KillOnDropJob {
        /// Create a `KILL_ON_JOB_CLOSE` job and assign `pid` to it. Returns
        /// `None` (caller falls back to an explicit kill) if any step fails —
        /// e.g. the process already exited or assignment is refused.
        pub fn assign(pid: u32) -> Option<Self> {
            // SAFETY: each handle is closed on every failure path; the limit
            // struct is zero-initialized before its one field is set.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let set = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if set == 0 {
                    CloseHandle(handle);
                    return None;
                }
                let proc = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
                if proc.is_null() {
                    CloseHandle(handle);
                    return None;
                }
                let assigned = AssignProcessToJobObject(handle, proc);
                CloseHandle(proc);
                if assigned == 0 {
                    CloseHandle(handle);
                    return None;
                }
                Some(KillOnDropJob {
                    handle: handle as isize,
                })
            }
        }
    }

    impl Drop for KillOnDropJob {
        fn drop(&mut self) {
            // Closing the last handle to a KILL_ON_JOB_CLOSE job terminates
            // every process still assigned to it.
            // SAFETY: handle came from CreateJobObjectW and is closed once.
            unsafe {
                CloseHandle(self.handle as _);
            }
        }
    }
}

/// Windows console raw-mode + VT passthrough, restored on drop.
mod console {
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
        GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };

    pub struct RawConsoleGuard {
        stdin_handle: isize,
        stdout_handle: isize,
        prev_in: CONSOLE_MODE,
        prev_out: CONSOLE_MODE,
        restore: bool,
    }

    impl RawConsoleGuard {
        /// Best-effort: disable line input/echo on stdin, enable VT input, and
        /// enable VT processing on stdout so the child's escape sequences render.
        /// If the handles aren't consoles (piped), this is a no-op.
        pub fn enable() -> Self {
            // SAFETY: GetStdHandle returns process-owned console handles; the
            // mode getters/setters only touch those handles.
            unsafe {
                let stdin_handle = GetStdHandle(STD_INPUT_HANDLE) as isize;
                let stdout_handle = GetStdHandle(STD_OUTPUT_HANDLE) as isize;
                let mut prev_in: CONSOLE_MODE = 0;
                let mut prev_out: CONSOLE_MODE = 0;
                let ok_in = GetConsoleMode(stdin_handle as _, &mut prev_in) != 0;
                let ok_out = GetConsoleMode(stdout_handle as _, &mut prev_out) != 0;
                if ok_in {
                    let raw_in = (prev_in
                        & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                        | ENABLE_VIRTUAL_TERMINAL_INPUT;
                    SetConsoleMode(stdin_handle as _, raw_in);
                }
                if ok_out {
                    SetConsoleMode(
                        stdout_handle as _,
                        prev_out | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                    );
                }
                RawConsoleGuard {
                    stdin_handle,
                    stdout_handle,
                    prev_in,
                    prev_out,
                    restore: ok_in || ok_out,
                }
            }
        }
    }

    impl Drop for RawConsoleGuard {
        fn drop(&mut self) {
            if !self.restore {
                return;
            }
            // SAFETY: restoring the previously-read modes on the same handles.
            unsafe {
                SetConsoleMode(self.stdin_handle as _, self.prev_in);
                SetConsoleMode(self.stdout_handle as _, self.prev_out);
            }
        }
    }
}
