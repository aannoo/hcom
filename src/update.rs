//! Auto-update checker — checks latest release via git ls-remote once daily.
//! Uses git ls-remote instead of the GitHub REST API to avoid rate limits.

use crate::paths::{FLAGS_DIR, atomic_write, hcom_path};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const CHECK_INTERVAL: Duration = Duration::from_secs(86400); // 24 hours
const UNIX_INSTALL_CMD: &str =
    "curl -fsSL https://github.com/aannoo/hcom/releases/latest/download/hcom-installer.sh | sh";
const WINDOWS_INSTALL_CMD: &str = "powershell -NoProfile -ExecutionPolicy Bypass -Command \"irm https://github.com/aannoo/hcom/releases/latest/download/hcom-installer.ps1 | iex\"";

pub(crate) fn flag_path() -> PathBuf {
    hcom_path(&[FLAGS_DIR, "update_check"])
}

/// Parse version string "x.y.z" into comparable tuple.
fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let parts: Vec<&str> = v.trim().trim_start_matches('v').split('.').collect();
    if parts.len() >= 3 {
        Some((
            parts[0].parse().ok()?,
            parts[1].parse().ok()?,
            parts[2].parse().ok()?,
        ))
    } else {
        None
    }
}

/// Spawn a detached background process to fetch latest version and write the cache file.
/// Returns immediately — result shows up on next command.
///
/// No-op on Windows: the script below is POSIX (`sh -c`, `awk`, `git`/`curl`
/// piping), and there's no `sh` to run it. Porting this to PowerShell is
/// disproportionate for a fire-and-forget cache refresh (errors are already
/// silently swallowed), so Windows just skips the doomed spawn attempt.
fn spawn_background_check(flag: &Path, current: &str) {
    if cfg!(windows) {
        return;
    }
    let flag_str = flag.to_string_lossy().to_string();
    let current = current.to_string();

    // Shell script: uses git ls-remote (no rate limits) to get latest tag, compares, writes cache.
    // Runs completely detached — parent doesn't wait.
    let script = format!(
        r#"
TAG=$(GIT_HTTP_LOW_SPEED_LIMIT=1000 GIT_HTTP_LOW_SPEED_TIME=5 git ls-remote --tags --sort=version:refname https://github.com/aannoo/hcom.git 2>/dev/null | grep -v '\^{{}}' | tail -1 | sed 's|.*refs/tags/||')
# Fallback to GitHub API if git unavailable
if [ -z "$TAG" ]; then
    TAG=$(curl -fsSL --max-time 5 https://api.github.com/repos/aannoo/hcom/releases/latest 2>/dev/null | grep '"tag_name"' | head -1 | cut -d'"' -f4)
fi
VER="${{TAG#v}}"
if [ -n "$VER" ]; then
    # Compare: if remote > current, write version; else write empty
    REMOTE=$(echo "$VER" | awk -F. '{{printf "%d%06d%06d", $1, $2, $3}}')
    LOCAL=$(echo "{current}" | awk -F. '{{printf "%d%06d%06d", $1, $2, $3}}')
    if [ "$REMOTE" -gt "$LOCAL" ] 2>/dev/null; then
        printf '%s' "$VER" > "{flag_str}"
    else
        printf '' > "{flag_str}"
    fi
else
    printf '' > "{flag_str}"
fi
"#
    );

    // Fire and forget — detach from parent process
    let _ = std::process::Command::new("sh")
        .args(["-c", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Synchronously fetch the latest version. Tries git ls-remote first (no rate limits),
/// falls back to GitHub API if git is unavailable.
fn fetch_latest_version() -> Option<String> {
    fetch_via_git().or_else(fetch_via_curl)
}

fn fetch_via_git() -> Option<String> {
    let output = std::process::Command::new("git")
        .args([
            "ls-remote",
            "--tags",
            "--sort=version:refname",
            "https://github.com/aannoo/hcom.git",
        ])
        .env("GIT_HTTP_LOW_SPEED_LIMIT", "1000")
        .env("GIT_HTTP_LOW_SPEED_TIME", "5")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let body = String::from_utf8_lossy(&output.stdout);
    let tag = body
        .lines()
        .rfind(|l| !l.ends_with("^{}"))?
        .split("refs/tags/")
        .nth(1)?
        .trim()
        .to_string();

    let ver = tag.trim_start_matches('v').to_string();
    if ver.is_empty() { None } else { Some(ver) }
}

fn fetch_via_curl() -> Option<String> {
    let output = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "5",
            "https://api.github.com/repos/aannoo/hcom/releases/latest",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let body = String::from_utf8_lossy(&output.stdout);
    let tag = body
        .lines()
        .find(|l| l.contains("\"tag_name\""))?
        .split('"')
        .nth(3)?
        .to_string();

    let ver = tag.trim_start_matches('v').to_string();
    if ver.is_empty() { None } else { Some(ver) }
}

/// Structured update information: current version, latest available, availability, and update command.
#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub available: bool,
    pub cmd: &'static str,
}

/// Synchronously fetch current + latest version info from GitHub.
/// Single source of truth for all update-related logic (fetching, parsing, command selection).
/// Used by `hcom update` command for fresh checks.
pub fn fetch_update_info() -> anyhow::Result<UpdateInfo> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let latest =
        fetch_latest_version().ok_or_else(|| anyhow::anyhow!("Could not reach GitHub API"))?;

    let current_parsed = parse_version(&current);
    let latest_parsed = parse_version(&latest);
    let available = current_parsed < latest_parsed;
    let cmd = get_update_cmd();

    Ok(UpdateInfo {
        current,
        latest,
        available,
        cmd,
    })
}

/// Whether `cmd` needs POSIX shell semantics to run (currently: only the
/// curl-installer fallback, which is a pipe to `sh`). All other commands
/// `get_update_cmd()` returns (`pip install -U hcom`, `uv tool upgrade hcom`,
/// `brew upgrade hcom`) are a plain program + args and need no shell at all.
///
/// Platform-independent so it's testable on any host; `cmd_update` uses this
/// on Windows (which has no `sh`) to decide whether to refuse instead of
/// attempting a doomed spawn.
pub(crate) fn is_shell_pipe_command(cmd: &str) -> bool {
    cmd.starts_with("curl ")
}

pub(crate) fn is_powershell_installer_command(cmd: &str) -> bool {
    cmd == WINDOWS_INSTALL_CMD
}

/// Prefer `pwsh` over `powershell`: Windows PowerShell 5.1's module load can
/// fail on a polluted `PSModulePath` (nested shells, OneDrive redirects);
/// `pwsh` isn't affected. Falls back to `powershell` if pwsh isn't installed.
pub(crate) fn windows_installer_program() -> &'static str {
    let pwsh_available = std::process::Command::new("pwsh")
        .args(["-NoProfile", "-Command", "exit 0"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if pwsh_available { "pwsh" } else { "powershell" }
}

/// Split a plain `program arg1 arg2 ...` command string into program + args.
/// Only meant for the shell-free update commands `get_update_cmd()` returns
/// (no quoting to worry about); not a general shell parser.
pub(crate) fn split_program_args(cmd: &str) -> Option<(&str, Vec<&str>)> {
    let mut parts = cmd.split_whitespace();
    let program = parts.next()?;
    Some((program, parts.collect()))
}

/// Detect install method and return appropriate update command.
fn get_update_cmd() -> &'static str {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return platform_installer_cmd(),
    };
    get_update_cmd_for_exe(&exe)
}

fn get_update_cmd_for_exe(exe: &Path) -> &'static str {
    // Resolve symlinks (e.g. Homebrew Cellar, uv shims).
    let resolved = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    // Normalizing separators also makes install detection testable and handles
    // native Windows paths without duplicating every path pattern.
    let path_str = resolved.to_string_lossy().replace('\\', "/");
    let path_lower = path_str.to_ascii_lowercase();

    // Homebrew install (Cellar path on both Apple Silicon and Intel)
    if path_str.contains("/Cellar/") {
        return "brew upgrade hcom";
    }

    // uv tool install
    if path_lower.contains("/uv/") || path_lower.contains("/.local/share/uv/") {
        return "uv tool upgrade hcom";
    }

    // pip install inside a venv. Maturin's `bindings = "bin"` wheels put the
    // executable in the environment's scripts directory, not site-packages,
    // so arbitrary environment names are also covered by the metadata check
    // below.
    if path_lower.contains("/site-packages/")
        || path_lower.contains("/dist-packages/")
        || path_lower.contains("/venv/")
        || path_lower.contains("/.venv/")
    {
        return "pip install -U hcom";
    }

    // A prefix-wide pip install puts the binary in <prefix>/bin and metadata
    // below <prefix>/lib. This is the normal layout on Termux, where prefix is
    // /data/data/com.termux/files/usr, and is also common for system Python.
    if is_prefix_pip_install(&resolved) {
        return "pip install -U hcom";
    }

    platform_installer_cmd()
}

fn platform_installer_cmd() -> &'static str {
    if cfg!(windows) {
        WINDOWS_INSTALL_CMD
    } else {
        UNIX_INSTALL_CMD
    }
}

