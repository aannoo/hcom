//! Auto-update checker — checks latest release via git ls-remote once daily.
//! Uses git ls-remote instead of the GitHub REST API to avoid rate limits.

use std::path::Path;

use crate::paths::{FLAGS_DIR, atomic_write, hcom_path};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const CHECK_INTERVAL: Duration = Duration::from_secs(86400); // 24 hours

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
fn spawn_background_check(flag: &Path, current: &str) {
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

/// Detect install method and return appropriate update command.
fn get_update_cmd() -> &'static str {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => {
            return "curl -fsSL https://github.com/aannoo/hcom/releases/latest/download/hcom-installer.sh | sh";
        }
    };

    // Resolve symlink — build.sh copies binary and symlinks ~/.local/bin/hcom -> repo bin/
    let resolved = std::fs::canonicalize(&exe).unwrap_or(exe.clone());
    let path_str = resolved.to_string_lossy();

    // Dev build
    if path_str.contains("/hook-comms/")
        || path_str.contains("/target/")
        || path_str.contains("/cargo-target/")
        || path_str.contains("/.hcom-build/")
    {
        return "./build.sh";
    }

    // Homebrew install (Cellar path on both Apple Silicon and Intel)
    if path_str.contains("/Cellar/") {
        return "brew upgrade hcom";
    }

    // uv tool install
    if path_str.contains("/uv/") || path_str.contains("/.local/share/uv/") {
        return "uv tool upgrade hcom";
    }

    // pip install (venv or site-packages)
    if path_str.contains("/site-packages/") || path_str.contains("/venv/") {
        return "pip install -U hcom";
    }

    // Default: curl installer
    "curl -fsSL https://github.com/aannoo/hcom/releases/latest/download/hcom-installer.sh | sh"
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
    fn test_version_comparison() {
        assert!(parse_version("0.8.0") > parse_version("0.7.0"));
        assert!(parse_version("1.0.0") > parse_version("0.99.99"));
        assert!(parse_version("0.7.0") == parse_version("0.7.0"));
    }

    #[test]
    fn test_get_update_cmd_default() {
        // In test context, binary is in target/debug, which contains /target/
        assert_eq!(get_update_cmd(), "./build.sh");
    }
}
