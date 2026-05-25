//! Hermetic CLI fixture for integration tests.
//!
//! `Hcom::new()` returns a fixture pointing at a fresh temp `HCOM_DIR`. Use
//! `.cmd().arg(...).output()` for one-shot runs that assert exit code / stdout.
//!
//! Each integration-test file that uses this declares `mod support;` so this
//! `tests/support/mod.rs` is picked up via the subdirectory-module rule (which
//! also keeps it out of being compiled as a standalone test binary).

#![allow(dead_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

pub struct Hcom {
    pub root: TempDir,
    pub home: PathBuf,
    pub hcom_dir: PathBuf,
    bin: PathBuf,
}

impl Hcom {
    /// Build a fixture with a temp HOME and `HOME/.hcom` as `HCOM_DIR` — the
    /// shape the binary actually sees in real usage.
    pub fn new() -> Self {
        let root = tempfile::tempdir().expect("create temp dir");
        let home = root.path().join("home");
        let hcom_dir = home.join(".hcom");
        std::fs::create_dir_all(&hcom_dir).expect("create temp HOME/.hcom");
        let bin = PathBuf::from(env!("CARGO_BIN_EXE_hcom"));
        Self {
            root,
            home,
            hcom_dir,
            bin,
        }
    }

    pub fn path(&self) -> &Path {
        &self.hcom_dir
    }

    /// Build a Command wired into the hermetic temp tree. We `env_clear` and
    /// restore only the minimum needed (PATH for any subprocesses, LANG for
    /// locale-sensitive output). HOME points at the temp tree and HCOM_DIR
    /// at `HOME/.hcom`, while XDG_* are pinned under the temp root so any
    /// dir-resolver in `dirs` returns paths inside the fixture. The point is
    /// to prove no hcom command depends on `~/.hcom` or the developer's real
    /// AI-tool config leaking in via CLAUDECODE/CODEX_HOME/etc.
    pub fn cmd(&self) -> Command {
        let mut c = Command::new(&self.bin);
        c.env_clear();
        if let Ok(path) = std::env::var("PATH") {
            c.env("PATH", path);
        }
        if let Ok(lang) = std::env::var("LANG") {
            c.env("LANG", lang);
        }
        let root = self.root.path();
        c.env("HOME", &self.home);
        c.env("HCOM_DIR", &self.hcom_dir);
        c.env("TMPDIR", root.join("tmp"));
        c.env("XDG_CONFIG_HOME", root.join("xdg/config"));
        c.env("XDG_CACHE_HOME", root.join("xdg/cache"));
        c.env("XDG_DATA_HOME", root.join("xdg/data"));
        c.env("XDG_STATE_HOME", root.join("xdg/state"));
        c
    }

    /// Run with args, return (exit_code, stdout, stderr). Panics on spawn fail.
    pub fn run<I, S>(&self, args: I) -> (i32, String, String)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self.cmd().args(args).output().expect("spawn hcom binary");
        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        (code, stdout, stderr)
    }

    /// Run plain `hcom start` — the normal steady-state lifecycle entry — and
    /// return the auto-assigned identity name parsed from the `[hcom:NAME]`
    /// marker the binary prints on its first line. Use this in any setup that
    /// just needs "an existing identity"; reserve `start --as` for tests that
    /// are explicitly about reclaim/rebind.
    pub fn start(&self) -> String {
        let (code, stdout, stderr) = self.run(["start"]);
        assert_eq!(
            code, 0,
            "hcom start failed:\n-- stdout --\n{stdout}\n-- stderr --\n{stderr}"
        );
        parse_hcom_marker(&stdout)
            .unwrap_or_else(|| panic!("no [hcom:NAME] marker in stdout:\n{stdout}"))
    }
}

pub fn parse_hcom_marker(stdout: &str) -> Option<String> {
    let marker = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("[hcom:"))?;
    let after = marker.trim_start().strip_prefix("[hcom:")?;
    let name = after.split(']').next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}
