//! Hermetic CLI fixture for integration tests.
//!
//! `Hcom::new()` returns a fixture pointing at a fresh temp tree. Every hcom,
//! Codex, XDG, and temporary path is redirected below that tree. Long-lived
//! launches are cleaned up by process group when the fixture is dropped.
//!
//! Each integration-test file that uses this declares `mod support;` so this
//! `tests/support/mod.rs` is picked up via the subdirectory-module rule (which
//! also keeps it out of being compiled as a standalone test binary).

#![allow(dead_code)]

pub mod claude_mock;
pub mod codex_mock;
pub mod mock_http;
pub mod real_tool;

use rusqlite::OptionalExtension;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Hard ceiling on a single hcom CLI invocation. Real commands finish in well
/// under a second; this only trips when one is genuinely wedged, converting an
/// unbounded hang into a fast, labelled failure instead of a CI job timeout.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);
/// Ceiling on the diagnostics process snapshot. Shorter than [`RUN_TIMEOUT`]:
/// it is one OS query, and it runs from the panic hook where an unbounded wait
/// would turn a failing test into a hung job.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Hcom {
    pub root: TempDir,
    pub home: PathBuf,
    pub hcom_dir: PathBuf,
    pub codex_home: PathBuf,
    pub claude_home: PathBuf,
    pub workspace: PathBuf,
    bin: PathBuf,
    path_env: OsString,
    /// Provider/config vars the launched tool must see. Applied to every hcom
    /// command AND persisted to `$HCOM_DIR/env`, because `CI=1` makes hcom treat
    /// the parent as contaminated and rebuild the child's env from a clean shell
    /// (`launcher::build_launch_env`) — a var set only on the parent `Command`
    /// would be dropped. The `$HCOM_DIR/env` passthrough is overlaid last and
    /// wins, so Claude's `ANTHROPIC_BASE_URL` actually reaches the child.
    launch_env: RefCell<BTreeMap<String, String>>,
    cleanup_pids: RefCell<HashSet<i64>>,
    cleanup_children: RefCell<Vec<Child>>,
}

/// Everything [`diagnostics_for`] needs to shell out to the fixture's exact
/// hcom binary with its isolated env — split out of `Hcom` (which also holds
/// non-`Send`-friendly-to-share bits like open `Child` handles) so the panic
/// hook installed by [`install_diagnostics_panic_hook`] can hold one without
/// borrowing a live `&Hcom`. `launch_env` (provider vars for the *launched*
/// tool, not hcom's own read commands) is deliberately omitted — diagnostics
/// only ever runs read-only hcom subcommands (list/status/events/term/
/// transcript), which don't consult it.
#[derive(Clone)]
struct DiagContext {
    bin: PathBuf,
    root_path: PathBuf,
    home: PathBuf,
    hcom_dir: PathBuf,
    codex_home: PathBuf,
    path_env: OsString,
}

thread_local! {
    // Thread-local, not a process-wide `Mutex`: `cli_smoke.rs` runs ~20
    // non-`#[ignore]` tests that each call `Hcom::new()`, and the Justfile's
    // `step test` (unlike the three real-tool/relay steps) runs plain `cargo
    // test --locked` with the default multi-threaded runner — a shared slot
    // there would dump whichever fixture happened to be active on some other
    // thread, or nothing at all. The panic hook always runs on the panicking
    // thread before unwinding starts, so thread-local storage is guaranteed
    // correct regardless of how many fixtures are live across other threads,
    // and needs no `Drop`-time ownership check to avoid clobbering.
    static ACTIVE_DIAG: RefCell<Option<DiagContext>> = const { RefCell::new(None) };
}

static PANIC_HOOK_INIT: Once = Once::new();

/// Installs (once per process) a panic hook that prints the active fixture's
/// diagnostics — the same `hcom list/status/events`, hcom.log tail, and
/// per-instance term/transcript dump `Hcom::diagnostics()` produces — to
/// stderr before the default hook runs. Most `assert_eq!` call sites in the
/// shared real-tool runner already thread `h.diagnostics()` through by hand,
/// but that's easy to forget at any new call site, and a bare `assert!` /
/// `panic!` / `.expect()` anywhere (this repo has dozens) previously surfaced
/// with zero context — exactly the gap that made a real Windows relay-worker
/// flake undiagnosable (it panicked with just "relay worker not running",
/// no way to tell a real crash from a timing race). Fires for every
/// `Hcom`-based real-tool test process-wide, not just one call site.
///
/// Skips the dump entirely if the panic payload already contains the
/// diagnostics header — several call sites (`real_tool.rs`, `claude_mock.rs`,
/// `real_tool_claude.rs`, `real_tool_codex.rs`, `Hcom::diagnostics` callers)
/// already thread `h.diagnostics()` through their own panic message, and
/// regenerating the same dump a second time would double the output and the
/// subprocess cost exactly when the box is already under load.
///
/// The hook body must never itself panic: std aborts the process
/// (`rtabort!`, uncatchable by `catch_unwind`) on a panic raised from inside
/// a panic hook, which would skip `Drop` entirely and leak every fixture
/// process this test spawned. [`diagnostics_for`] and everything it calls
/// (`run_ctx`, `list_json_ctx`) fold spawn/wait errors into the dump text
/// instead of panicking for exactly this reason.
fn install_diagnostics_panic_hook() {
    PANIC_HOOK_INIT.call_once(|| {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            default_hook(info);
            let already_dumped = info
                .payload()
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| info.payload().downcast_ref::<&str>().copied())
                .is_some_and(|msg| msg.contains("hcom integration-test diagnostics"));
            if already_dumped {
                return;
            }
            let ctx = ACTIVE_DIAG.with(|slot| slot.borrow().clone());
            if let Some(ctx) = ctx {
                eprintln!("\n===== hcom integration-test diagnostics (panic hook) =====");
                eprintln!("{}", diagnostics_for(&ctx));
                eprintln!("===== end diagnostics =====\n");
            }
        }));
    });
}