fn record_owns_exe(site_dir: &Path, dist_info: &Path, exe: &Path) -> bool {
    let Ok(record) = fs::read_to_string(dist_info.join("RECORD")) else {
        return false;
    };

    record.lines().any(|line| {
        let Some(record_path) = line.split(',').next() else {
            return false;
        };
        let candidate = site_dir.join(record_path);
        matches!(
            (std::fs::canonicalize(candidate), std::fs::canonicalize(exe)),
            (Ok(candidate), Ok(exe)) if candidate == exe
        )
    })
}

fn site_dir_has_hcom_exe(site_dir: &Path, exe: &Path) -> bool {
    let Ok(entries) = fs::read_dir(site_dir) else {
        return false;
    };

    entries.flatten().any(|pkg| {
        let is_hcom_dist_info = pkg
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("hcom-") && name.ends_with(".dist-info"));
        is_hcom_dist_info && pkg.path().is_dir() && record_owns_exe(site_dir, &pkg.path(), exe)
    })
}

fn python_lib_has_hcom_exe(lib_dir: &Path, exe: &Path) -> bool {
    let Ok(entries) = fs::read_dir(lib_dir) else {
        return false;
    };

    entries.flatten().any(|entry| {
        let python_dir = entry.path();
        python_dir.is_dir()
            && ["site-packages", "dist-packages"]
                .iter()
                .any(|name| site_dir_has_hcom_exe(&python_dir.join(name), exe))
    })
}

