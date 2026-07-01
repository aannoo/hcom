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
use super::shared;

use crate::db::HcomDb;
use crate::delivery::{EXIT_WAS_KILLED, ScreenState};
use crate::log::log_error;

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
    /// reader thread starts delivery (ready-or-timeout gated) and stores the
    /// handle, while `run()`/`Drop` take it to join.
    delivery_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
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
            cmd.cwd(cwd);
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
            approval_clear_requested: Arc::new(AtomicBool::new(false)),
            pending_resize: Arc::new(RwLock::new(None)),
            last_tail: Arc::new(RwLock::new(None)),
            launch_failed: Arc::new(AtomicBool::new(false)),
            _job: job,
        })
    }

    /// Run the proxy until the child exits, returning its exit code.
    pub fn run(&mut self) -> Result<i32> {
        // Put our console into raw + VT passthrough so the tool's TUI renders
        // and keystrokes flow through unbuffered. Restored on drop.
        let _console = console::RawConsoleGuard::enable();

        let startup_time = Instant::now();

        let inject_server = InjectServer::new()?;
        let inject_port = inject_server.port();

        // The reader thread now owns delivery-thread startup (ready-or-timeout
        // gated), so it needs the inject port. Keep its handle: run() joins it
        // below so the EOF-captured launch-failure tail is available.
        let reader_handle = self.spawn_reader_thread(inject_port);
        self.spawn_stdin_thread();
        self.spawn_inject_thread(inject_server);
        self.spawn_resize_watcher();

        // Block until the child exits.
        let status = self.child.wait().context("waiting for ConPTY child")?;
        let exit_code = status.exit_code() as i32;

        // Commit the exit reason before setting running=false. The delivery
        // thread reads EXIT_WAS_KILLED in cleanup; if we set running=false first
        // (or allow the reader thread to do so), the delivery loop can enter
        // cleanup before this store — recording exit:closed for a kill.
        // Exit code 130 is the sentinel written by terminate_win().
        EXIT_WAS_KILLED.store(exit_code == 130, Ordering::Release);

        // Join the reader BEFORE reading last_tail (and before running=false, so
        // the ordering below still matches the Unix proxy). The reader breaks
        // its loop on PTY EOF (Ok(0)), not on `running`, so the child having
        // exited is enough for it to wind down — no stop signal is needed first.
        // It writes last_tail only at that EOF, and on Windows the ConPTY pipe
        // can signal EOF after `child.wait()` already returned; without the join
        // we could read last_tail while it is still None and emit a launch
        // failure with an empty PTY tail. Joining first closes that race.
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
        if let Some(reader_handle) = reader_handle {
            join_with_timeout(reader_handle, Duration::from_secs(2));
        }

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

        // If delivery initialization failed, surface it as a nonzero exit. main
        // maps the returned Err to a nonzero process exit; Drop still runs after
        // to reap the child.
        //
        // Unlike the Unix proxy — which propagates the error immediately via
        // `start_delivery_thread(...)?` and tears the child down at once — the
        // reader thread here cannot return early or kill `child` (it does not
        // own it), so it only sets `launch_failed`. The failure is therefore not
        // surfaced until the child exits on its own and `run()` reaches this
        // check: the returned Err matches Unix, but the report can lag by up to
        // the child's full lifetime. This is left as-is because delivery-init
        // failure (DB open / notify-port registration) is rare and, when it
        // happens, the delivery thread never started, so the delayed report is
        // the only observable cost.
        if self.launch_failed.load(Ordering::Acquire) {
            anyhow::bail!("delivery initialization failed");
        }

        Ok(exit_code)
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
    /// `shared::update_delivery_state`), starting the delivery thread once the
    /// tool is ready (or the start-timeout elapses), consuming approval-clear
    /// requests from the stdin/inject threads, applying pending resizes, and
    /// emitting title OSC updates on status/name changes.
    ///
    /// Returns the thread's `JoinHandle` so `run()` can join it after the child
    /// exits and before reading `last_tail` — the reader writes `last_tail` only
    /// at PTY EOF (`Ok(0)`), which on Windows can lag the child's exit, so the
    /// join is what guarantees the launch-failure tail is populated. Returns
    /// `None` if the reader could not be cloned (the proxy then runs without
    /// screen tracking, exactly as before).
    fn spawn_reader_thread(&self, inject_port: u16) -> Option<JoinHandle<()>> {
        let reader = match self.master.lock() {
            Ok(master) => match master.try_clone_reader() {
                Ok(r) => r,
                Err(e) => {
                    log_error("native", "win.reader", &format!("try_clone_reader: {e}"));
                    return None;
                }
            },
            Err(e) => {
                log_error("native", "win.reader", &format!("master lock: {e}"));
                return None;
            }
        };
        let running = self.running.clone();
        let screen_state = self.screen_state.clone();
        let launch_phase = self.launch_phase_active.clone();
        let target = self.config.target.clone();
        let ready_pattern = self.config.ready_pattern.clone();
        let instance = self.config.instance_name.clone();
        let current_name = self.current_name.clone();
        let current_status = self.current_status.clone();
        let approval_clear_requested = self.approval_clear_requested.clone();
        let pending_resize = self.pending_resize.clone();
        let last_tail = self.last_tail.clone();
        let launch_failed = self.launch_failed.clone();
        let delivery_handle = self.delivery_handle.clone();
        let notify_port = self.notify_port.clone();
        let delivery_start_timeout = self.config.target.delivery_start_timeout();
        let (rows, cols) = (self.rows, self.cols);

        Some(thread::spawn(move || {
            let mut reader = reader;
            let mut screen =
                ScreenTracker::new_with_instance(rows, cols, &ready_pattern, instance.as_deref());
            let mut stdout = std::io::stdout();
            let mut filter = OutputModeFilter::default();
            let mut buf = [0u8; 8192];
            let mut scratch: Vec<u8> = Vec::with_capacity(8192);

            let mut delivery_started = false;
            let mut ready_signaled = false;
            let startup = Instant::now();
            let mut last_name = String::new();
            let mut last_status = String::new();

            // Attempt to start the delivery thread. Stores the handle through a
            // poisoned mutex too, so it is never dropped here where run() can't
            // join it. Returns `true` when delivery is settled (must not retry),
            // `false` to retry on the next chunk; see the per-arm reasons below.
            let start_delivery = || match shared::start_delivery_thread(
                instance.as_deref(),
                running.clone(),
                screen_state.clone(),
                launch_phase.clone(),
                inject_port,
                target.clone(),
                notify_port.clone(),
                current_name.clone(),
                current_status.clone(),
            ) {
                Ok(Some(h)) => {
                    *delivery_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
                    // Clear any launch_failed from an earlier failed attempt so a
                    // transient error doesn't poison the exit code once a retry wins.
                    launch_failed.store(false, Ordering::Release);
                    true
                }
                Ok(None) => true,
                Err(e) => {
                    launch_failed.store(true, Ordering::Release);
                    // A timed-out start left a live detached thread; settle so we
                    // do not spawn a duplicate. Any other init error is safe to
                    // retry.
                    e.downcast_ref::<shared::DeliveryStartTimeout>().is_some()
                }
            };

            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        // EOF: capture the visible tail so run() can build the
                        // launch-failure diagnostic before the screen is gone.
                        if let Ok(mut g) = last_tail.write() {
                            *g = screen.visible_tail(8, 1000);
                        }
                        break; // child exited / PTY closed
                    }
                    Ok(n) => {
                        // A genuine keystroke / injected answer flagged a pending
                        // approval for clearing; the reader owns the tracker.
                        if approval_clear_requested.swap(false, Ordering::AcqRel) {
                            screen.clear_approval();
                        }
                        // Apply a pending terminal resize before processing this
                        // frame so the screen model matches the new geometry.
                        if let Some((r, c)) = pending_resize.write().ok().and_then(|mut g| g.take())
                        {
                            screen.resize(r, c);
                        }

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

                        let publish = |a: bool| {
                            shared::publish_approval_status(a, instance.as_deref(), &current_status)
                        };
                        shared::update_delivery_state(
                            &screen_state,
                            &screen,
                            &target,
                            &launch_phase,
                            &publish,
                        );

                        if !ready_signaled && screen.is_ready() {
                            ready_signaled = true;
                        }
                        if !delivery_started
                            && (ready_signaled || startup.elapsed() > delivery_start_timeout)
                        {
                            // Only latch as started when the attempt settled; a
                            // transient init failure leaves this clear so the
                            // next chunk retries instead of disabling delivery
                            // for the rest of the session.
                            delivery_started = start_delivery();
                        }

                        // Title OSC update. Only safe to write between complete
                        // sequences — the OutputModeFilter exposes Ground state
                        // for exactly that. The delivery thread updates the name
                        // and status Arcs; mirror them into the window title.
                        //
                        // Compare under the read guards against the last-written
                        // values and only build/clone when something actually
                        // changed. This runs on every at-ground chunk (frequent
                        // under heavy output) and name/status rarely change, so
                        // the common path holds the two read locks briefly but
                        // allocates nothing.
                        if filter.at_ground()
                            && let (Ok(name), Ok(status)) =
                                (current_name.read(), current_status.read())
                            && !name.is_empty()
                            && (*name != last_name || *status != last_status)
                        {
                            let esc = shared::build_title_escape(&name, &status, target.name());
                            let _ = stdout.write_all(esc.as_bytes());
                            let _ = stdout.flush();
                            last_name = name.clone();
                            last_status = status.clone();
                        }
                    }
                    Err(_) => break,
                }
            }
            // Fallback start: a child that exits with no output hits Ok(0) (or
            // Err) on the first read and breaks before the in-loop start above
            // ever runs. The old design called spawn_delivery_thread()
            // unconditionally from run(); moving startup into this thread lost
            // that guarantee. Start it here so an in-flight `hcom deliver`
            // payload still has a consumer and notify/inject ports get
            // registered, exactly as before — `run()` joins this thread before
            // taking the handle, so the store is always visible to it.
            if !delivery_started {
                start_delivery();
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
        let target = self.config.target.clone();
        let screen_state = self.screen_state.clone();
        let current_status = self.current_status.clone();
        let instance = self.config.instance_name.clone();
        let approval_clear_requested = self.approval_clear_requested.clone();
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
                        if n > 0 {
                            // A genuine keystroke answering a title-detected
                            // approval clears it immediately. Record the cleared
                            // edge against shared state; the reader thread owns
                            // the tracker, so request a tracker-clear via the
                            // atomic it consumes — but ONLY when an approval was
                            // actually standing. `clear_approval()` wipes the OSC
                            // scrape buffer, so requesting it on every keystroke
                            // would let a routine keypress race out an approval
                            // edge arriving in the same window.
                            let publish = |a: bool| {
                                shared::publish_approval_status(
                                    a,
                                    instance.as_deref(),
                                    &current_status,
                                )
                            };
                            if shared::note_user_keystroke(&target, &screen_state, &publish) {
                                approval_clear_requested.store(true, Ordering::Release);
                            }
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
        let target = self.config.target.clone();
        let screen_state = self.screen_state.clone();
        let current_status = self.current_status.clone();
        let instance = self.config.instance_name.clone();
        let approval_clear_requested = self.approval_clear_requested.clone();
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

    /// True when the filter is not mid-sequence (no incomplete ESC/CSI held).
    /// The reader thread gates title-OSC writes on this so a title escape is
    /// never interleaved into an incomplete sequence still being assembled.
    fn at_ground(&self) -> bool {
        self.state == FilterState::Ground
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
