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
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const POLL_INTERVAL: Duration = Duration::from_millis(100);

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

        Self {
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
        command.env_clear();
        command.env("PATH", &self.path_env);
        if let Ok(lang) = std::env::var("LANG") {
            command.env("LANG", lang);
        } else {
            command.env("LANG", "C.UTF-8");
        }
        command.env("LC_ALL", "C.UTF-8");
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");
        command.env("CI", "1");

        command.env("HOME", &self.home);
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
            command.env("USERPROFILE", &self.home);
            command.env("APPDATA", self.home.join("AppData/Roaming"));
            command.env("LOCALAPPDATA", self.home.join("AppData/Local"));
            command.env("TEMP", self.root.path().join("tmp"));
            command.env("TMP", self.root.path().join("tmp"));
        }
        command.env("HCOM_DIR", &self.hcom_dir);
        command.env("TMPDIR", self.root.path().join("tmp"));
        command.env("XDG_CONFIG_HOME", self.root.path().join("xdg/config"));
        command.env("XDG_CACHE_HOME", self.root.path().join("xdg/cache"));
        command.env("XDG_DATA_HOME", self.root.path().join("xdg/data"));
        command.env("XDG_STATE_HOME", self.root.path().join("xdg/state"));

        // Codex reads CODEX_HOME for config/state/sessions and hcom installs its
        // native hooks there. The mock-provider `env_key` (DUMMY_KEY) only needs
        // to be non-empty: it is sent as `Authorization: Bearer` to the
        // localhost mock, never to OpenAI. env_clear guarantees no real key leaks.
        command.env("CODEX_HOME", &self.codex_home);
        command.env("DUMMY_KEY", "hcom-real-test-dummy-key");

        // Fixture-owned provider/config vars (e.g. Claude's ANTHROPIC_BASE_URL).
        // Set on the parent too so the hcom CLI itself resolves them while it
        // installs hooks; the launched child gets them from `$HCOM_DIR/env`.
        for (key, value) in self.launch_env.borrow().iter() {
            command.env(key, value);
        }
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

    /// Build a non-hcom command (for example `codex --version`) with the same
    /// credential-stripped, isolated environment.
    pub fn external_cmd<S: AsRef<OsStr>>(&self, program: S) -> Command {
        #[cfg(windows)]
        let mut command = {
            let program = program.as_ref();
            let resolved = std::env::split_paths(&self.path_env)
                .flat_map(|dir| {
                    [".COM", ".EXE", ".BAT", ".CMD", ""]
                        .map(move |ext| dir.join(format!("{}{ext}", program.to_string_lossy())))
                })
                .find(|candidate| candidate.is_file());
            match resolved {
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
        let out = self
            .cmd()
            .args(&args)
            .output()
            .unwrap_or_else(|error| panic!("spawn hcom binary for {:?}: {error}", args));
        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        (code, stdout, stderr)
    }

    /// Run a command as a manually-started identity.
    pub fn run_as_process<I, S>(&self, process_id: &str, args: I) -> (i32, String, String)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self
            .cmd()
            .env("HCOM_PROCESS_ID", process_id)
            .args(args)
            .output()
            .expect("spawn hcom binary with HCOM_PROCESS_ID");
        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        (code, stdout, stderr)
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
        let (code, stdout, stderr) = self.run(["list", "--json"]);
        if code != 0 {
            return Err(format!("hcom list --json failed ({code}): {stderr}"));
        }
        serde_json::from_str::<Vec<Value>>(&stdout)
            .map_err(|e| format!("invalid list JSON: {e}\n{stdout}"))
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
        let mut out = String::new();
        out.push_str("\n===== hcom integration-test diagnostics =====\n");
        for (label, args) in [
            ("list --json", vec!["list", "--json"]),
            ("status --json", vec!["status", "--json"]),
            ("events --last 100", vec!["events", "--last", "100"]),
        ] {
            let (code, stdout, stderr) = self.run(args);
            out.push_str(&format!(
                "\n--- {label} (exit {code}) ---\n{stdout}{stderr}"
            ));
        }

        let hcom_log = self.hcom_dir.join(".tmp/logs/hcom.log");
        out.push_str(&format!(
            "\n--- {} (tail) ---\n{}",
            hcom_log.display(),
            read_tail(&hcom_log, 120)
        ));

        // PTY screen per instance shows the exact upstream error text for
        // failed model turns.
        if let Ok(instances) = self.list_json() {
            for instance in &instances {
                if let Some(name) = instance.get("name").and_then(Value::as_str) {
                    let (code, stdout, stderr) = self.run(["term", name]);
                    out.push_str(&format!(
                        "\n--- term {name} (exit {code}) ---\n{stdout}{stderr}"
                    ));
                    let (code, stdout, stderr) =
                        self.run(["transcript", name, "--full", "--detailed"]);
                    out.push_str(&format!(
                        "\n--- transcript {name} --full --detailed (exit {code}) ---\n{stdout}{stderr}"
                    ));
                }
            }
        }

        if let Ok(instances) = self.list_json() {
            for instance in instances {
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