fn is_prefix_pip_install(exe: &Path) -> bool {
    let Some(scripts_dir) = exe.parent() else {
        return false;
    };
    let Some(prefix) = scripts_dir.parent() else {
        return false;
    };

    let scripts_dir_name = scripts_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if scripts_dir_name != "bin" && !scripts_dir_name.eq_ignore_ascii_case("scripts") {
        return false;
    }

    [prefix.join("lib"), prefix.join("lib64")]
        .iter()
        .any(|lib_dir| python_lib_has_hcom_exe(lib_dir, exe))
        || site_dir_has_hcom_exe(&prefix.join("Lib/site-packages"), exe)
}

/// Check for updates (once daily cached). Returns (latest_version, update_cmd) or None.
///
/// Never blocks: if the cache is stale, spawns a background process to refresh it
/// and returns the current (possibly stale) cached result.
pub fn get_update_info() -> Option<(String, &'static str)> {
    let flag = flag_path();
    let current = env!("CARGO_PKG_VERSION");

    // Check if cache is stale and needs refresh
    let should_check = if flag.exists() {
        match flag.metadata().and_then(|m| m.modified()) {
            Ok(mtime) => {
                SystemTime::now()
                    .duration_since(mtime)
                    .unwrap_or(Duration::ZERO)
                    > CHECK_INTERVAL
            }
            Err(_) => true,
        }
    } else {
        true
    };

    if should_check {
        // Non-blocking: spawn background check, result appears on next command
        spawn_background_check(&flag, current);
    }

    // Read cached result (may be from a previous check)
    let latest = fs::read_to_string(&flag).ok()?.trim().to_string();
    if latest.is_empty() {
        return None;
    }

    // Double-check (handles manual upgrades)
    if parse_version(current) >= parse_version(&latest) {
        atomic_write(&flag, "");
        return None;
    }

    Some((latest, get_update_cmd()))
}