/// Build the isolated, credential-stripped env every hcom invocation under a
/// fixture runs with. Shared by `Hcom::apply_isolated_env` (real launch_env)
/// and the panic-hook/diagnostics path (empty launch_env — read-only hcom
/// subcommands don't consult it).
fn apply_isolated_env_ctx(
    ctx: &DiagContext,
    launch_env: &BTreeMap<String, String>,
    command: &mut Command,
) {
    command.env_clear();
    command.env("PATH", &ctx.path_env);
    if let Ok(lang) = std::env::var("LANG") {
        command.env("LANG", lang);
    } else {
        command.env("LANG", "C.UTF-8");
    }
    command.env("LC_ALL", "C.UTF-8");
    command.env("TERM", "xterm-256color");
    command.env("NO_COLOR", "1");
    command.env("CI", "1");

    command.env("HOME", &ctx.home);
    command.env("HCOM_DEV_ROOT", env!("CARGO_MANIFEST_DIR"));
    #[cfg(windows)]
    {
        // Windows PowerShell and cmd are OS components, not user state.
        // Clearing SystemRoot/WINDIR can make powershell.exe fail before it
        // reads the generated runner; PATHEXT/COMSPEC are required for npm
        // .cmd shims. Keep user-writable profile/temp locations isolated.
        for key in ["SystemRoot", "WINDIR", "COMSPEC", "PATHEXT"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command.env("USERPROFILE", &ctx.home);
        command.env("APPDATA", ctx.home.join("AppData/Roaming"));
        command.env("LOCALAPPDATA", ctx.home.join("AppData/Local"));
        command.env("TEMP", ctx.root_path.join("tmp"));
        command.env("TMP", ctx.root_path.join("tmp"));
    }
    command.env("HCOM_DIR", &ctx.hcom_dir);
    command.env("TMPDIR", ctx.root_path.join("tmp"));
    command.env("XDG_CONFIG_HOME", ctx.root_path.join("xdg/config"));
    command.env("XDG_CACHE_HOME", ctx.root_path.join("xdg/cache"));
    command.env("XDG_DATA_HOME", ctx.root_path.join("xdg/data"));
    command.env("XDG_STATE_HOME", ctx.root_path.join("xdg/state"));

    // Codex reads CODEX_HOME for config/state/sessions and hcom installs its
    // native hooks there. The mock-provider `env_key` (DUMMY_KEY) only needs
    // to be non-empty: it is sent as `Authorization: Bearer` to the
    // localhost mock, never to OpenAI. env_clear guarantees no real key leaks.
    command.env("CODEX_HOME", &ctx.codex_home);
    command.env("DUMMY_KEY", "hcom-real-test-dummy-key");

    // Fixture-owned provider/config vars (e.g. Claude's ANTHROPIC_BASE_URL).
    // Set on the parent too so the hcom CLI itself resolves them while it
    // installs hooks; the launched child gets them from `$HCOM_DIR/env`.
    for (key, value) in launch_env.iter() {
        command.env(key, value);
    }
}

/// Run an hcom invocation directly from a [`DiagContext`], for the
/// panic-hook/diagnostics path where there's no live `&Hcom` to call
/// `Hcom::run` on.
fn run_ctx<I, S>(ctx: &DiagContext, args: I) -> (i32, String, String)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect();
    let mut command = Command::new(&ctx.bin);
    apply_isolated_env_ctx(ctx, &BTreeMap::new(), &mut command);
    command.args(&args);
    run_command_bounded_safe(command, &args)
}

fn list_json_ctx(ctx: &DiagContext) -> Result<Vec<Value>, String> {
    let (code, stdout, stderr) = run_ctx(ctx, ["list", "--json"]);
    if code != 0 {
        return Err(format!("hcom list --json failed ({code}): {stderr}"));
    }
    serde_json::from_str::<Vec<Value>>(&stdout)
        .map_err(|e| format!("invalid list JSON: {e}\n{stdout}"))
}

