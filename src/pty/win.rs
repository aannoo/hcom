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
use std::io::{IsTerminal, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use super::ProxyConfig;
use super::inject::{InjectResult, InjectServer, QueryCommand};
use super::screen::ScreenTracker;
use super::shared;
use super::stdout_queue::StdoutQueue;
use super::trace::{self, Hop};

use crate::db::HcomDb;
use crate::delivery::{EXIT_WAS_KILLED, ScreenState};
use crate::integration_spec::ConsoleInput;
use crate::log::{log_error, log_warn};

/// True if `path` is a `.cmd`/`.bat` script (case-insensitive), which
/// `CreateProcessW` cannot execute directly — only `cmd.exe /c` can run those.
/// Covers npm-installed tool shims, which `terminal::which_bin` resolves to
/// `<name>.cmd` via PATHEXT.
fn is_cmd_script(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
}

/// Windows ConPTY-backed PTY proxy.
pub struct Proxy {
    config: ProxyConfig,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// ConPTY master, shared so the resize-watcher (calls `resize`) and the
    /// reader-spawn (calls `try_clone_reader`) can both lock it. `MasterPty` is
    /// `Send` but not `Clone`, so a `Mutex` is the only way to share it.
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    screen_state: Arc<RwLock<ScreenState>>,
    launch_phase_active: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    notify_port: Arc<AtomicU16>,
    current_name: Arc<RwLock<String>>,
    current_status: Arc<RwLock<String>>,
    rows: u16,
    cols: u16,
    /// Delivery thread handle. Wrapped in `Arc<Mutex<Option<_>>>` because the
    /// delivery coordinator thread starts delivery (ready-or-timeout gated) and
    /// stores the handle, while `run()`/`Drop` take it to join.
    delivery_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Latched by the reader thread once the tool's ready pattern is observed;
    /// read by the delivery coordinator to start delivery early. A latch never
    /// regresses, unlike reading the (non-latched) screen-state ready flag.
    ready_signaled: Arc<AtomicBool>,
    /// Eagerly-maintained screen dump for `hcom term` screen queries. The reader
    /// owns the `ScreenTracker` and refreshes this (throttled) each chunk; the
    /// inject thread serves it directly so an idle agent — whose reader is
    /// blocked in `read()` — still answers immediately (#4).
    screen_snapshot: Arc<RwLock<String>>,
    /// Set by the stdin/inject threads when a genuine keystroke (or injected
    /// answer) should clear a pending approval; consumed by the reader thread,
    /// which owns the `ScreenTracker` and calls `clear_approval()`.
    approval_clear_requested: Arc<AtomicBool>,
    /// Pending terminal resize `(rows, cols)` detected by the resize-watcher;
    /// applied to the `ScreenTracker` by the reader thread before `process`.
    pending_resize: Arc<RwLock<Option<(u16, u16)>>>,
    /// Visible tail captured by the reader thread on EOF, read by `run()` to
    /// build the launch-failure diagnostic.
    last_tail: Arc<RwLock<Option<String>>>,
    /// Set when delivery initialization fails; `run()` maps it to a nonzero exit.
    launch_failed: Arc<AtomicBool>,
    /// The child's SGR mouse tracking (`MouseTracking` as u8), published by
    /// the reader thread, which owns the screen tracker, for the stdin thread.
    mouse_tracking: Arc<AtomicU8>,
    /// Job object the child is assigned to (`KILL_ON_JOB_CLOSE`). Reaps the
    /// child's whole tree even if this proxy dies abnormally and `Drop` never
    /// runs. `None` if the child couldn't be assigned (falls back to the
    /// snapshot-based kill in `Drop`).
    _job: Option<job::KillOnDropJob>,
    /// When the ConPTY child was spawned; the reader's startup trace measures
    /// from here.
    spawned_at: Instant,
}

impl Proxy {
    /// Spawn `command` under a ConPTY and prepare the proxy.
    pub fn spawn(command: &str, args: &[&str], config: ProxyConfig) -> Result<Self> {
        let spawn_started = Instant::now();
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

        super::conpty::select();
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty (ConPTY) failed")?;

        // npm-installed tool shims (e.g. `gemini.cmd`, `codex.cmd`) resolve to
        // `.cmd`/`.bat` files. CreateProcessW cannot execute those directly —
        // only `cmd.exe /c` knows how to run a batch file — so route through
        // the shell in that case. Other extensions (`.exe`, extension-less)
        // spawn directly, exactly as before.
        let mut cmd = if is_cmd_script(command) {
            let mut c = CommandBuilder::new("cmd.exe");
            c.args(["/c", command]);
            c.args(args);
            c
        } else {
            let mut c = CommandBuilder::new(command);
            c.args(args);
            c
        };
        for (k, v) in &config.env_vars {
            cmd.env(k, v);
        }
        // Pin the ConPTY child's working directory. The Unix runner `.sh` does
        // `cd {cwd}` then `exec hcom`, so the openpty child inherits the launch
        // dir; the Windows runner `.ps1` uses `Set-Location` then invokes
        // `hcom.exe` as a *child*. `Set-Location` only moves the PowerShell
        // host's cwd — the spawned hcom (and the ConPTY child) do not reliably
        // inherit it, and `CommandBuilder` defaults the child to the process
        // default (the user's home) when no cwd is set. That launched Claude
        // outside the repo, so its file index fell back to a full-home ripgrep
        // scan (~11s), freezing input and swallowing ESC-ESC. hcom's own cwd is
        // already the launch dir, so pinning to it keeps Claude in-repo.
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(crate::shared::platform::child_process_path(&cwd));
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .context("ConPTY spawn failed")?;
        let spawned_at = Instant::now();
        shared::log_spawned(
            config.instance_name.as_deref(),
            child.process_id(),
            spawned_at.duration_since(spawn_started),
            command,
        );
        // Install descendant cleanup immediately. Any later setup failure must
        // still reap the spawned process tree.
        let job = child.process_id().and_then(job::KillOnDropJob::assign);
        // The parent does not need the slave handle once the child holds it.
        drop(pair.slave);

        let writer = pair.master.take_writer().context("take_writer failed")?;

        // Persist PID so `hcom kill` can target the agent.
        if let Some(ref instance_name) = config.instance_name {
            let persist_result = (|| -> Result<()> {
                let pid = child
                    .process_id()
                    .context("ConPTY child has no process id")?;
                let db = HcomDb::open()?;
                // The child handle keeps this PID from being reused, so a missing
                // identity only means no reuse protection for later cleanup.
                db.update_instance_pid_with_identity(
                    instance_name,
                    pid,
                    crate::sys::process::identity(pid).as_deref(),
                )?;

                Ok(())
            })();
            if let Err(error) = persist_result {
                // We still hold the child's handle, so its PID can't have been
                // reused. Kill the tree, then the child itself, and reap it so
                // nothing outlives the failed launch (matches the Unix path).
                if let Some(pid) = child.process_id() {
                    let _ = crate::sys::process::kill_group(pid);
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.context("failed to persist ConPTY process"));
            }
        }

        let initial_name = config.instance_name.clone().unwrap_or_default();

        Ok(Self {
            config,
            child,
            master: Arc::new(Mutex::new(pair.master)),
            writer: Arc::new(Mutex::new(writer)),
            screen_state: Arc::new(RwLock::new(ScreenState::default())),
            launch_phase_active: Arc::new(AtomicBool::new(true)),
            running: Arc::new(AtomicBool::new(true)),
            notify_port: Arc::new(AtomicU16::new(0)),
            current_name: Arc::new(RwLock::new(initial_name)),
            current_status: Arc::new(RwLock::new(String::new())),
            rows,
            cols,
            delivery_handle: Arc::new(Mutex::new(None)),
            ready_signaled: Arc::new(AtomicBool::new(false)),
            screen_snapshot: Arc::new(RwLock::new(String::new())),
            approval_clear_requested: Arc::new(AtomicBool::new(false)),
            pending_resize: Arc::new(RwLock::new(None)),
            last_tail: Arc::new(RwLock::new(None)),
            launch_failed: Arc::new(AtomicBool::new(false)),
            mouse_tracking: Arc::new(AtomicU8::new(0)),
            _job: job,
            spawned_at,
        })
    }

    /// Run the proxy until the child exits, returning its exit code.
    pub fn run(&mut self) -> Result<i32> {
        // Put our console into raw + VT passthrough so the tool's TUI renders
        // and keystrokes flow through unbuffered. Restored on drop.
        trace::note(
            Hop::Mode,
            &format!(
                "tool={} input={:?} conpty={} size={}x{}",
                self.config.target.name(),
                self.config.target.console_input(),
                super::conpty::selected(),
                self.cols,
                self.rows
            ),
        );
        let _console = console::RawConsoleGuard::enable(self.config.target.console_input());

        let startup_time = Instant::now();

        let inject_server = InjectServer::new()?;
        let inject_port = inject_server.port();

        // Spawn the reader first: it can fail (ConPTY reader clone), and a
        // failure here must tear nothing half-up (#23), so it bails before any
        // other thread exists. The already-spawned child is reaped by Drop. Keep
        // its handle: run() joins it below so the EOF-captured launch-failure
        // tail is available.
        let reader_handle = self.spawn_reader_thread(inject_port)?;
        // A dedicated coordinator owns delivery-thread startup (ready-or-timeout
        // gated); the reader can't, since it blocks in read() and cannot run a
        // timer while the child is idle.
        let coordinator_handle = self.spawn_delivery_coordinator(inject_port);
        self.spawn_stdin_thread();
        self.spawn_inject_thread(inject_server);
        self.spawn_resize_watcher();

        // Terminal metadata may take 900ms to arrive. Collect it only after
        // starting the IO threads: ConPTY stops capturing its startup cursor
        // report after 1s, and a late reply can become literal prompt input.
        // PID ownership was persisted in spawn(), so kill can still find the
        // child during this wait. store_launch_context fills missing fields
        // without overwriting richer context already captured by a start hook.
        if let Some(ref instance_name) = self.config.instance_name {
            let context = shared::build_early_launch_context();
            if let Ok(db) = HcomDb::open() {
                let _ = db.store_launch_context(instance_name, &context);
            }
        }

        // Poll rather than blocking solely in child.wait(): a definitive
        // delivery-init failure must terminate a long-lived child immediately.
        let exit_code = loop {
            if self.launch_failed.load(Ordering::Acquire) {
                if let Some(pid) = self.child.process_id() {
                    let _ = crate::sys::process::kill_group(pid);
                } else {
                    let _ = self.child.kill();
                }
                break wait_child_blocking(self.child.as_mut());
            }
            match self.child.try_wait() {
                Ok(Some(status)) => break status.exit_code() as i32,
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(error) => {
                    log_error("native", "win.wait", &format!("child wait failed: {error}"));
                    break 1;
                }
            }
        };

        // Commit the exit reason before setting running=false. The delivery
        // thread reads EXIT_WAS_KILLED in cleanup; if we set running=false first
        // (or allow the reader thread to do so), the delivery loop can enter
        // cleanup before this store — recording exit:closed for a kill.
        // Exit code 130 is the sentinel written by terminate_win() for an
        // externally-issued `hcom kill`.
        EXIT_WAS_KILLED.store(exit_code == 130, Ordering::Release);

        // Join the reader BEFORE reading last_tail (and before running=false, so
        // the ordering below still matches the Unix proxy). The reader breaks
        // its loop on PTY EOF (Ok(0)), not on `running`, so the child having
        // exited is enough for it to wind down — no stop signal is needed first.
        // It refreshes last_tail when launch output goes quiet and again at that
        // EOF; the ConPTY pipe can signal EOF after `child.wait()` returned (or
        // not until the pseudoconsole closes), so joining first lets a final
        // EOF capture land before we read it.
        //
        // Bounded join: the ConPTY pipe only reaches EOF once *every* process
        // holding the slave handle exits. If the child spawned a grandchild that
        // inherited the handle and outlives it, `reader.read()` never returns and
        // an unbounded join would hang run() forever — running=false and the
        // delivery join below would never run. Time-box the wait; on timeout we
        // proceed (losing only the launch-failure tail) and let Drop kill the
        // whole tree, which closes the pipe and lets the orphaned reader wind
        // down. The normal EOF lag is milliseconds, so this only trips on a
        // genuinely stuck grandchild.
        join_with_timeout(reader_handle, Duration::from_secs(2));

        // Record a precise launch-failure (exited-before-bind) BEFORE flipping
        // running=false, mirroring the Unix proxy: finalize records the real
        // evidence first, and the shared launch_phase flag then suppresses a
        // duplicate generic failure from delivery cleanup. Skipped on a kill so
        // a manual `hcom kill` is never recorded as a launch failure.
        if !EXIT_WAS_KILLED.load(Ordering::Acquire) {
            let tail = self.last_tail.read().ok().and_then(|g| g.clone());
            shared::finalize_launch_failure_after_exit(
                self.config.instance_name.as_deref(),
                tail.as_deref(),
                &self.launch_phase_active,
                startup_time.elapsed(),
                exit_code,
            );
        }

        // Signal threads to stop and wake the delivery loop's notify select.
        self.running.store(false, Ordering::Release);
        let port = self.notify_port.load(Ordering::Acquire);
        if port != 0 {
            let _ = std::net::TcpStream::connect(("127.0.0.1", port));
        }

        // Join the coordinator BEFORE taking `delivery_handle`. The coordinator
        // is the sole writer of that handle; joining it establishes the
        // happens-before that makes its store visible here. Its shutdown-time
        // final start attempt can wait up to the 5s init recv_timeout, so bound
        // the join at 6s (common case: coordinator already returned → instant).
        join_with_timeout(coordinator_handle, Duration::from_secs(6));

        // Recover the guard even if the mutex was poisoned: a panic elsewhere
        // must not strand the delivery thread unjoined. The handle is the only
        // thing behind this lock, so the (possibly stale) inner value is safe to
        // take.
        let handle = self
            .delivery_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }

        // If delivery initialization failed, surface it as a nonzero exit.
        if self.launch_failed.load(Ordering::Acquire) {
            anyhow::bail!("delivery initialization failed");
        }

        Ok(exit_code)
    }

    /// Own the delivery-thread startup decision. Polls every 50ms and starts
    /// delivery exactly once when the tool is ready, the start-timeout elapses,
    /// or shutdown begins. The reader thread cannot own this: it blocks in
    /// `read()` and so cannot run a timer while the child is idle (#8).
    ///
    /// Stores the delivery-thread handle (through a poisoned mutex too) so it is
    /// never dropped where `run()` can't join it. On [`shared::DeliveryStart::
    /// Pending`] it keeps the handle and stops retrying — a retry would spawn a
    /// second delivery thread and double-deliver (#6). A transient up-front
    /// `Err` sets `launch_failed` and retries on the next tick.
    fn spawn_delivery_coordinator(&self, inject_port: u16) -> JoinHandle<()> {
        let running = self.running.clone();
        let ready_signaled = self.ready_signaled.clone();
        let screen_state = self.screen_state.clone();
        let launch_phase = self.launch_phase_active.clone();
        let target = self.config.target.clone();
        let instance = self.config.instance_name.clone();
        let grok_acp = self.config.grok_acp.clone();
        let current_name = self.current_name.clone();
        let current_status = self.current_status.clone();
        let notify_port = self.notify_port.clone();
        let delivery_handle = self.delivery_handle.clone();
        let launch_failed = self.launch_failed.clone();
        let timeout = self.config.target.delivery_start_timeout();
        thread::spawn(move || {
            let startup = Instant::now();
            loop {
                let shutting_down = !running.load(Ordering::Acquire);
                let should_start = shared::should_start_delivery(
                    ready_signaled.load(Ordering::Acquire),
                    startup.elapsed(),
                    timeout,
                    shutting_down,
                );
                if should_start {
                    match shared::start_delivery_thread(
                        instance.as_deref(),
                        running.clone(),
                        screen_state.clone(),
                        launch_phase.clone(),
                        inject_port,
                        target.clone(),
                        notify_port.clone(),
                        current_name.clone(),
                        current_status.clone(),
                        None,
                        grok_acp.clone(),
                    ) {
                        Ok(shared::DeliveryStart::Started(h)) => {
                            *delivery_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
                            // Clear any launch_failed from an earlier failed
                            // attempt so a transient error doesn't poison the
                            // exit code once a retry wins.
                            launch_failed.store(false, Ordering::Release);
                            return;
                        }
                        Ok(shared::DeliveryStart::Pending(h, init_rx)) => {
                            // Keep the handle and wait (bounded) for the
                            // timed-out initializer's eventual result. A
                            // definitive failure wakes run(), which kills and
                            // reaps the ConPTY child instead of waiting for its
                            // session to end naturally. Bounded rather than a
                            // plain recv() so a permanently stuck initializer
                            // still resolves this thread instead of blocking it
                            // forever.
                            *delivery_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
                            if !matches!(init_rx.recv_timeout(Duration::from_secs(30)), Ok(Ok(())))
                            {
                                launch_failed.store(true, Ordering::Release);
                                running.store(false, Ordering::Release);
                            }
                            return;
                        }
                        Ok(shared::DeliveryStart::Disabled) => return,
                        Err(_transient) => {
                            launch_failed.store(true, Ordering::Release);
                            // A shutdown tick gets exactly one last attempt; a
                            // running tick falls through to retry.
                            if shutting_down {
                                return;
                            }
                        }
                    }
                }
                if shutting_down {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
        })
    }

    /// Poll the outer terminal size and forward changes to the ConPTY and the
    /// screen tracker. Windows has no SIGWINCH, so this ~200ms poll is the
    /// Windows counterpart to the Unix proxy's `forward_winsize`.
    fn spawn_resize_watcher(&self) {
        let running = self.running.clone();
        let master = self.master.clone();
        let pending_resize = self.pending_resize.clone();
        let (mut last_cols, mut last_rows) = (self.cols, self.rows);
        thread::spawn(move || {
            while running.load(Ordering::Acquire) {
                if let Ok((cols, rows)) = crossterm::terminal::size()
                    && (cols, rows) != (last_cols, last_rows)
                {
                    last_cols = cols;
                    last_rows = rows;
                    if let Ok(master) = master.lock() {
                        let _ = master.resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                    // Hand the new size to the reader thread, which owns the
                    // ScreenTracker and applies it before the next `process`.
                    if let Ok(mut g) = pending_resize.write() {
                        *g = Some((rows, cols));
                    }
                }
                thread::sleep(Duration::from_millis(200));
            }
        });
    }

    /// PTY output → our stdout, feeding the screen tracker and the shared
    /// screen state the delivery loop reads. portable-pty's reader blocks, so a
    /// dedicated thread replaces the Unix poll loop.
    ///
    /// This thread also owns the Windows equivalents of the Unix poll loop's
    /// per-iteration work: refreshing the shared delivery state (via
    /// `shared::update_delivery_state`), latching the ready signal for the
    /// delivery coordinator, consuming approval-clear requests from the
    /// stdin/inject threads, applying pending resizes, refreshing the screen
    /// snapshot for `hcom term` queries, answering the child's cursor-position
    /// query when headless, and emitting title OSC updates on status/name
    /// changes.
    ///
    /// Returns the thread's `JoinHandle` so `run()` can join it after the child
    /// exits and before reading `last_tail`. During the launch phase the reader
    /// writes `last_tail` whenever output goes quiet, and again at PTY EOF;
    /// ConPTY may not reach EOF until the pseudoconsole closes, so the quiet-
    /// period capture is what usually supplies the launch-failure tail.
    ///
    /// Returns `Err` if the ConPTY reader can't be cloned (or the master mutex is
    /// poisoned): without a reader there is no screen tracking, no delivery
    /// coordination, and no launch-failure tail, so a silently degraded session
    /// is worse than a loud failure (#23). `run()` calls this with `?` before
    /// spawning any other thread, so an early bail tears nothing half-up.
    fn spawn_reader_thread(&self, inject_port: u16) -> Result<JoinHandle<()>> {
        let reader = match self.master.lock() {
            Ok(master) => master
                .try_clone_reader()
                .context("ConPTY try_clone_reader failed; cannot track screen")?,
            Err(_) => anyhow::bail!("ConPTY master mutex poisoned; cannot spawn reader"),
        };
        let screen_state = self.screen_state.clone();
        let launch_phase = self.launch_phase_active.clone();
        let target = self.config.target.clone();
        let ready_patterns = self.config.ready_patterns.clone();
        let instance = self.config.instance_name.clone();
        let current_name = self.current_name.clone();
        let current_status = self.current_status.clone();
        let approval_clear_requested = self.approval_clear_requested.clone();
        let pending_resize = self.pending_resize.clone();
        let last_tail = self.last_tail.clone();
        let ready_signaled = self.ready_signaled.clone();
        let screen_snapshot = self.screen_snapshot.clone();
        let writer = self.writer.clone();
        let mouse_tracking = self.mouse_tracking.clone();
        let (rows, cols) = (self.rows, self.cols);
        let console_input = self.config.target.console_input();
        let mut trace = shared::StartupTrace::new(self.spawned_at, instance.as_deref());

        // Producer: owns the ConPTY reader and blocks in read(), forwarding raw
        // chunks over a channel. This exists so the consumer loop below can wait
        // with a bounded timeout (`recv_timeout`) instead of blocking in read()
        // forever — that bounded wait is what lets it render a trailing `hcom
        // term` snapshot ~120ms after an idle agent's output stops (#4); a plain
        // blocking read() could not, since it never returns while the child is
        // idle. Detached like the old single reader was: on the orphaned-
        // grandchild EOF-lag case (see run()) it may stay blocked in read(), but
        // it holds no lock and process::exit reaps it.
        // Bounded so the producer blocks (SyncSender::send) when the consumer
        // lags, restoring the child backpressure the pre-split single-thread
        // reader had implicitly (read + stdout write on one thread). Unbounded
        // here would let the producer drain the ConPTY into RAM without limit
        // under sustained output or a stalled headless stdout. ~256 * 8KiB
        // chunks (~2MiB) absorbs normal bursts; disconnect/recv_timeout
        // semantics are identical to an unbounded channel.
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(256);
        thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF: child exited / PTY closed
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break; // consumer gone
                        }
                    }
                    Err(_) => break,
                }
            }
            // Dropping `tx` here signals EOF/read-error to the consumer, which
            // then captures the launch-failure tail and does a final snapshot.
        });

        Ok(thread::spawn(move || {
            let mut screen =
                ScreenTracker::new_with_instance(rows, cols, &ready_patterns, instance.as_deref());
            let stdout = StdoutQueue::spawn(std::io::stdout(), |b| trace::bytes(Hop::Stdout, b));
            let mut filter = shared::OutputModeFilter::default();
            let mut scratch: Vec<u8> = Vec::with_capacity(8192);

            // In interactive mode stdout is a real console, so the outer terminal
            // answers the child's cursor-position query itself — we must not also
            // answer. Headless (piped/no console) there is no terminal to answer,
            // so the reader replies on the child's behalf or startup hangs (#1).
            let headless = !std::io::stdout().is_terminal();
            let mut last_name = String::new();
            let mut last_status = String::new();
            // Terminal-title behavior. Read once; the child title comes from the
            // reader-owned `screen`, no extra lock needed. In `Off` the filter
            // passes the tool's own titles through and we write nothing.
            let title_mode = crate::config::HcomConfig::load(None)
                .map(|c| crate::shared::TitleMode::from_config(&c.title_mode))
                .unwrap_or(crate::shared::TitleMode::Combined);
            let title_enabled = title_mode != crate::shared::TitleMode::Off;
            filter.set_passthrough_titles(!title_enabled);
            // A relay reading input records can't receive the terminal's
            // cursor reports (see set_local_cursor_reports).
            let local_cursor_reports = !headless && console_input == ConsoleInput::Records;
            filter.set_local_cursor_reports(local_cursor_reports);
            let mut last_child = String::new();

            // `hcom term` snapshot refresh (see should_refresh_snapshot). Under
            // sustained output we render at most once per SNAPSHOT_THROTTLE;
            // chunks skipped by that throttle set `dirty`. When output then goes
            // quiet, recv_timeout fires after SNAPSHOT_DEBOUNCE and we render one
            // final dump, so an idle agent's last frame is current within ~120ms
            // (#4). At most one extra dump per quiet period, zero under sustained
            // output.
            const SNAPSHOT_THROTTLE: Duration = Duration::from_millis(100);
            const SNAPSHOT_DEBOUNCE: Duration = Duration::from_millis(120);
            let mut last_snapshot = Instant::now();
            let mut dirty = false;
            // Launch-failure tail, captured once output goes quiet. ConPTY keeps
            // the output pipe open after the child exits (until the pseudoconsole
            // is closed), so waiting for EOF to capture it loses the race with
            // run()'s bounded reader join — and with it the tool's error text.
            let mut tail_stale = false;
            let refresh = |screen: &ScreenTracker| {
                if let Ok(mut s) = screen_snapshot.write() {
                    *s = screen.get_screen_dump(target.name(), inject_port);
                }
            };
            let publish =
                |a: bool| shared::publish_approval_status(a, instance.as_deref(), &current_status);

            loop {
                match rx.recv_timeout(SNAPSHOT_DEBOUNCE) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        // Output has been quiet for SNAPSHOT_DEBOUNCE. If a frame
                        // was deferred by the throttle, render it now so an idle
                        // agent's final frame is current for `hcom term`.
                        if dirty {
                            refresh(&screen);
                            last_snapshot = Instant::now();
                            dirty = false;
                        }
                        if tail_stale && launch_phase.load(Ordering::Acquire) {
                            if let Ok(mut g) = last_tail.write() {
                                *g = screen.visible_tail(8, 1000);
                            }
                            tail_stale = false;
                        }
                        // Mirror the Unix poll loop's idle-path hooks (src/pty/mod.rs):
                        // without this, `update_delivery_state` on Windows only ever
                        // runs from the `Ok(data)` branch above, so once output goes
                        // quiet it never re-evaluates. A transient misread on the last
                        // chunk before quiescence — e.g. a mid-redraw frame where the
                        // gate sees leftover non-dim text on the prompt row — then
                        // latches forever instead of self-correcting within one poll,
                        // stalling delivery indefinitely until new output arrives.
                        if ready_signaled.load(Ordering::Acquire) {
                            shared::update_delivery_state(
                                &screen_state,
                                &screen,
                                &target,
                                &launch_phase,
                                &publish,
                            );
                        }
                        if !ready_signaled.load(Ordering::Acquire) {
                            trace.check_not_ready();
                        }
                        screen.check_debug_flag();
                        screen.check_periodic_dump(
                            target.name(),
                            inject_port,
                            "Periodic dump (win reader loop)",
                        );
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        // EOF / read error: capture the visible tail so run() can
                        // build the launch-failure diagnostic before the screen is
                        // gone, and render the final frame for a late `hcom term`.
                        if let Ok(mut g) = last_tail.write() {
                            *g = screen.visible_tail(8, 1000);
                        }
                        refresh(&screen);
                        break; // child exited / PTY closed
                    }
                    Ok(data) => {
                        let data = data.as_slice();
                        trace::bytes(Hop::Output, data);
                        trace.on_output(data);
                        // A genuine keystroke / injected answer flagged a pending
                        // approval for clearing; the reader owns the tracker.
                        if approval_clear_requested.swap(false, Ordering::AcqRel) {
                            screen.clear_approval();
                        }
                        // Apply a pending terminal resize before processing this
                        // frame so the screen model matches the new geometry.
                        if let Some((r, c)) = pending_resize.write().ok().and_then(|mut g| g.take())
                        {
                            trace::note(Hop::Mode, &format!("resize {c}x{r}"));
                            screen.resize(r, c);
                        }

                        // Strip the child's Win32-input/focus mode-set sequences
                        // before they reach the *outer* terminal (see
                        // OutputModeFilter); otherwise the outer terminal answers
                        // the child's DSR query in Win32 input-record encoding,
                        // which the child can't parse, and startup hangs.
                        scratch.clear();
                        filter.filter(data, &mut scratch);
                        let queries = filter.take_queries();
                        let mut written = 0;
                        for &(query, at) in &queries {
                            let reply = if headless {
                                // No outer terminal saw the ConPTY's startup
                                // queries; answer them to unblock its console
                                // initialization.
                                Some(query.headless_reply().to_vec())
                            } else if local_cursor_reports
                                && query == shared::TerminalQuery::CursorPosition
                            {
                                // Answer once everything before the query is on
                                // the console, so the cursor is where it asked.
                                stdout.write(&scratch[written..at]);
                                stdout.barrier();
                                written = at;
                                console::cursor_position_report()
                            } else {
                                // The real terminal answers.
                                None
                            };
                            if let Some(reply) = reply
                                && let Ok(mut w) = writer.lock()
                            {
                                trace::bytes(Hop::Input, &reply);
                                let _ = w.write_all(&reply);
                                let _ = w.flush();
                                if query == shared::TerminalQuery::CursorPosition {
                                    trace.on_dsr_answered();
                                }
                            }
                        }
                        stdout.write(&scratch[written..]);

                        screen.process(data);
                        mouse_tracking.store(screen.mouse_tracking() as u8, Ordering::Relaxed);

                        // Refresh the `hcom term` snapshot, throttled to ≤10Hz so
                        // heavy output doesn't spend the reader in screen dumps
                        // (~150µs each). A chunk skipped here is marked dirty and
                        // captured by the trailing-edge Timeout branch above once
                        // output stops.
                        if shared::should_refresh_snapshot(
                            last_snapshot.elapsed(),
                            SNAPSHOT_THROTTLE,
                        ) {
                            refresh(&screen);
                            last_snapshot = Instant::now();
                            dirty = false;
                        } else {
                            dirty = true;
                        }
                        tail_stale = true;

                        shared::update_delivery_state(
                            &screen_state,
                            &screen,
                            &target,
                            &launch_phase,
                            &publish,
                        );

                        // Latch the ready signal for the delivery coordinator. A
                        // latch never regresses (unlike re-reading a screen flag
                        // that can flicker during redraws).
                        if !ready_signaled.load(Ordering::Acquire) && screen.is_ready() {
                            ready_signaled.store(true, Ordering::Release);
                        }

                        // Title OSC update. The output filter tracks complete
                        // CSI/OSC/DCS/UTF-8 boundaries, so this cannot split the
                        // child's byte stream. Never emit terminal metadata into
                        // redirected/headless stdout.
                        //
                        // Compare under the read guards against the last-written
                        // values and only build/clone when something actually
                        // changed. This runs on every at-ground chunk (frequent
                        // under heavy output) and name/status rarely change, so
                        // the common path holds the two read locks briefly but
                        // allocates nothing.
                        let child = if title_mode == crate::shared::TitleMode::Combined {
                            screen.child_title().unwrap_or("")
                        } else {
                            ""
                        };
                        if !headless
                            && title_enabled
                            && filter.title_write_safe()
                            && let (Ok(name), Ok(status)) =
                                (current_name.read(), current_status.read())
                            && !name.is_empty()
                            && (*name != last_name || *status != last_status || child != last_child)
                        {
                            let child_opt = (!child.is_empty()).then_some(child);
                            let esc = shared::build_title_escape(
                                &name,
                                &status,
                                target.name(),
                                title_mode,
                                child_opt,
                            );
                            stdout.write(esc.as_bytes());
                            last_child.clear();
                            last_child.push_str(child);
                            last_name = name.clone();
                            last_status = status.clone();
                        }
                    }
                }
            }
            // Get the child's last frame onto the console before run() moves on
            // to exit and restores the console modes. Bounded below run()'s 2s
            // join on this thread: past it, what's still queued is dropped
            // rather than written raw after the restore.
            if !stdout.finish(Duration::from_millis(1500)) {
                log_warn(
                    "native",
                    "win.stdout",
                    "console not draining at exit; dropped queued output",
                );
            }
            // Do NOT store running=false here. Letting run() be the sole writer
            // ensures EXIT_WAS_KILLED is committed before the delivery thread
            // sees running=false and enters cleanup. If the reader set it first,
            // the delivery loop could read EXIT_WAS_KILLED=false and record
            // exit:closed even when the child was killed via `hcom kill`.
        }))
    }

    /// Our stdin → PTY input. Intentionally detached and never joined.
    ///
    /// A console stdin is read as input records and re-encoded (see
    /// `console_input`); `std::io::stdin()` can't be used there, since on a
    /// console it reports end-of-file for a read that ends in Ctrl+Z, which
    /// used to stop all input for the rest of the session. A redirected stdin
    /// is forwarded byte for byte.
    ///
    /// The `running` check only catches shutdown *between* reads; a pipe
    /// `read()` already blocked when the child exits cannot be interrupted and
    /// outlives the child. This does not leak: `main` calls
    /// `std::process::exit` immediately after `run` returns (and `Proxy::drop`),
    /// which terminates the process and reaps this thread even mid-read. The
    /// thread holds no lock across the blocking read, so it cannot wedge
    /// cleanup. If `Proxy` ever gains a caller that keeps running after `run`
    /// returns, this read would need an explicit interrupt (e.g.
    /// `CancelSynchronousIo`).
    fn spawn_stdin_thread(&self) {
        let writer = self.writer.clone();
        let running = self.running.clone();
        let target = self.config.target.clone();
        let screen_state = self.screen_state.clone();
        let current_status = self.current_status.clone();
        let instance = self.config.instance_name.clone();
        let approval_clear_requested = self.approval_clear_requested.clone();
        let console_input = self.config.target.console_input();
        let mouse_tracking = self.mouse_tracking.clone();
        thread::spawn(move || {
            let forward = |bytes: &[u8]| {
                trace::bytes(Hop::Input, bytes);
                if let Ok(mut w) = writer.lock() {
                    let _ = w.write_all(bytes);
                    let _ = w.flush();
                }
                // A genuine keystroke answering a title-detected approval
                // clears it immediately. Record the cleared edge against shared
                // state; the reader thread owns the tracker, so request a
                // tracker-clear via the atomic it consumes — but ONLY when an
                // approval was actually standing. `clear_approval()` wipes the
                // OSC scrape buffer, so requesting it on every keystroke would
                // let a routine keypress race out an approval edge arriving in
                // the same window.
                let publish = |a: bool| {
                    shared::publish_approval_status(a, instance.as_deref(), &current_status)
                };
                if shared::note_user_keystroke(&target, &screen_state, &publish) {
                    approval_clear_requested.store(true, Ordering::Release);
                }
            };

            if let Some(mut input) = console::InputReader::open(console_input, mouse_tracking) {
                let mut out = Vec::new();
                while running.load(Ordering::Acquire) {
                    out.clear();
                    // Bounded wait so the loop sees shutdown.
                    if let Err(error) = input.read(100, &mut out) {
                        log_error(
                            "native",
                            "win.stdin",
                            &format!("console input read failed: {error}"),
                        );
                        break;
                    }
                    if !out.is_empty() {
                        forward(&out);
                    }
                }
                return;
            }

            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 4096];
            while running.load(Ordering::Acquire) {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Err(error) => {
                        log_error(
                            "native",
                            "win.stdin",
                            &format!("stdin read failed: {error}"),
                        );
                        break;
                    }
                    Ok(n) => forward(&buf[..n]),
                }
            }
        });
    }

    /// InjectServer → PTY input. Polls for inject connections (the delivery loop
    /// and `hcom term inject` connect here) and writes the text to the ConPTY.
    fn spawn_inject_thread(&self, mut inject_server: InjectServer) {
        let writer = self.writer.clone();
        let running = self.running.clone();
        let target = self.config.target.clone();
        let screen_state = self.screen_state.clone();
        let current_status = self.current_status.clone();
        let instance = self.config.instance_name.clone();
        let approval_clear_requested = self.approval_clear_requested.clone();
        let screen_snapshot = self.screen_snapshot.clone();
        // A client stuck Pending past this long (connected but never sending
        // EOF/erroring — e.g. a killed-without-cleanup peer) stops blocking
        // later-queued clients such as independent `hcom term` screen queries.
        const STALL_TIMEOUT: Duration = Duration::from_secs(2);
        thread::spawn(move || {
            let mut stalled_since: Option<Instant> = None;
            while running.load(Ordering::Acquire) {
                // Drain the accept queue.
                while matches!(inject_server.accept(), Ok(true)) {}
                // Preserve connection order. `hcom term inject --enter` sends
                // text and Enter on two consecutive TCP connections; processing
                // newest-first delivers Enter before the text and leaves the
                // prompt filled but unsubmitted. Completed clients remove
                // themselves, so keep the same index after completion.
                let mut index = 0;
                while index < inject_server.client_count() {
                    let completed = match inject_server.read_client(index) {
                        Ok(InjectResult::Inject(text)) => {
                            trace::bytes(Hop::Inject, text.as_bytes());
                            if let Ok(mut w) = writer.lock() {
                                let _ = w.write_all(text.as_bytes());
                                let _ = w.flush();
                            }
                            // An injected answer reaches the PTY directly and
                            // bypasses the stdin handler. Publish the cleared
                            // edge synchronously (while the row is still blocked)
                            // and request a tracker-clear from the reader thread.
                            let publish = |a: bool| {
                                shared::publish_approval_status(
                                    a,
                                    instance.as_deref(),
                                    &current_status,
                                )
                            };
                            if shared::clear_injected_approval_state(
                                &target,
                                &screen_state,
                                &publish,
                            ) {
                                approval_clear_requested.store(true, Ordering::Release);
                            }
                            true
                        }
                        // Screen queries (`hcom term`) are served from the
                        // eagerly-maintained snapshot the reader refreshes, so an
                        // idle agent (reader blocked in read()) still answers
                        // immediately rather than hanging or returning "" (#4).
                        Ok(InjectResult::Query(q)) => {
                            match q.command {
                                QueryCommand::Screen => {
                                    let dump = screen_snapshot
                                        .read()
                                        .map(|s| s.clone())
                                        .unwrap_or_default();
                                    q.respond(&dump);
                                }
                                QueryCommand::Unknown => q.respond("error: unknown command\n"),
                            }
                            true
                        }
                        Ok(InjectResult::Pending) => false,
                        Err(_) => true,
                    };
                    if completed {
                        stalled_since = None;
                        // A completed client at `index` was removed, shifting
                        // the next client into this slot; retry it here.
                        continue;
                    }
                    if index == 0 {
                        let stalled = stalled_since.get_or_insert_with(Instant::now);
                        if stalled.elapsed() < STALL_TIMEOUT {
                            // Strict FIFO: do not let a later, already-complete
                            // Enter frame overtake an earlier text frame that
                            // has not exposed EOF yet.
                            break;
                        }
                        // Client 0 has been stuck long enough that it's
                        // unlikely to ever complete; stop enforcing strict
                        // order so later, independent clients aren't starved.
                    }
                    index += 1;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
    }
}

/// Block until `child` exits, returning its exit code.
///
/// This wait spans the entire interactive session, so it is deliberately
/// unbounded: from here a healthy child running for hours is indistinguishable
/// from a hung one, and any watchdog timeout at this spot force-kills every
/// session once the timer elapses (a 5s escalation here used to kill Claude a
/// few seconds after launch). The Unix proxy's 5s SIGKILL escalation is not an
/// analogue for this wait — it lives in `drain_and_wait_child`, which runs
/// only after the PTY signals EOF/HUP (the child is already tearing down) and
/// exists to break the full-PTY-buffer write deadlock; that deadlock cannot
/// happen here because the reader thread keeps draining the ConPTY pipe
/// concurrently. A genuinely stuck child is still covered: `hcom kill`
/// terminates the tree directly (releasing this wait), and Drop's kill_group
/// plus the job object's kill-on-close reap anything left.
///
/// Never returns an `Err` (unlike a bare `child.wait()?`): a wait failure is
/// logged and treated as an unknown exit code, since propagating it via `?`
/// would skip run()'s cleanup (stop signal, notify wake, delivery-thread
/// join) — that cleanup must run regardless of whether waiting on the child
/// itself succeeded.
fn wait_child_blocking(child: &mut (dyn portable_pty::Child + Send + Sync)) -> i32 {
    match child.wait() {
        Ok(status) => status.exit_code() as i32,
        Err(e) => {
            log_error("native", "win.wait", &format!("child wait failed: {e}"));
            1
        }
    }
}

/// Join `handle`, but give up after `timeout` and return regardless.
///
/// `JoinHandle::join` has no timeout, so we hand the handle to a short-lived
/// joiner thread and wait on a channel. On timeout the joiner thread is left
/// running (detached) — it owns `handle` and completes on its own once the
/// reader finally exits (e.g. after Drop kills the process tree and the ConPTY
/// pipe closes). Dropping the receiver here does not abort it. The joiner holds
/// no lock, so a lingering one cannot wedge the rest of shutdown.
fn join_with_timeout(handle: JoinHandle<()>, timeout: Duration) {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = handle.join();
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(timeout);
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        // Reap the child and any descendants it spawned (race-free snapshot
        // walk). The `_job` field's kill-on-close is the backstop for the case
        // where Drop never runs.
        //
        // `child.process_id()` keeps returning `Some` even after the child has
        // already exited normally — it's a fresh `GetProcessId()` on our still-
        // open handle, not a liveness check — so a bare `is_some()` can't tell
        // "run() exited early and this child still needs killing" apart from
        // "run() already waited on and reaped this child." Check `try_wait()`
        // first (side-effect-free, safe to call again after run()'s own wait):
        // today Drop only ever runs after run()'s wait/cleanup has completed,
        // so this has no observable effect, but it guards against a future
        // reordering marking a normal exit as `exit:killed`.
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            if let Some(pid) = self.child.process_id() {
                // Mark as killed so the delivery thread records exit:killed if
                // it is still running. Do not also call child.kill() below: it
                // would send a second, competing TerminateProcess to the same
                // PID and can overwrite the hcom-kill sentinel exit code (130)
                // that kill_group's terminate_win already set — the same race
                // fixed in kill_child_group (sys/process.rs).
                EXIT_WAS_KILLED.store(true, Ordering::Release);
                let _ = crate::sys::process::kill_group(pid);
            } else {
                let _ = self.child.kill();
            }
        }
        if let Some(ref instance_name) = self.config.instance_name
            && let Ok(db) = HcomDb::open()
        {
            let _ = db.delete_notify_endpoint(instance_name, "inject");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_cmd_script, wait_child_blocking};
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    /// Spawn `argv` under a ConPTY. Returns the master too: dropping it closes
    /// the ConPTY and kills the child, so tests must keep it alive while
    /// waiting.
    ///
    /// A service thread plays the role of the outer terminal, which this
    /// headless test doesn't have — without it the child hangs before ever
    /// exiting (even `cmd /c exit 42`; both tests once ran 60s+ in CI):
    /// - It drains the master output so the ConPTY output pipe never fills.
    /// - It answers conhost's cursor-position query. portable-pty creates the
    ///   ConPTY with `PSEUDOCONSOLE_INHERIT_CURSOR`, so conhost emits `ESC[6n`
    ///   and blocks console initialization — and with it every console API
    ///   call the child makes — until an `ESC[<r>;<c>R` report arrives on the
    ///   input pipe. In production the real terminal answers automatically.
    fn spawn_in_conpty(
        argv: &[&str],
    ) -> (
        Box<dyn portable_pty::MasterPty + Send>,
        Box<dyn portable_pty::Child + Send + Sync>,
    ) {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty (ConPTY) failed");
        let mut cmd = CommandBuilder::new(argv[0]);
        cmd.args(&argv[1..]);
        let child = pair.slave.spawn_command(cmd).expect("spawn failed");
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .expect("try_clone_reader failed");
        let mut writer = pair.master.take_writer().expect("take_writer failed");
        std::thread::spawn(move || {
            let mut sink = [0u8; 8192];
            let mut replied = false;
            loop {
                match reader.read(&mut sink) {
                    Ok(n) if n > 0 => {
                        if !replied && sink[..n].windows(4).any(|w| w == b"\x1b[6n") {
                            let _ = writer.write_all(b"\x1b[1;1R");
                            let _ = writer.flush();
                            replied = true;
                        }
                    }
                    _ => break,
                }
            }
        });
        (pair.master, child)
    }

    /// Run `wait_child_blocking` on a helper thread with a hard deadline, so a
    /// hung child fails the test in bounded time instead of stalling CI until
    /// the job timeout.
    fn wait_with_deadline(
        mut child: Box<dyn portable_pty::Child + Send + Sync>,
        deadline: Duration,
    ) -> i32 {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(wait_child_blocking(child.as_mut()));
        });
        rx.recv_timeout(deadline)
            .expect("child did not exit within the test deadline")
    }

    #[test]
    fn wait_child_blocking_returns_the_child_exit_code() {
        let (_master, child) = spawn_in_conpty(&["cmd.exe", "/c", "exit 42"]);
        assert_eq!(wait_with_deadline(child, Duration::from_secs(30)), 42);
    }

    #[test]
    fn wait_child_blocking_does_not_kill_a_healthy_long_running_child() {
        // Regression: a 5s watchdog in run()'s child wait used to kill_group
        // every session a few seconds after launch, forcing the kill sentinel
        // (130) as the exit code. A child that outlives that window must still
        // exit on its own, with its own code.
        let (_master, child) = spawn_in_conpty(&["ping", "-n", "7", "127.0.0.1"]);
        let start = Instant::now();
        let code = wait_with_deadline(child, Duration::from_secs(60));
        let elapsed = start.elapsed();
        assert_eq!(code, 0, "child should exit on its own, not be killed");
        assert!(
            elapsed >= Duration::from_secs(5),
            "child exited after {elapsed:?}; expected it to outlive the former 5s watchdog window"
        );
    }

    #[test]
    fn is_cmd_script_matches_cmd_and_bat_case_insensitively() {
        assert!(is_cmd_script("gemini.cmd"));
        assert!(is_cmd_script(r"C:\Users\me\AppData\Roaming\npm\codex.CMD"));
        assert!(is_cmd_script("run.bat"));
        assert!(is_cmd_script("RUN.BAT"));
    }

    #[test]
    fn is_cmd_script_rejects_other_extensions() {
        assert!(!is_cmd_script("claude.exe"));
        assert!(!is_cmd_script("gemini"));
        assert!(!is_cmd_script("script.ps1"));
        assert!(!is_cmd_script("noext."));
    }

    // OutputModeFilter and its tests now live in `super::shared` so they run on
    // the (non-Windows) host gate rather than being cross-compiled only.
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

/// Windows console raw mode + VT output, restored on drop, and the console
/// input reader.
mod console {
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS,
        ENABLE_LINE_INPUT, ENABLE_MOUSE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_QUICK_EDIT_MODE,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
        GetConsoleScreenBufferInfo, GetStdHandle, INPUT_RECORD, KEY_EVENT, MOUSE_EVENT,
        ReadConsoleInputW, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, Ordering};

    use super::super::console_input::{InputEncoder, KeyRecord, MouseRecord, MouseTracking};
    use super::super::trace::{self, Hop};
    use crate::integration_spec::ConsoleInput;

    fn screen_info() -> Option<CONSOLE_SCREEN_BUFFER_INFO> {
        // SAFETY: CONSOLE_SCREEN_BUFFER_INFO is plain data; the call fills it
        // for our own stdout handle.
        unsafe {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            (GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) != 0)
                .then_some(info)
        }
    }

    /// `\x1b[<row>;<col>R` for our console's cursor, relative to the visible
    /// window — what the terminal would reply to `\x1b[6n`.
    pub fn cursor_position_report() -> Option<Vec<u8>> {
        let info = screen_info()?;
        let row = i32::from(info.dwCursorPosition.Y) - i32::from(info.srWindow.Top) + 1;
        let col = i32::from(info.dwCursorPosition.X) - i32::from(info.srWindow.Left) + 1;
        Some(format!("\x1b[{};{}R", row.max(1), col.max(1)).into_bytes())
    }

    /// Reads the console's input records and encodes them for the ConPTY (see
    /// `console_input`).
    pub struct InputReader {
        handle: isize,
        records: Vec<INPUT_RECORD>,
        encoder: InputEncoder,
        input: ConsoleInput,
        /// The child's mouse tracking, from the reader thread.
        mouse_tracking: Arc<AtomicU8>,
        /// Vt mode: whether we turned console mouse input on for the child.
        mouse_input: bool,
        /// The mouse-input and QuickEdit bits before we changed them.
        mouse_bits: CONSOLE_MODE,
    }

    impl InputReader {
        /// `None` when stdin isn't a console.
        pub fn open(input: ConsoleInput, mouse_tracking: Arc<AtomicU8>) -> Option<Self> {
            // SAFETY: querying the mode of our own std handle.
            let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) } as isize;
            let mut mode: CONSOLE_MODE = 0;
            // SAFETY: as above; `mode` is a valid out-pointer.
            if unsafe { GetConsoleMode(handle as _, &mut mode) } == 0 {
                return None;
            }
            Some(Self {
                handle,
                // SAFETY: INPUT_RECORD is plain data; all-zero is valid.
                records: vec![unsafe { std::mem::zeroed() }; 128],
                encoder: InputEncoder::new(input),
                input,
                mouse_tracking,
                mouse_input: false,
                mouse_bits: mode & (ENABLE_MOUSE_INPUT | ENABLE_QUICK_EDIT_MODE),
            })
        }

        /// Vt mode: while the child tracks the mouse, take mouse input the
        /// way a child run directly would (QuickEdit off, mouse input on). A
        /// console host that parses the terminal's mouse reports into records
        /// only delivers them in that mode; the encoder turns them back into
        /// reports, unless reports already arrive as text. The mode stays
        /// put until tracking ends: a host that sends text may need it too.
        fn sync_mouse_input(&mut self, tracking: MouseTracking) {
            let want = self.input == ConsoleInput::Vt && tracking != MouseTracking::Off;
            if want == self.mouse_input {
                return;
            }
            self.mouse_input = want;
            let mut mode: CONSOLE_MODE = 0;
            // SAFETY: our own console input handle; `mode` is a valid out-pointer.
            if unsafe { GetConsoleMode(self.handle as _, &mut mode) } == 0 {
                return;
            }
            let bits = ENABLE_MOUSE_INPUT | ENABLE_QUICK_EDIT_MODE;
            let new = if want {
                (mode & !bits) | ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS
            } else {
                (mode & !bits) | self.mouse_bits | ENABLE_EXTENDED_FLAGS
            };
            // SAFETY: as above.
            unsafe { SetConsoleMode(self.handle as _, new) };
            trace::note(
                Hop::Mode,
                &format!("stdin {mode:#06x} -> {new:#06x} (mouse tracking {tracking:?})"),
            );
        }

        /// Wait up to `timeout_ms` for input and append its encoding to `out`.
        pub fn read(&mut self, timeout_ms: u32, out: &mut Vec<u8>) -> std::io::Result<()> {
            let tracking = MouseTracking::from_u8(self.mouse_tracking.load(Ordering::Relaxed));
            self.sync_mouse_input(tracking);
            // SAFETY: waiting on our own console input handle.
            match unsafe { WaitForSingleObject(self.handle as _, timeout_ms) } {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => return Ok(()),
                _ => return Err(std::io::Error::last_os_error()),
            }
            let mut n = 0u32;
            // SAFETY: `records` holds `len` writable records; `n` receives the
            // count actually read.
            let ok = unsafe {
                ReadConsoleInputW(
                    self.handle as _,
                    self.records.as_mut_ptr(),
                    self.records.len() as u32,
                    &mut n,
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Mouse positions are buffer coordinates; SGR reports are relative
            // to the visible window. Looked up once per batch, if needed.
            let mut window_origin = None;
            for record in &self.records[..n as usize] {
                // SAFETY: the union member read matches `EventType`.
                unsafe {
                    match u32::from(record.EventType) {
                        KEY_EVENT => {
                            let k = record.Event.KeyEvent;
                            if trace::enabled() {
                                trace::note(
                                    Hop::Record,
                                    &format!(
                                        "key down={} vk={:#04x} scan={:#04x} ch={:#06x} ctrl={:#x} rep={}",
                                        k.bKeyDown,
                                        k.wVirtualKeyCode,
                                        k.wVirtualScanCode,
                                        k.uChar.UnicodeChar,
                                        k.dwControlKeyState,
                                        k.wRepeatCount
                                    ),
                                );
                            }
                            self.encoder.key(
                                KeyRecord {
                                    down: k.bKeyDown != 0,
                                    repeat: k.wRepeatCount,
                                    vk: k.wVirtualKeyCode,
                                    scan: k.wVirtualScanCode,
                                    ch: k.uChar.UnicodeChar,
                                    ctrl: k.dwControlKeyState,
                                },
                                out,
                            );
                        }
                        MOUSE_EVENT => {
                            let m = record.Event.MouseEvent;
                            if trace::enabled() {
                                trace::note(
                                    Hop::Record,
                                    &format!(
                                        "mouse x={} y={} buttons={:#x} ctrl={:#x} flags={:#x}",
                                        m.dwMousePosition.X,
                                        m.dwMousePosition.Y,
                                        m.dwButtonState,
                                        m.dwControlKeyState,
                                        m.dwEventFlags
                                    ),
                                );
                            }
                            let (left, top) = *window_origin.get_or_insert_with(|| {
                                screen_info().map_or((0, 0), |i| (i.srWindow.Left, i.srWindow.Top))
                            });
                            self.encoder.mouse(
                                MouseRecord {
                                    x: m.dwMousePosition.X.saturating_sub(left),
                                    y: m.dwMousePosition.Y.saturating_sub(top),
                                    buttons: m.dwButtonState,
                                    ctrl: m.dwControlKeyState,
                                    flags: m.dwEventFlags,
                                },
                                tracking,
                                out,
                            );
                        }
                        // Focus and buffer-size records: focus reporting is
                        // kept off the outer terminal, and resizes are polled.
                        other => {
                            trace::note(Hop::Record, &format!("event type={other:#x}"));
                        }
                    }
                }
            }
            Ok(())
        }
    }

    pub struct RawConsoleGuard {
        stdin_handle: isize,
        stdout_handle: isize,
        prev_in: CONSOLE_MODE,
        prev_out: CONSOLE_MODE,
        restore: bool,
    }

    impl RawConsoleGuard {
        /// Best-effort: disable line input, echo and Ctrl+C processing on
        /// stdin, with VT input on only when the child reads VT (see
        /// `console_input`), and enable VT processing on stdout so the child's
        /// escape sequences render. If the handles aren't consoles (piped),
        /// this is a no-op.
        pub fn enable(input: ConsoleInput) -> Self {
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
                    let mut raw_in = prev_in
                        & !(ENABLE_LINE_INPUT
                            | ENABLE_ECHO_INPUT
                            | ENABLE_PROCESSED_INPUT
                            | ENABLE_VIRTUAL_TERMINAL_INPUT);
                    match input {
                        ConsoleInput::Vt => raw_in |= ENABLE_VIRTUAL_TERMINAL_INPUT,
                        // Mouse input as crossterm sets it for a child run
                        // directly. With QuickEdit left on, an outer ConPTY
                        // older than ours (WezTerm's) never reports the mouse.
                        ConsoleInput::Records => {
                            raw_in = (raw_in & !ENABLE_QUICK_EDIT_MODE)
                                | ENABLE_MOUSE_INPUT
                                | ENABLE_EXTENDED_FLAGS;
                        }
                    }
                    SetConsoleMode(stdin_handle as _, raw_in);
                    trace::note(
                        Hop::Mode,
                        &format!("stdin {prev_in:#06x} -> {raw_in:#06x} ({input:?})"),
                    );
                }
                if ok_out {
                    SetConsoleMode(
                        stdout_handle as _,
                        prev_out | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                    );
                    trace::note(
                        Hop::Mode,
                        &format!(
                            "stdout {prev_out:#06x} -> {:#06x}",
                            prev_out | ENABLE_VIRTUAL_TERMINAL_PROCESSING
                        ),
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
                // QuickEdit only changes alongside ENABLE_EXTENDED_FLAGS, which
                // the mode we read back may lack; without it the QuickEdit we
                // turned off for Records input would stay off after we exit.
                let prev_in = if self.prev_in & ENABLE_QUICK_EDIT_MODE != 0 {
                    self.prev_in | ENABLE_EXTENDED_FLAGS
                } else {
                    self.prev_in
                };
                SetConsoleMode(self.stdin_handle as _, prev_in);
                SetConsoleMode(self.stdout_handle as _, self.prev_out);
            }
        }
    }
}