/// Return update notice string for stderr, or None if up to date.
pub fn get_update_notice() -> Option<String> {
    let (latest, _cmd) = get_update_info()?;
    Some(format!("→ hcom v{latest} available — run `hcom update`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version() {
        assert_eq!(parse_version("0.7.0"), Some((0, 7, 0)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("bad"), None);
        assert_eq!(parse_version("1.2"), None);
    }

    #[test]
    fn test_is_shell_pipe_command() {
        assert!(is_shell_pipe_command(
            "curl -fsSL https://example.com/install.sh | sh"
        ));
        assert!(!is_shell_pipe_command("pip install -U hcom"));
        assert!(!is_shell_pipe_command("uv tool upgrade hcom"));
        assert!(!is_shell_pipe_command("brew upgrade hcom"));
        assert!(!is_shell_pipe_command(WINDOWS_INSTALL_CMD));
        assert!(is_powershell_installer_command(WINDOWS_INSTALL_CMD));
        assert!(!is_powershell_installer_command("pip install -U hcom"));
    }

    #[test]
    fn test_split_program_args() {
        assert_eq!(
            split_program_args("pip install -U hcom"),
            Some(("pip", vec!["install", "-U", "hcom"]))
        );
        assert_eq!(
            split_program_args("uv tool upgrade hcom"),
            Some(("uv", vec!["tool", "upgrade", "hcom"]))
        );
        assert_eq!(split_program_args(""), None);
        assert_eq!(split_program_args("   "), None);
    }

    #[test]
    fn test_version_comparison() {
        assert!(parse_version("0.8.0") > parse_version("0.7.0"));
        assert!(parse_version("1.0.0") > parse_version("0.99.99"));
        assert!(parse_version("0.7.0") == parse_version("0.7.0"));
    }

    #[test]
    fn test_get_update_cmd_default() {
        // Test binary path won't match any known install method.
        let cmd = get_update_cmd();
        if cfg!(windows) {
            assert!(
                cmd.contains("hcom-installer.ps1"),
                "expected PowerShell fallback, got: {cmd}"
            );
        } else {
            assert!(cmd.contains("curl"), "expected curl fallback, got: {cmd}");
        }
    }

    #[test]
    fn test_windows_style_install_paths_are_detected() {
        assert_eq!(
            get_update_cmd_for_exe(Path::new(
                r"C:\Users\me\AppData\Local\uv\tools\hcom\Scripts\hcom.exe"
            )),
            "uv tool upgrade hcom"
        );
        assert_eq!(
            get_update_cmd_for_exe(Path::new(r"C:\Users\me\project\.venv\Scripts\hcom.exe")),
            "pip install -U hcom"
        );
    }

    #[test]
    fn test_prefix_pip_detection_matches_termux_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = tmp.path().join("data/data/com.termux/files/usr");
        let exe = prefix.join("bin/hcom");
        let dist_info = prefix.join("lib/python3.14/site-packages/hcom-0.7.23.dist-info");

        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&dist_info).unwrap();
        std::fs::write(&exe, b"binary").unwrap();
        std::fs::write(
            dist_info.join("RECORD"),
            "../../../bin/hcom,sha256=test,6\n",
        )
        .unwrap();

        assert_eq!(get_update_cmd_for_exe(&exe), "pip install -U hcom");
    }

    #[test]
    fn test_prefix_pip_detection_ignores_stale_dist_info() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = tmp.path().join("usr");
        let exe = prefix.join("bin/hcom");
        let other_exe = prefix.join("bin/other-hcom");
        let dist_info = prefix.join("lib/python3.14/site-packages/hcom-0.7.23.dist-info");

        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&dist_info).unwrap();
        std::fs::write(&exe, b"binary").unwrap();
        std::fs::write(&other_exe, b"other binary").unwrap();
        std::fs::write(
            dist_info.join("RECORD"),
            "../../../bin/other-hcom,sha256=test,12\n",
        )
        .unwrap();

        assert_eq!(get_update_cmd_for_exe(&exe), platform_installer_cmd());
    }
}