/// Same dump `Hcom::diagnostics()` produces, built from a [`DiagContext`] so
/// the panic hook can call it without a live `&Hcom`.
fn diagnostics_for(ctx: &DiagContext) -> String {
    let mut out = String::new();
    out.push_str("\n===== hcom integration-test diagnostics =====\n");
    for (label, args) in [
        ("list --json", vec!["list", "--json"]),
        ("status --json", vec!["status", "--json"]),
        ("events --last 100", vec!["events", "--last", "100"]),
    ] {
        let (code, stdout, stderr) = run_ctx(ctx, args);
        out.push_str(&format!(
            "\n--- {label} (exit {code}) ---\n{stdout}{stderr}"
        ));
    }

    // `list -v` adds what the JSON omits: the headless log path and the
    // human-readable status detail. Do NOT read its `bindings:` line as
    // evidence the PTY came up — "pty" there means `process_bound`, which the
    // *launcher* writes before it spawns anything, so it is true for every
    // launch. The `term <name>` exit code below and the process snapshot are
    // the signals that actually distinguish a live PTY proxy from none.
    let (code, stdout, stderr) = run_ctx(ctx, ["list", "-v"]);
    out.push_str(&format!(
        "\n--- list -v (exit {code}) ---\n{stdout}{stderr}"
    ));

    let hcom_log = ctx.hcom_dir.join(".tmp/logs/hcom.log");
    out.push_str(&format!(
        "\n--- {} (tail) ---\n{}",
        hcom_log.display(),
        read_tail(&hcom_log, 120)
    ));

    // The generated launch scripts are the exact commands the launch chain was
    // going to run. A launch that stalls before the tool prints anything leaves
    // no other record of what it tried; background mode never deletes them, so
    // they are still on disk at failure time.
    out.push_str(&format!(
        "\n--- launch scripts ---\n{}",
        launch_scripts_dump(&ctx.hcom_dir.join(".tmp/launch"))
    ));

    // Which processes in the launch chain are actually alive. Without this the
    // dump cannot distinguish "the tool hung" from "the wrapper shell never got
    // as far as starting the tool" — the tracked pid is the background wrapper,
    // not the tool.
    out.push_str(&format!(
        "\n--- launch-chain processes ---\n{}",
        process_snapshot()
    ));

    // PTY screen per instance shows the exact upstream error text for
    // failed model turns. Single `list --json` call reused for both the
    // term/transcript dump and the background-log tail below — spawning
    // hcom is the costliest part of this function.
    if let Ok(instances) = list_json_ctx(ctx) {
        for instance in &instances {
            if let Some(name) = instance.get("name").and_then(Value::as_str) {
                let (code, stdout, stderr) = run_ctx(ctx, ["term", name]);
                out.push_str(&format!(
                    "\n--- term {name} (exit {code}) ---\n{stdout}{stderr}"
                ));
                let (code, stdout, stderr) =
                    run_ctx(ctx, ["transcript", name, "--full", "--detailed"]);
                out.push_str(&format!(
                    "\n--- transcript {name} --full --detailed (exit {code}) ---\n{stdout}{stderr}"
                ));
            }
            if let Some(path) = instance
                .get("background_log_file")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let path = PathBuf::from(path);
                out.push_str(&format!(
                    "\n--- {} (tail) ---\n{}",
                    path.display(),
                    read_tail(&path, 120)
                ));
            }
        }
    }

    out
}

impl Hcom {
    /// Build a fixture whose every writable path is below one temporary root.
    pub fn new() -> Self {
        let root = tempfile::tempdir().expect("create temp dir");
        let home = root.path().join("home");
        let hcom_dir = root.path().join("hcom-state");
        let codex_home = root.path().join("codex-home");
        let claude_home = root.path().join("claude-home");
        let workspace = root.path().join("workspace");
        let bin = PathBuf::from(env!("CARGO_BIN_EXE_hcom"));

        for dir in [
            &home,
            &hcom_dir,
            &codex_home,
            &claude_home,
            &workspace,
            &root.path().join("tmp"),
            &root.path().join("xdg/config"),
            &root.path().join("xdg/cache"),
            &root.path().join("xdg/data"),
            &root.path().join("xdg/state"),
        ] {
            fs::create_dir_all(dir)
                .unwrap_or_else(|e| panic!("create isolated directory {}: {e}", dir.display()));
        }

        let mut path_entries = Vec::new();
        if let Some(parent) = bin.parent() {
            // The scripted Codex shell call uses `hcom ...`; make the exact
            // CARGO_BIN_EXE_hcom binary discoverable before any ambient PATH.
            path_entries.push(parent.to_path_buf());
        }
        if let Some(inherited) = std::env::var_os("PATH") {
            path_entries.extend(std::env::split_paths(&inherited));
        }
        let path_env = std::env::join_paths(path_entries).expect("construct isolated PATH");

        let fixture = Self {
            root,
            home,
            hcom_dir,
            codex_home,
            claude_home,
            workspace,
            bin,
            path_env,
            launch_env: RefCell::new(BTreeMap::new()),
            cleanup_pids: RefCell::new(HashSet::new()),
            cleanup_children: RefCell::new(Vec::new()),
        };
        install_diagnostics_panic_hook();
        let ctx = fixture.diag_context();
        ACTIVE_DIAG.with(|slot| *slot.borrow_mut() = Some(ctx));
        fixture
    }

    fn diag_context(&self) -> DiagContext {
        DiagContext {
            bin: self.bin.clone(),
            root_path: self.root.path().to_path_buf(),
            home: self.home.clone(),
            hcom_dir: self.hcom_dir.clone(),
            codex_home: self.codex_home.clone(),
            path_env: self.path_env.clone(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.hcom_dir
    }

    pub fn root_path(&self) -> &Path {
        self.root.path()
    }

    /// Shell expression that invokes this test's exact hcom binary.
    pub fn shell_hcom_command(&self) -> String {
        let path = self.bin.to_string_lossy();
        if cfg!(windows) {
            format!("& '{}'", path.replace('\'', "''"))
        } else {
            format!("'{}'", path.replace('\'', "'\\''"))
        }
    }

    /// Exact binary invocation for tools whose Windows shell is Git Bash
    /// (not PowerShell), notably Claude's Bash tool.
    pub fn bash_hcom_command(&self) -> String {
        let path = self.bin.to_string_lossy().replace('\\', "/");
        format!("'{}'", path.replace('\'', "'\\''"))
    }

    fn apply_isolated_env(&self, command: &mut Command) {
        apply_isolated_env_ctx(&self.diag_context(), &self.launch_env.borrow(), command);
    }

    /// Set a provider/config var the launched tool must see, surviving hcom's
    /// `CI=1` clean-shell launch rebuild. Written to both the parent env and the
    /// `$HCOM_DIR/env` passthrough (which wins). `HCOM_*` keys are rejected: the
    /// config loader owns those and treats them separately.
    pub fn set_launch_env(&self, key: &str, value: &str) {
        assert!(
            !key.starts_with("HCOM_"),
            "set_launch_env is for provider/config vars, not hcom-owned {key}"
        );
        self.launch_env
            .borrow_mut()
            .insert(key.to_string(), value.to_string());
        self.write_hcom_env_file();
    }

    /// Bulk form of [`set_launch_env`].
    pub fn set_launch_envs(&self, values: &[(&str, &str)]) {
        {
            let mut env = self.launch_env.borrow_mut();
            for (key, value) in values {
                assert!(
                    !key.starts_with("HCOM_"),
                    "set_launch_env is for provider/config vars, not hcom-owned {key}"
                );
                env.insert((*key).to_string(), (*value).to_string());
            }
        }
        self.write_hcom_env_file();
    }

    fn write_hcom_env_file(&self) {
        let body: String = self
            .launch_env
            .borrow()
            .iter()
            .map(|(key, value)| format!("{key}={value}\n"))
            .collect();
        fs::write(self.hcom_dir.join("env"), body).expect("write isolated hcom env passthrough");
    }

    /// Build a Command wired into the isolated temp tree.
    pub fn cmd(&self) -> Command {
        let mut command = Command::new(&self.bin);
        self.apply_isolated_env(&mut command);
        command
    }

    /// Resolve an external tool the same way hcom's own `which_bin` does, so a
    /// version check and the launch it gates can never disagree about which file
    /// they mean.
    ///
    /// Windows resolves extension-major *within* each PATH directory, so a stray
    /// `claude.exe` sitting next to npm's `claude.cmd` shim wins — which is
    /// exactly how a mock-tools prefix left over from an earlier pin silently
    /// takes over. Returning the path lets callers name the offending file.
    pub fn resolve_external<S: AsRef<OsStr>>(&self, program: S) -> Option<PathBuf> {
        let program = program.as_ref();
        #[cfg(windows)]
        {
            std::env::split_paths(&self.path_env)
                .flat_map(|dir| {
                    [".COM", ".EXE", ".BAT", ".CMD", ""]
                        .map(move |ext| dir.join(format!("{}{ext}", program.to_string_lossy())))
                })
                .find(|candidate| candidate.is_file())
        }
        #[cfg(not(windows))]
        {
            std::env::split_paths(&self.path_env)
                .map(|dir| dir.join(program))
                .find(|candidate| candidate.is_file())
        }
    }

    /// Build a non-hcom command (for example `codex --version`) with the same
    /// credential-stripped, isolated environment.
    pub fn external_cmd<S: AsRef<OsStr>>(&self, program: S) -> Command {
        #[cfg(windows)]
        let mut command = {
            let program = program.as_ref();
            match self.resolve_external(program) {
                Some(path)
                    if matches!(
                        path.extension().and_then(OsStr::to_str),
                        Some(ext) if ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat")
                    ) =>
                {
                    let mut command = Command::new("cmd.exe");
                    command.args(["/d", "/c"]).arg(path);
                    command
                }
                Some(path) => Command::new(path),
                None => Command::new(program),
            }
        };
        #[cfg(not(windows))]
        let mut command = Command::new(program);
        self.apply_isolated_env(&mut command);
        command
    }

    /// Run with args, returning `(exit_code, stdout, stderr)`.
    pub fn run<I, S>(&self, args: I) -> (i32, String, String)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_os_string())
            .collect();
        if std::env::var_os("HCOM_TEST_TRACE_COMMANDS").is_some() {
            eprintln!("hcom test command: {:?}", args);
        }
        let mut command = self.cmd();
        command.args(&args);
        run_command_bounded(command, &args)
    }

    /// Run a command as a manually-started identity.
    pub fn run_as_process<I, S>(&self, process_id: &str, args: I) -> (i32, String, String)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_os_string())
            .collect();
        let mut command = self.cmd();
        command.env("HCOM_PROCESS_ID", process_id).args(&args);
        run_command_bounded(command, &args)
    }

    /// Start a manual identity and return its canonical name.
    pub fn start_with_process_id(&self, process_id: &str) -> String {
        let (code, stdout, stderr) = self.run_as_process(process_id, ["start"]);
        assert_eq!(
            code, 0,
            "hcom start failed:\n-- stdout --\n{stdout}\n-- stderr --\n{stderr}"
        );
        parse_hcom_marker(&stdout)
            .unwrap_or_else(|| panic!("no [hcom:NAME] marker in stdout:\n{stdout}"))
    }

    /// Start a manual identity and keep it genuinely live while a real tool
    /// performs its comparatively slow startup. A bare `hcom start` identity
    /// has no heartbeat source and is correctly considered stale after 30s.
    pub fn start_listening_with_process_id(&self, process_id: &str) -> String {
        let name = self.start_with_process_id(process_id);
        let output_path = self.recipient_output_path(process_id);
        let output = fs::File::create(&output_path).expect("create live recipient output");
        let mut command = self.cmd();
        command
            .env("HCOM_PROCESS_ID", process_id)
            .args(["listen", "--json", "--timeout", "600"])
            .stdin(Stdio::null())
            .stdout(output)
            .stderr(Stdio::null());
        let child = command.spawn().expect("spawn live test recipient");
        self.track_cleanup_pid(i64::from(child.id()));
        self.cleanup_children.borrow_mut().push(child);
        self.eventually(
            "manual test recipient to enter listening state",
            Duration::from_secs(10),
            || {
                let instance = self.instance_json(&name)?;
                Ok(instance
                    .filter(|value| {
                        value.get("status").and_then(Value::as_str) == Some("listening")
                    })
                    .map(|_| ()))
            },
        );
        name
    }

    pub fn recipient_output(&self, process_id: &str) -> String {
        fs::read_to_string(self.recipient_output_path(process_id)).unwrap_or_default()
    }

    fn recipient_output_path(&self, process_id: &str) -> PathBuf {
        self.hcom_dir.join(format!("recipient-{process_id}.jsonl"))
    }

    /// Run plain `hcom start` and return the auto-assigned identity name.
    pub fn start(&self) -> String {
        let (code, stdout, stderr) = self.run(["start"]);
        assert_eq!(
            code, 0,
            "hcom start failed:\n-- stdout --\n{stdout}\n-- stderr --\n{stderr}"
        );
        parse_hcom_marker(&stdout)
            .unwrap_or_else(|| panic!("no [hcom:NAME] marker in stdout:\n{stdout}"))
    }

    /// Write the isolated Codex `config.toml` pointing the default model
    /// provider at the localhost mock. hcom still installs every native Codex
    /// hook and auto-trusts the workspace through the real launch path.
    ///
    /// `requires_openai_auth = false` plus the dummy `env_key` (DUMMY_KEY, set in
    /// the isolated env) lets Codex start fully headless against the mock. The
    /// model is a stable real id so Codex advertises its normal tool set; the
    /// mock supplies every turn so the id is never used for routing.
    ///
    /// Deliberately omits `approval_policy`: approvals are hcom's job, driven by
    /// the `--sandbox <mode>` launch flag (`get_sandbox_flags` →
    /// `--sandbox workspace-write` / `-a untrusted` / bypass). Hand-writing the
    /// policy here would bypass that translation and let a regression in it pass
    /// unnoticed — so tests set the policy through the real hcom launch path.
    pub fn prepare_codex_config(&self, mock_base_url: &str) {
        fs::create_dir_all(&self.codex_home).expect("create isolated Codex home");
        let config = format!(
            "model = \"gpt-5.5\"\n\
             model_provider = \"mock_local\"\n\
             \n\
             [model_providers.mock_local]\n\
             name = \"Local Mock\"\n\
             base_url = \"{mock_base_url}\"\n\
             env_key = \"DUMMY_KEY\"\n\
             wire_api = \"responses\"\n\
             requires_openai_auth = false\n"
        );
        fs::write(self.codex_home.join("config.toml"), config)
            .expect("write isolated Codex config.toml");
    }

    /// Return installed Codex version text, or a clear absence/error reason.
    pub fn codex_version(&self) -> Result<String, String> {
        self.external_version("codex")
    }

    /// Return `<binary> --version` text, or a clear absence/error reason, run in
    /// the same credential-stripped isolated environment as launches.
    pub fn external_version(&self, binary: &str) -> Result<String, String> {
        let output = self
            .external_cmd(binary)
            .arg("--version")
            .output()
            .map_err(|e| format!("could not execute `{binary} --version`: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "`{binary} --version` exited {:?}: stdout={} stderr={}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let version = if stdout.is_empty() { stderr } else { stdout };
        if version.is_empty() {
            Err(format!("`{binary} --version` produced no version text"))
        } else {
            Ok(version)
        }
    }

    /// Active instances for one tool (`codex`, `claude`, ...).
    pub fn instances_for_tool(&self, tool: &str) -> Result<Vec<Value>, String> {
        Ok(self
            .list_json()?
            .into_iter()
            .filter(|v| v.get("tool").and_then(Value::as_str) == Some(tool))
            .collect())
    }

    pub fn codex_instances(&self) -> Result<Vec<Value>, String> {
        self.instances_for_tool("codex")
    }

    pub fn list_json(&self) -> Result<Vec<Value>, String> {
        list_json_ctx(&self.diag_context())
    }

    pub fn instance_json(&self, name: &str) -> Result<Option<Value>, String> {
        Ok(self.list_json()?.into_iter().find(|v| {
            v.get("name").and_then(Value::as_str) == Some(name)
                || v.get("base_name").and_then(Value::as_str) == Some(name)
        }))
    }

    pub fn instance_pid(&self, name: &str) -> Result<Option<i64>, String> {
        let db_path = self.hcom_dir.join("hcom.db");
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = rusqlite::Connection::open(&db_path)
            .map_err(|e| format!("open {}: {e}", db_path.display()))?;
        let pid = conn
            .query_row("SELECT pid FROM instances WHERE name = ?1", [name], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .optional()
            .map(|row| row.flatten())
            .map_err(|e| format!("query pid for {name}: {e}"))?;
        if let Some(pid) = pid.filter(|pid| *pid > 1) {
            self.cleanup_pids.borrow_mut().insert(pid);
        }
        Ok(pid)
    }

    pub fn track_cleanup_pid(&self, pid: i64) {
        if pid > 1 {
            self.cleanup_pids.borrow_mut().insert(pid);
        }
    }

    /// Insert a synthetic file-edit status event for another instance, directly
    /// into the DB. This exercises the public `--collision` query surface (does
    /// it join two instances' edits on the same path?), not real concurrent-edit
    /// detection — spawning a second real tool purely to touch one file would
    /// roughly double the test's runtime for no extra coverage of the query
    /// itself. `context` is the tool's file-edit context (`tool:Write`,
    /// `tool:apply_patch`, ...).
    pub fn log_file_edit_for_test(
        &self,
        instance: &str,
        context: &str,
        path: &str,
    ) -> Result<(), String> {
        let db_path = self.hcom_dir.join("hcom.db");
        let conn = rusqlite::Connection::open(&db_path)
            .map_err(|e| format!("open {}: {e}", db_path.display()))?;
        let data = serde_json::json!({
            "status": "active",
            "context": context,
            "detail": path,
        });
        conn.execute(
            "INSERT INTO events (timestamp, type, instance, data)
             VALUES (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 'status', ?1, ?2)",
            rusqlite::params![instance, data.to_string()],
        )
        .map_err(|e| format!("insert test file-edit event: {e}"))?;
        Ok(())
    }

    pub fn all_tracked_pids(&self) -> Vec<i64> {
        let db_path = self.hcom_dir.join("hcom.db");
        if !db_path.exists() {
            return Vec::new();
        }
        let Ok(conn) = rusqlite::Connection::open(&db_path) else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare("SELECT DISTINCT pid FROM instances WHERE pid IS NOT NULL")
        else {
            return Vec::new();
        };
        let rows = match stmt.query_map([], |row| row.get::<_, i64>(0)) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(Result::ok).filter(|pid| *pid > 1).collect()
    }

    /// Poll a public/semantic condition. On timeout, panic with hcom state,
    /// event output, and log tails instead of leaving an opaque assertion.
    pub fn eventually<T, F>(&self, description: &str, timeout: Duration, mut poll: F) -> T
    where
        F: FnMut() -> Result<Option<T>, String>,
    {
        let deadline = Instant::now() + timeout;
        let mut last_error = None;
        loop {
            match poll() {
                Ok(Some(value)) => return value,
                Ok(None) => {}
                Err(error) => last_error = Some(error),
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {description}\nlast poll error: {}\n{}",
                    last_error.as_deref().unwrap_or("<none>"),
                    self.diagnostics()
                );
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    pub fn diagnostics(&self) -> String {
        diagnostics_for(&self.diag_context())
    }

    pub fn process_group_alive(&self, pid: i64) -> bool {
        process_group_alive(pid)
    }

    /// Terminate one hcom-owned process group, escalating only after bounded
    /// polling. Returns true once the group no longer exists.
    pub fn terminate_process_group(&self, pid: i64) -> bool {
        terminate_process_group(pid)
    }
}

impl Drop for Hcom {
    fn drop(&mut self) {
        // Thread-local: always this thread's own fixture, so no ownership
        // check is needed before clearing (unlike a process-wide slot, a
        // fixture on another thread can't have overwritten this one).
        ACTIVE_DIAG.with(|slot| *slot.borrow_mut() = None);
        if std::thread::panicking() {
            self.root.disable_cleanup(true);
            eprintln!(
                "preserving failed real-tool test directory: {}",
                self.root.path().display()
            );
        }
        // Capture pids before `hcom kill all` removes instance rows.
        let mut pids: HashSet<i64> = self.all_tracked_pids().into_iter().collect();
        pids.extend(self.cleanup_pids.borrow().iter().copied());
        // `kill all` is the clean teardown path, but a wedged binary must not
        // hang suite teardown: bound it, then fall through to the pid sweep
        // (which SIGKILLs by process group) regardless of how it ended.
        if let Ok(mut child) = self.cmd().args(["kill", "all"]).spawn() {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() >= deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    Ok(None) => std::thread::sleep(POLL_INTERVAL),
                    Err(_) => break,
                }
            }
        }
        for pid in pids {
            let _ = self.terminate_process_group(pid);
        }
        for mut child in self.cleanup_children.borrow_mut().drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(unix)]
pub fn process_group_alive(pid: i64) -> bool {
    if pid <= 1 || pid > i32::MAX as i64 {
        return false;
    }
    // A negative pid addresses the process group whose id is `pid`.
    let rc = unsafe { nix::libc::kill(-(pid as i32), 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(nix::libc::EPERM)
}

// Windows has no process-group primitive; this fixture only ever tracks a
// single spawned pid on this codepath, so liveness degrades to a plain PID
// check.
#[cfg(windows)]
pub fn process_group_alive(pid: i64) -> bool {
    if pid <= 1 || pid > u32::MAX as i64 {
        return false;
    }
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: query-only access mask; the handle is closed before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut exit_code) != 0;
        CloseHandle(handle);
        ok && exit_code == STILL_ACTIVE as u32
    }
}

/// Terminate one hcom-owned process group, escalating only after bounded
/// polling. Returns true once the group no longer exists.
#[cfg(unix)]
pub fn terminate_process_group(pid: i64) -> bool {
    if !process_group_alive(pid) {
        return true;
    }
    unsafe {
        nix::libc::kill(-(pid as i32), nix::libc::SIGTERM);
    }
    if poll_until(Duration::from_secs(3), || !process_group_alive(pid)) {
        return true;
    }
    unsafe {
        nix::libc::kill(-(pid as i32), nix::libc::SIGKILL);
    }
    poll_until(Duration::from_secs(3), || !process_group_alive(pid))
}

// Windows has no process-group primitive; terminate just the tracked pid.
#[cfg(windows)]
pub fn terminate_process_group(pid: i64) -> bool {
    if !process_group_alive(pid) {
        return true;
    }
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    // SAFETY: opens a terminate-only handle, closes it before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid as u32);
        if !handle.is_null() {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
    poll_until(Duration::from_secs(3), || !process_group_alive(pid))
}

pub fn parse_hcom_marker(stdout: &str) -> Option<String> {
    let marker = stdout
        .lines()
        .find(|line| line.trim_start().starts_with("[hcom:"))?;
    let after = marker.trim_start().strip_prefix("[hcom:")?;
    let name = after.split(']').next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

pub fn parse_launch_names(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("Names: "))
        .map(|names| names.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

pub fn unique_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

/// Spawn `command` and collect its output under a hard deadline.
///
/// `Command::output` waits for stdout+stderr to reach EOF, which needs every
/// process holding an inherited copy of those pipe handles to exit —
/// including a detached launch grandchild that keeps the write end open long
/// after the direct child returns. That turns one wedged invocation into an
/// unbounded hang (the `eventually` poll only checks its own deadline
/// *between* calls), and CI can only end it with a job timeout.
///
/// Instead we drain both pipes on background threads into shared buffers and
/// wait for the *direct* child alone. On exit (or [`RUN_TIMEOUT`]) we
/// snapshot whatever the readers captured rather than joining them, so a
/// grandchild holding the pipe never blocks the call. A timeout kills the
/// child, reports a non-zero code, and prints a labelled marker so the wedged
/// subcommand is named in the log.
///
/// Panics on spawn/wait failure — fine for the ordinary `Hcom::run` path
/// where that's a fixture-breaking bug worth failing loudly on. The
/// diagnostics/panic-hook path must never panic (see
/// [`install_diagnostics_panic_hook`]) and uses [`run_command_bounded_safe`]
/// instead.
fn run_command_bounded(mut command: Command, args: &[OsString]) -> (i32, String, String) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("spawn hcom binary for {args:?}: {error}"));
    let stdout_buf = drain_stream(child.stdout.take());
    let stderr_buf = drain_stream(child.stderr.take());

    let deadline = Instant::now() + RUN_TIMEOUT;
    let (code, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.code().unwrap_or(-1), false),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break (-1, true);
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(error) => panic!("wait on hcom binary for {args:?}: {error}"),
        }
    };
    // Grace for the readers to drain bytes already buffered in the pipe. We
    // snapshot instead of joining: a detached grandchild can hold the write
    // end open, so a reader may never observe EOF.
    std::thread::sleep(Duration::from_millis(200));
    let stdout = String::from_utf8_lossy(&stdout_buf.lock().unwrap()).into_owned();
    let mut stderr = String::from_utf8_lossy(&stderr_buf.lock().unwrap()).into_owned();
    if timed_out {
        let marker = format!(
            "<hcom test: `hcom {}` exceeded {}s and was killed>",
            args.iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            RUN_TIMEOUT.as_secs()
        );
        eprintln!("{marker}");
        stderr.push_str(&marker);
        stderr.push('\n');
    }
    (code, stdout, stderr)
}

/// Same contract as [`run_command_bounded`], for the diagnostics/panic-hook
/// path (`run_ctx`, `list_json_ctx`, `diagnostics_for`) where a panic would
/// abort the whole process instead of just failing one test — std aborts
/// (`rtabort!`, uncatchable) on a panic raised from inside a panic hook.
/// Spawn and wait failures are folded into the returned stderr as `<...>`
/// markers instead.
fn run_command_bounded_safe(mut command: Command, args: &[OsString]) -> (i32, String, String) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return (
                -1,
                String::new(),
                format!("<failed to spawn hcom binary for {args:?}: {error}>\n"),
            );
        }
    };
    let stdout_buf = drain_stream(child.stdout.take());
    let stderr_buf = drain_stream(child.stderr.take());

    let deadline = Instant::now() + RUN_TIMEOUT;
    let (code, timed_out, wait_error) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.code().unwrap_or(-1), false, None),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break (-1, true, None);
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(error) => break (-1, false, Some(error.to_string())),
        }
    };
    std::thread::sleep(Duration::from_millis(200));
    let stdout = String::from_utf8_lossy(
        &stdout_buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_owned();
    let mut stderr = String::from_utf8_lossy(
        &stderr_buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_owned();
    if timed_out {
        stderr.push_str(&format!(
            "<hcom test: `hcom {}` exceeded {}s and was killed>\n",
            args.iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            RUN_TIMEOUT.as_secs()
        ));
    }
    if let Some(error) = wait_error {
        stderr.push_str(&format!("<wait on hcom binary failed: {error}>\n"));
    }
    (code, stdout, stderr)
}

/// Drain a child pipe on a background thread into a shared buffer, appending as
/// bytes arrive. The caller snapshots the buffer under its own deadline instead
/// of joining, so a pipe the child never closes (a detached grandchild holds the
/// write end) can't wedge the read. A leaked reader dies with the test process.
fn drain_stream<R: Read + Send + 'static>(stream: Option<R>) -> Arc<Mutex<Vec<u8>>> {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    if let Some(mut stream) = stream {
        let sink = Arc::clone(&buffer);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().unwrap().extend_from_slice(&chunk[..n]),
                }
            }
        });
    }
    buffer
}

fn poll_until<F>(timeout: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn read_tail(path: &Path, max_lines: usize) -> String {
    let Ok(content) = fs::read_to_string(path) else {
        return "<missing or unreadable>\n".to_string();
    };
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    let mut tail = lines[start..].join("\n");
    tail.push('\n');
    tail
}

/// Every generated launch/runner script still on disk, with its body.
///
/// A background launch that never produces tool output leaves these as the only
/// record of what the wrapper shell was asked to run (background mode, unlike
/// foreground, never deletes them).
///
/// Bodies are printed only for files positively identified as an hcom-generated
/// wrapper/runner or an args sidecar. The launch dir ALSO holds the ambient-env
/// sidecar, which carries the parent's non-HCOM environment — real credentials
/// on a dev box — and the runner deletes it right after sourcing it. A launch
/// that stalls before the runner runs is exactly when it is still there, i.e.
/// exactly the case this dump exists for, so name-based exclusion is not enough
/// (on Windows it is another `.ps1` with the same naming pattern). Identify by
/// content instead and fail closed: hcom's scripts open with a comment or the
/// window-title line, the env sidecar opens with an assignment.
fn launch_scripts_dump(launch_dir: &Path) -> String {
    let Ok(entries) = fs::read_dir(launch_dir) else {
        return format!("<no launch dir at {}>\n", launch_dir.display());
    };
    let mut names: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    names.sort();
    if names.is_empty() {
        return "<launch dir is empty>\n".to_string();
    }
    let mut out = String::new();
    for path in names {
        out.push_str(&format!("\n-- {} --\n", path.display()));
        let is_args = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".args.json"));
        let body = fs::read_to_string(&path).unwrap_or_default();
        let first = body
            .trim_start_matches('\u{feff}')
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default();
        let generated = first.starts_with('#') || first.starts_with("$Host.UI.RawUI.WindowTitle");
        if is_args || generated {
            out.push_str(&read_tail(&path, 60));
        } else {
            out.push_str("<not dumped: unrecognized file, may be the ambient-env sidecar>\n");
        }
    }
    out
}

#[test]
fn launch_scripts_dump_prints_generated_scripts_but_not_the_env_sidecar() {
    let dir = tempfile::tempdir().expect("temp launch dir");
    fs::write(
        dir.path().join("claude_luna_1_2.ps1"),
        "\u{feff}# Claude hcom native runner (luna)\n& 'hcom.exe' pty claude\n",
    )
    .expect("write runner");
    fs::write(
        dir.path().join("hcom_1_3.ps1"),
        "\u{feff}$Host.UI.RawUI.WindowTitle = \"hcom: starting Claude...\"\nWrite-Host x\n",
    )
    .expect("write wrapper");
    // Same extension and naming shape as the runner — only the body tells them
    // apart, and this one holds the parent's ambient environment.
    fs::write(
        dir.path().join("claude_luna_1_4.ps1"),
        "\u{feff}$env:AWS_SECRET_ACCESS_KEY = 'super-secret'\n",
    )
    .expect("write env sidecar");

    let dump = launch_scripts_dump(dir.path());
    assert!(dump.contains("pty claude"), "runner body missing:\n{dump}");
    assert!(
        dump.contains("Write-Host x"),
        "wrapper body missing:\n{dump}"
    );
    assert!(
        !dump.contains("super-secret"),
        "ambient-env sidecar must never be dumped:\n{dump}"
    );
    assert!(
        dump.contains("may be the ambient-env sidecar"),
        "skipped file should say why:\n{dump}"
    );
}

/// Processes in the launch chain that are still alive, with command lines.
///
/// The pid hcom tracks for a background launch is the *wrapper shell*, not the
/// tool, so "process alive" in a launch-failure detail says nothing about
/// whether the tool ever started. This snapshot is what separates the two:
/// wrapper-only means the chain stalled before the tool; a live tool process
/// means the tool itself is stuck.
fn process_snapshot() -> String {
    #[cfg(windows)]
    let mut command = {
        // CIM rather than `tasklist`: the command line is what distinguishes the
        // outer wrapper, the runner shell, and `hcom pty` — all three are
        // `powershell.exe`/`hcom.exe` by image name alone.
        //
        // Filter on the image name inside the query, not on the rendered line:
        // matching `node` against whole command lines pulls in every Electron
        // helper on a dev box (`--utility-sub-type=node.mojom.NodeService`) and
        // buries the four processes this dump exists to show.
        let mut command = Command::new("powershell");
        command.args([
            "-NoProfile",
            "-Command",
            "Get-CimInstance Win32_Process \
             | Where-Object { $_.Name -match '^(hcom|claude|codex|node|powershell|pwsh|cmd|conhost|OpenConsole)\\.exe$' } \
             | Select-Object ProcessId,ParentProcessId,Name,CommandLine \
             | Format-Table -AutoSize | Out-String -Width 400",
        ]);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = Command::new("ps");
        command.args(["-eo", "pid,ppid,etime,args"]);
        command
    };

    // Bounded like every other diagnostics subprocess: this runs from the panic
    // hook, where a `wait_with_output()` that never returns would hang the test
    // binary instead of failing it, and WMI in particular can stall on a loaded
    // machine — exactly when this dump matters most.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return format!("<process snapshot unavailable: {error}>\n"),
    };
    let stdout_buf = drain_stream(child.stdout.take());
    let _stderr_buf = drain_stream(child.stderr.take());
    let deadline = Instant::now() + SNAPSHOT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return format!(
                    "<process snapshot exceeded {}s and was killed>\n",
                    SNAPSHOT_TIMEOUT.as_secs()
                );
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(error) => return format!("<process snapshot wait failed: {error}>\n"),
        }
    }
    let captured = stdout_buf
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let text = String::from_utf8_lossy(&captured);
    let mut out = String::new();
    for line in text.lines() {
        // Windows already filtered in the query; `ps` output is narrow enough
        // that a substring match over the whole line is fine here.
        #[cfg(not(windows))]
        {
            const INTERESTING: &[&str] =
                &["hcom", "claude", "codex", "node", "bash", "sh -", "script"];
            let low = line.to_lowercase();
            if !INTERESTING.iter().any(|needle| low.contains(needle)) {
                continue;
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("<no launch-chain processes alive>\n");
    }
    out
}
