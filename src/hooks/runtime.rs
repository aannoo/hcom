//! Per-run hook integration: shared plumbing.
//!
//! A per-run tool loads hcom's hooks/plugin for one invocation (a flag or an
//! env var the launcher adds) instead of from a global install in the tool's
//! config dir. There is no persistent mode for these tools: a plain run of the
//! tool never carries hcom, and nested same-tool children started from an
//! agent's shell don't inherit the parent's hooks.
//!
//! Each tool supplies a [`PerRunAdapter`] (schema and merge logic stay in
//! `src/hooks/<tool>.rs`); tools without one keep the persistent install path in
//! `launcher::ensure_hooks_installed`.
//!
//! Launch order ([`plan`]): prepare/validate the injection → remove hcom's
//! legacy installs from the effective config dirs (a failure is a warning naming
//! the file) → sync permission-only files → inject.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::tool::Tool;

/// Set by the OpenCode/Kilo plugin in its host process. A nested host that
/// inherited the plugin via `*_CONFIG_CONTENT` sees a foreign PID and stays
/// inert. The launcher strips it so a legitimate new host starts clean.
pub const PLUGIN_HOST_PID_ENV: &str = "HCOM_PLUGIN_HOST_PID";

/// Everything an adapter may read to build its injection.
///
/// `env` is the effective child environment (`build_launch_env` + caller env +
/// tool config-dir env), which is authoritative: adapters resolve config dirs
/// (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, …) and caller values
/// (`OPENCODE_CONFIG_CONTENT`, …) from it, not from `std::env`.
#[derive(Debug, Clone)]
pub struct LaunchCtx {
    pub tool: Tool,
    pub env: HashMap<String, String>,
    pub cwd: PathBuf,
    /// Caller args with hcom-owned replay values already stripped.
    pub args: Vec<String>,
    pub auto_approve: bool,
}

impl LaunchCtx {
    /// Context for commands that run outside a launch (`hcom hooks`,
    /// `hcom config auto_approve`): the current process env and cwd, no args.
    pub fn ambient(tool: Tool, auto_approve: bool) -> Self {
        Self {
            tool,
            env: std::env::vars().collect(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            args: Vec::new(),
            auto_approve,
        }
    }

    /// Non-empty env value, matching the name case-insensitively on Windows.
    pub fn var(&self, key: &str) -> Option<&str> {
        let value = if cfg!(windows) {
            self.env
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.as_str())
        } else {
            self.env.get(key).map(String::as_str)
        };
        value.filter(|v| !v.is_empty())
    }

    /// Non-empty path env value; a relative one is resolved against the
    /// launch cwd, which is where the tool itself resolves it.
    pub fn path_var(&self, key: &str) -> Option<PathBuf> {
        self.var(key).map(|value| self.cwd.join(value))
    }
}

/// What a per-run launch adds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeInjection {
    /// The complete argv to launch with (caller args with hcom's flags merged
    /// in). Adapters that merge a single-valued caller flag remove the caller's
    /// occurrences here; see [`take_flag_values`] and [`insert_before_separator`].
    pub args: Vec<String>,
    /// Env vars to set on the child (e.g. a merged `OPENCODE_CONFIG_CONTENT`).
    pub env: Vec<(String, String)>,
}

/// One per-run tool's hooks into the shared launch flow.
pub struct PerRunAdapter {
    /// Build and validate the injection: publish artifacts, merge caller
    /// values, fetch trust entries. Must not write outside
    /// `<HCOM_DIR>/integrations/`. An error fails the launch.
    pub prepare: fn(&LaunchCtx) -> Result<RuntimeInjection>,
    /// Remove hcom-owned legacy installs (from older hcom versions) in the dirs
    /// that are effective for this launch only. Delete by ownership (content
    /// match / hcom marker), never by filename; leave malformed files alone and
    /// return an error naming them. An error is a launch warning, not a
    /// failure. Idempotent and cheap when nothing is there.
    pub cleanup_legacy: fn(&LaunchCtx) -> Result<()>,
    /// Keep permission-only files in sync with `ctx.auto_approve` (write when
    /// on, remove hcom's entries when off). Called on every launch and when
    /// `auto_approve` changes.
    pub ensure_permissions: Option<fn(&LaunchCtx) -> Result<()>>,
    /// Flags whose values hcom may have injected (`--plugin-dir`, `-e`,
    /// `--settings`, …). Replayed args drop values that are hcom runtime
    /// artifacts ([`is_hcom_runtime_path`]) before the injection is rebuilt.
    pub managed_value_flags: &'static [&'static str],
    /// Extra replay cleanup for values older hcom versions injected outside
    /// `<HCOM_DIR>/integrations/` (e.g. OMP's `-e ~/.omp/agent/extensions/hcom.ts`).
    /// Legacy cleanup deletes those files, so replaying them would fail startup.
    pub strip_legacy_args: Option<fn(&mut Vec<String>)>,
}

/// The per-run adapter for `tool`, or `None` for tools on the persistent path.
pub fn adapter(tool: Tool) -> Option<&'static PerRunAdapter> {
    match tool {
        Tool::Claude => Some(&crate::hooks::claude::PER_RUN),
        Tool::Codex => Some(&crate::hooks::codex::PER_RUN),
        Tool::Copilot => Some(&crate::hooks::copilot::PER_RUN),
        Tool::Pi => Some(&crate::hooks::pi::PER_RUN),
        Tool::Omp => Some(&crate::hooks::omp::PER_RUN),
        Tool::OpenCode => Some(&crate::hooks::opencode::OPENCODE_PER_RUN),
        Tool::Kilo => Some(&crate::hooks::opencode::KILO_PER_RUN),
        // No hooks at all: status and delivery come over Grok's ACP.
        Tool::Grok => Some(&crate::delivery::grok::PER_RUN),
        // Cursor stays persistent: cursor-agent loads --plugin-dir hooks
        // asynchronously and beforeSubmitPrompt/stop often never fire from them.
        Tool::Cursor | Tool::Gemini | Tool::Kimi | Tool::Antigravity | Tool::Adhoc => None,
    }
}

pub fn is_per_run(tool: Tool) -> bool {
    adapter(tool).is_some()
}

/// How a tool gets hcom's hooks. Static per tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookMode {
    PerRun,
    Persistent,
    /// The integration uses no hooks (Grok: everything comes over its ACP).
    None,
}

impl HookMode {
    pub fn of(tool: Tool) -> Self {
        if tool.hooks().is_empty() {
            HookMode::None
        } else if is_per_run(tool) {
            HookMode::PerRun
        } else {
            HookMode::Persistent
        }
    }

    /// Stable value for JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            HookMode::PerRun => "per_run",
            HookMode::Persistent => "persistent",
            HookMode::None => "none",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HookMode::PerRun => "per-run",
            HookMode::Persistent => "persistent",
            HookMode::None => "no hooks",
        }
    }
}

/// Run the per-run launch steps and return the injection to apply.
pub fn plan(adapter: &PerRunAdapter, ctx: &LaunchCtx) -> Result<RuntimeInjection> {
    let tool = ctx.tool.as_str();
    let injection = (adapter.prepare)(ctx)
        .with_context(|| format!("Failed to prepare hcom's per-run {tool} integration"))?;
    // A leftover hcom hook the tool can still load would run next to the
    // per-run one, but most failures are files the tool can't load either
    // (unreadable, malformed), so warn and launch rather than block.
    if let Err(error) = (adapter.cleanup_legacy)(ctx) {
        let errors = match error.downcast::<LegacyErrors>() {
            Ok(LegacyErrors(errors)) => errors,
            Err(error) => vec![error],
        };
        for error in &errors {
            crate::log::log_warn(
                "launcher",
                "runtime.legacy_cleanup_failed",
                &format!("tool={tool} {error:#}"),
            );
            eprintln!("{}", legacy_cleanup_warning(tool, error));
        }
    }
    if let Some(ensure_permissions) = adapter.ensure_permissions {
        ensure_permissions(ctx)
            .with_context(|| format!("Failed to sync hcom's {tool} permission rules"))?;
    }
    crate::log::log_info(
        "launcher",
        "runtime.injection",
        &format!(
            "tool={tool} args={} env={}",
            injection.args.len(),
            injection
                .env
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ),
    );
    Ok(injection)
}

/// A file from an older hcom install that legacy cleanup could not fix, and
/// what the user should do to it. Attach with `.context(LegacyFile::..)` so
/// the launch warning can name both.
#[derive(Debug)]
pub struct LegacyFile {
    pub path: PathBuf,
    /// Imperative fix, e.g. "delete it".
    pub fix: String,
    /// hcom's legacy hooks may still load: a write or delete failed on a file
    /// the tool can read. False when the tool can't read or parse it either.
    pub still_loads: bool,
}

impl LegacyFile {
    /// Reading or parsing failed; the tool can't load the file either.
    pub fn read(path: &Path, fix: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            fix: fix.into(),
            still_loads: false,
        }
    }

    /// A write or delete failed, so hcom's hooks in it may still load.
    pub fn write(path: &Path, fix: impl Into<String>) -> Self {
        Self {
            still_loads: true,
            ..Self::read(path, fix)
        }
    }
}

/// Several independent cleanup failures; [`plan`] warns once per entry.
#[derive(Debug)]
pub struct LegacyErrors(pub Vec<anyhow::Error>);

impl std::fmt::Display for LegacyErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let messages: Vec<String> = self.0.iter().map(|e| format!("{e:#}")).collect();
        write!(f, "{}", messages.join("; "))
    }
}

/// `Ok` when empty, the error itself when one, else [`LegacyErrors`].
pub fn collect_errors(mut errors: Vec<anyhow::Error>) -> Result<()> {
    match errors.len() {
        0 => Ok(()),
        1 => Err(errors.remove(0)),
        _ => Err(anyhow::Error::msg(LegacyErrors(errors))),
    }
}

impl std::fmt::Display for LegacyFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path.display())
    }
}

/// Fix text for a settings or hooks file that may still hold hcom's entries.
pub const FIX_REMOVE_HCOM_HOOKS: &str = "remove the hook entries whose command runs hcom";
/// Fix text for a plugin or metadata file that is entirely hcom's.
pub const FIX_DELETE: &str = "delete it";

fn legacy_cleanup_warning(tool: &str, error: &anyhow::Error) -> String {
    let cause = error.root_cause();
    match error.downcast_ref::<LegacyFile>() {
        Some(file) => {
            let mut warning = format!(
                "Warning: could not clean up a file an older hcom left for {tool}: {}\n  \
                 Reason: {cause}\n  \
                 Fix: {}.",
                file.path.display(),
                file.fix,
            );
            if file.still_loads {
                warning.push_str(&format!(" Until then {tool} may run hcom's hooks twice."));
            }
            warning
        }
        None => format!("Warning: could not clean up an older hcom {tool} install: {error:#}"),
    }
}

/// Delete each of `paths` that `owned` says is hcom's plugin. Every path is
/// tried; failures are collected.
pub fn remove_owned_files(
    paths: impl IntoIterator<Item = PathBuf>,
    owned: impl Fn(&Path) -> std::io::Result<bool>,
) -> Result<()> {
    let errors = paths
        .into_iter()
        .filter_map(|path| remove_owned_file(&path, &owned).err())
        .collect();
    collect_errors(errors)
}

fn remove_owned_file(path: &Path, owned: impl Fn(&Path) -> std::io::Result<bool>) -> Result<()> {
    if owned(path).with_context(|| LegacyFile::read(path, FIX_DELETE))? {
        std::fs::remove_file(path).with_context(|| LegacyFile::write(path, FIX_DELETE))?;
    }
    Ok(())
}

// ── Ownership ────────────────────────────────────────────────────────────

/// Whether the file at `path` is hcom's, judged by `owned(content)`.
///
/// A missing file (or dangling symlink) is `Ok(false)`, and so is non-UTF-8
/// content (hcom never writes that). Any other read failure (permissions, a
/// directory in the way, I/O) is an error, so a remover never reports
/// success while leaving an hcom plugin it couldn't inspect.
pub fn file_is_hcom_owned(path: &Path, owned: impl Fn(&str) -> bool) -> std::io::Result<bool> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(owned(&content)),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(std::io::Error::new(
            e.kind(),
            format!("cannot read {}: {e}", path.display()),
        )),
    }
}

// ── Artifacts ────────────────────────────────────────────────────────────

/// `<HCOM_DIR>/integrations`: root of every published per-run artifact.
pub fn integrations_dir() -> PathBuf {
    crate::paths::hcom_path(&["integrations"])
}

/// Hex digest over named files. Names and lengths are framed so different
/// splits of the same bytes never collide.
pub fn content_digest(files: &[(&str, &[u8])]) -> String {
    let mut hasher = Sha256::new();
    for (name, content) in files {
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update((content.len() as u64).to_le_bytes());
        hasher.update(content);
    }
    let digest = hasher.finalize();
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Publish `files` (relative paths, `/`-separated) as
/// `<HCOM_DIR>/integrations/<tool>/<digest>/` and return that dir.
///
/// The digest must cover every generated byte, so put all generated content
/// (hook commands, command prefix, permission variant) in the files. Never
/// session-specific values.
pub fn publish_dir(tool: &str, files: &[(&str, &[u8])]) -> std::io::Result<PathBuf> {
    publish_dir_at(&integrations_dir(), tool, files)
}

/// [`publish_dir`] under an explicit integrations root.
///
/// Content-addressed and write-once: an existing digest dir is reused as-is
/// (a running agent may be using it) and its mtime refreshed. A new one is
/// written to a temp sibling and renamed into place, so readers never see a
/// partial dir and concurrent publishers of the same content both succeed.
/// Publishing a new digest also sweeps the tool's digests unused for
/// [`ARTIFACT_MAX_IDLE`].
pub fn publish_dir_at(
    root: &Path,
    tool: &str,
    files: &[(&str, &[u8])],
) -> std::io::Result<PathBuf> {
    let tool_dir = root.join(tool);
    let target = tool_dir.join(content_digest(files));
    if target.is_dir() {
        let _ = set_dir_mtime(&target, SystemTime::now());
        return Ok(target);
    }
    std::fs::create_dir_all(&tool_dir)?;
    // Merged caller config (e.g. Claude `--settings` with an `env` block) can
    // hold secrets. Owner-only root and tool dirs also cover digests written
    // before files were private.
    make_private_dir(root)?;
    make_private_dir(&tool_dir)?;
    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(&tool_dir)?;
    make_private_dir(staging.path())?;
    for (name, content) in files {
        let path = staging.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_private_file(&path, content)?;
    }
    match std::fs::rename(staging.path(), &target) {
        Ok(()) => {}
        // Lost the race to another publisher of the same digest.
        Err(_) if target.is_dir() => return Ok(target),
        Err(error) => return Err(error),
    }
    if let Some(cutoff) = SystemTime::now().checked_sub(ARTIFACT_MAX_IDLE) {
        sweep_idle_artifacts(&tool_dir, &target, cutoff);
    }
    Ok(target)
}

/// How long a digest dir can go without a launch using it before it is swept.
/// Resume and fork republish, so only a session that has run this long and
/// then reloads its plugin from disk could miss a swept dir.
pub const ARTIFACT_MAX_IDLE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Remove digest and crashed staging dirs in `tool_dir` last used before
/// `cutoff`, keeping `keep`. Best effort: a failure only leaves the dir.
fn sweep_idle_artifacts(tool_dir: &Path, keep: &Path, cutoff: SystemTime) {
    let Ok(entries) = std::fs::read_dir(tool_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_artifact = name.starts_with(".staging-")
            || (name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit()));
        if path == keep || !is_artifact {
            continue;
        }
        let idle = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|modified| modified < cutoff);
        if idle && let Err(error) = std::fs::remove_dir_all(&path) {
            crate::log::log_warn(
                "launcher",
                "runtime.artifact_sweep_failed",
                &format!("{}: {error}", path.display()),
            );
        }
    }
}

fn set_dir_mtime(path: &Path, time: SystemTime) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_WRITE_ATTRIBUTES; FILE_FLAG_BACKUP_SEMANTICS opens a directory.
        options.access_mode(0x100).custom_flags(0x0200_0000);
    }
    options.open(path)?.set_modified(time)
}

#[cfg(unix)]
fn make_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn make_private_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn write_private_file(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(content)
}

/// Publish a single file and return its path.
pub fn publish_file(tool: &str, name: &str, content: &[u8]) -> std::io::Result<PathBuf> {
    Ok(publish_dir(tool, &[(name, content)])?.join(name))
}

/// True when `value` (a path or `file://` URL) points into hcom's
/// integrations dir. Used to strip replayed hcom-injected flag values.
pub fn is_hcom_runtime_path(value: &str) -> bool {
    is_runtime_path_under(&integrations_dir(), value)
}

fn is_runtime_path_under(root: &Path, value: &str) -> bool {
    let path = match value.strip_prefix("file://") {
        Some(rest) => PathBuf::from(url_path_to_native(rest)),
        None => PathBuf::from(value),
    };
    // `<root>/../user.ts` starts with `<root>` component-wise; resolve `..`
    // lexically first so a user path is never taken for hcom's.
    if lexically_normalized(&path).starts_with(lexically_normalized(root)) {
        return true;
    }
    // Symlinked HCOM_DIR (e.g. /tmp vs /private/tmp): compare canonical forms.
    match (std::fs::canonicalize(root), std::fs::canonicalize(&path)) {
        (Ok(root), Ok(path)) => path.starts_with(root),
        _ => false,
    }
}

fn lexically_normalized(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `file://` URL for an absolute path. Percent-encodes everything outside the
/// RFC 3986 unreserved set (plus `/`); Windows drive paths become
/// `file:///C:/…`.
pub fn file_url(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let raw = raw.strip_prefix("//?/").unwrap_or(&raw);
    let mut out = String::from("file://");
    if !raw.starts_with('/') {
        out.push('/');
    }
    for (i, byte) in raw.bytes().enumerate() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/')
            // Drive colon (`C:`) stays literal.
            || (byte == b':' && i == 1);
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn url_path_to_native(rest: &str) -> String {
    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            decoded.push(byte);
            i += 3;
            continue;
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    let path = String::from_utf8_lossy(&decoded).into_owned();
    // `/C:/x` → `C:/x`
    let b = path.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        path[1..].to_string()
    } else {
        path
    }
}

// ── Args ─────────────────────────────────────────────────────────────────

fn separator_index(args: &[String]) -> usize {
    args.iter().position(|a| a == "--").unwrap_or(args.len())
}

/// Insert `injected` before the first `--` (so it stays an option, not prompt
/// text), or append when there is none.
pub fn insert_before_separator(args: &mut Vec<String>, injected: impl IntoIterator<Item = String>) {
    let at = separator_index(args);
    args.splice(at..at, injected);
}

/// Match `token` against `flags` in `--flag VALUE` / `--flag=VALUE` form.
/// Returns `Some(Some(value))` for the `=` form, `Some(None)` for the split
/// form, `None` when it's not one of `flags`.
fn match_value_flag<'a>(token: &'a str, flags: &[&str]) -> Option<Option<&'a str>> {
    for flag in flags {
        if token == *flag {
            return Some(None);
        }
        if let Some(value) = token
            .strip_prefix(flag)
            .and_then(|rest| rest.strip_prefix('='))
        {
            return Some(Some(value));
        }
    }
    None
}

/// Remove occurrences of value-taking `flags` (before any `--`) whose value
/// satisfies `remove`, returning the removed values in order. A trailing flag
/// with no value is left alone.
fn remove_flag_values(
    args: &mut Vec<String>,
    flags: &[&str],
    mut remove: impl FnMut(&str) -> bool,
) -> Vec<String> {
    let end = separator_index(args);
    let mut kept = Vec::with_capacity(args.len());
    let mut taken = Vec::new();
    let mut i = 0;
    while i < end {
        let token = &args[i];
        match match_value_flag(token, flags) {
            Some(Some(value)) if remove(value) => {
                taken.push(value.to_string());
                i += 1;
            }
            Some(None) if i + 1 < end && remove(&args[i + 1]) => {
                taken.push(args[i + 1].clone());
                i += 2;
            }
            Some(None) if i + 1 < end => {
                kept.push(token.clone());
                kept.push(args[i + 1].clone());
                i += 2;
            }
            _ => {
                kept.push(token.clone());
                i += 1;
            }
        }
    }
    kept.extend(args.drain(end..));
    *args = kept;
    taken
}

/// Remove every `flag VALUE` / `flag=VALUE` occurrence (before any `--`) and
/// return the values in order. For single-valued flags where the tool uses the
/// last occurrence, merge into `values.last()` and inject one flag.
pub fn take_flag_values(args: &mut Vec<String>, flags: &[&str]) -> Vec<String> {
    remove_flag_values(args, flags, |_| true)
}

/// Drop hcom-injected values (runtime artifacts, and legacy managed paths the
/// adapter recognises) from replayed args so the injection is rebuilt, not doubled.
pub fn strip_replayed_args(adapter: &PerRunAdapter, args: &mut Vec<String>) {
    strip_flag_values_where(args, adapter.managed_value_flags, is_hcom_runtime_path);
    if let Some(strip_legacy) = adapter.strip_legacy_args {
        strip_legacy(args);
    }
}

fn strip_flag_values_where(args: &mut Vec<String>, flags: &[&str], managed: impl Fn(&str) -> bool) {
    if !flags.is_empty() {
        remove_flag_values(args, flags, managed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[cfg(unix)]
    #[test]
    fn published_artifacts_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let integrations = root.path().join("integrations");
        let dir = publish_dir_at(&integrations, "claude", &[("a/settings.json", b"{}")]).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&integrations), 0o700);
        assert_eq!(mode(&integrations.join("claude")), 0o700);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("a/settings.json")), 0o600);
    }

    #[test]
    fn digest_frames_names_and_content() {
        let a = content_digest(&[("ab", b"c")]);
        let b = content_digest(&[("a", b"bc")]);
        assert_ne!(a, b);
        assert_eq!(a, content_digest(&[("ab", b"c")]));
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn publish_is_content_addressed_and_reused() {
        let root = tempfile::tempdir().unwrap();
        let files: &[(&str, &[u8])] = &[("hooks/hooks.json", b"{}"), ("plugin.json", b"{\"n\":1}")];
        let first = publish_dir_at(root.path(), "copilot", files).unwrap();
        assert_eq!(
            std::fs::read(first.join("hooks/hooks.json")).unwrap(),
            b"{}"
        );
        let second = publish_dir_at(root.path(), "copilot", files).unwrap();
        assert_eq!(first, second);
        let other = publish_dir_at(root.path(), "copilot", &[("plugin.json", b"{}")]).unwrap();
        assert_ne!(first, other);
        // No staging leftovers.
        let leftovers: Vec<_> = std::fs::read_dir(root.path().join("copilot"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".staging-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn publishing_a_new_digest_sweeps_idle_ones() {
        let root = tempfile::tempdir().unwrap();
        let idle = publish_dir_at(root.path(), "pi", &[("hcom.ts", b"v1")]).unwrap();
        let recent = publish_dir_at(root.path(), "pi", &[("hcom.ts", b"v2")]).unwrap();
        let crashed = root.path().join("pi/.staging-x");
        let user = root.path().join("pi/notes");
        std::fs::create_dir(&crashed).unwrap();
        std::fs::create_dir(&user).unwrap();
        let old = SystemTime::now() - ARTIFACT_MAX_IDLE - Duration::from_secs(60);
        for dir in [&idle, &crashed, &user] {
            set_dir_mtime(dir, old).unwrap();
        }

        // Reuse refreshes the mtime instead of sweeping.
        set_dir_mtime(&recent, old).unwrap();
        assert_eq!(
            publish_dir_at(root.path(), "pi", &[("hcom.ts", b"v2")]).unwrap(),
            recent
        );
        assert!(idle.exists());

        publish_dir_at(root.path(), "pi", &[("hcom.ts", b"v3")]).unwrap();
        assert!(!idle.exists());
        assert!(!crashed.exists());
        assert!(recent.exists());
        assert!(user.exists());
    }

    #[test]
    fn concurrent_publishers_of_same_content_all_succeed() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let root = root_path.clone();
                std::thread::spawn(move || {
                    publish_dir_at(&root, "pi", &[("hcom.ts", b"export default 1")]).unwrap()
                })
            })
            .collect();
        let dirs: Vec<PathBuf> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(dirs.windows(2).all(|w| w[0] == w[1]));
        assert_eq!(
            std::fs::read(dirs[0].join("hcom.ts")).unwrap(),
            b"export default 1"
        );
    }

    #[test]
    fn runtime_path_recognises_paths_and_file_urls() {
        let root = tempfile::tempdir().unwrap();
        let integrations = root.path().join("integrations");
        let artifact = publish_dir_at(&integrations, "opencode", &[("hcom.ts", b"x")])
            .unwrap()
            .join("hcom.ts");
        assert!(is_runtime_path_under(
            &integrations,
            &artifact.to_string_lossy()
        ));
        assert!(is_runtime_path_under(&integrations, &file_url(&artifact)));
        assert!(!is_runtime_path_under(
            &integrations,
            "/home/u/.pi/agent/extensions/hcom.ts"
        ));
        assert!(!is_runtime_path_under(&integrations, "./my-plugin"));
        let escaped = integrations
            .join("opencode")
            .join("..")
            .join("..")
            .join("user.ts");
        assert!(!is_runtime_path_under(
            &integrations,
            &escaped.to_string_lossy()
        ));
        assert!(!is_runtime_path_under(&integrations, &file_url(&escaped)));
    }

    #[test]
    fn file_url_encodes_spaces_and_windows_drives() {
        assert_eq!(
            file_url(Path::new("/Users/a b/.hcom/x.ts")),
            "file:///Users/a%20b/.hcom/x.ts"
        );
        assert_eq!(
            file_url(Path::new(r"C:\Users\a b\x.ts")),
            "file:///C:/Users/a%20b/x.ts"
        );
        assert_eq!(
            url_path_to_native("/C:/Users/a%20b/x.ts"),
            "C:/Users/a b/x.ts"
        );
        assert_eq!(url_path_to_native("/Users/a%20b/x.ts"), "/Users/a b/x.ts");
    }

    #[test]
    fn insert_goes_before_separator() {
        let mut args = sv(&["--model", "m", "--", "prompt text"]);
        insert_before_separator(&mut args, sv(&["--settings", "{}"]));
        assert_eq!(
            args,
            sv(&["--model", "m", "--settings", "{}", "--", "prompt text"])
        );
        let mut args = sv(&["prompt"]);
        insert_before_separator(&mut args, sv(&["-e", "x"]));
        assert_eq!(args, sv(&["prompt", "-e", "x"]));
    }

    #[test]
    fn take_flag_values_handles_both_forms_and_stops_at_separator() {
        let mut args = sv(&[
            "--settings",
            "a.json",
            "-p",
            "--settings={\"x\":1}",
            "--",
            "--settings",
            "text",
        ]);
        let taken = take_flag_values(&mut args, &["--settings"]);
        assert_eq!(taken, sv(&["a.json", "{\"x\":1}"]));
        assert_eq!(args, sv(&["-p", "--", "--settings", "text"]));
    }

    #[test]
    fn take_flag_values_leaves_trailing_valueless_flag() {
        let mut args = sv(&["--model", "m", "--settings"]);
        assert!(take_flag_values(&mut args, &["--settings"]).is_empty());
        assert_eq!(args, sv(&["--model", "m", "--settings"]));
    }

    #[test]
    fn take_flag_values_does_not_match_flag_prefixes() {
        let mut args = sv(&["--settings-file", "x", "-e", "y"]);
        assert!(take_flag_values(&mut args, &["--settings"]).is_empty());
        assert_eq!(args, sv(&["--settings-file", "x", "-e", "y"]));
    }

    #[test]
    fn strip_keeps_user_values_and_drops_managed_ones() {
        let managed = |v: &str| v.starts_with("/hcom/integrations/");
        let mut args = sv(&[
            "--plugin-dir",
            "/hcom/integrations/cursor/abc",
            "--plugin-dir",
            "/mine",
            "--plugin-dir=/hcom/integrations/cursor/old",
            "-e",
            "/hcom/integrations/pi/x/hcom.ts",
            "-e",
            "user.ts",
        ]);
        strip_flag_values_where(&mut args, &["--plugin-dir", "-e"], managed);
        assert_eq!(args, sv(&["--plugin-dir", "/mine", "-e", "user.ts"]));
    }

    #[test]
    fn plan_runs_prepare_then_cleanup_then_permissions() {
        use std::sync::Mutex;
        static CALLS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
        fn prepare(ctx: &LaunchCtx) -> Result<RuntimeInjection> {
            CALLS.lock().unwrap().push("prepare");
            let mut args = ctx.args.clone();
            insert_before_separator(&mut args, sv(&["--x", "1"]));
            Ok(RuntimeInjection {
                args,
                env: vec![("K".into(), "V".into())],
            })
        }
        fn cleanup(_: &LaunchCtx) -> Result<()> {
            CALLS.lock().unwrap().push("cleanup");
            Ok(())
        }
        fn permissions(_: &LaunchCtx) -> Result<()> {
            CALLS.lock().unwrap().push("permissions");
            Ok(())
        }
        let adapter = PerRunAdapter {
            prepare,
            cleanup_legacy: cleanup,
            ensure_permissions: Some(permissions),
            managed_value_flags: &[],
            strip_legacy_args: None,
        };
        let mut ctx = LaunchCtx::ambient(Tool::Claude, false);
        ctx.args = sv(&["--", "hi"]);
        let injection = plan(&adapter, &ctx).unwrap();
        assert_eq!(injection.args, sv(&["--x", "1", "--", "hi"]));
        assert_eq!(
            *CALLS.lock().unwrap(),
            vec!["prepare", "cleanup", "permissions"]
        );
    }

    #[test]
    fn prepare_failure_stops_plan_but_cleanup_failure_does_not() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CLEANUPS: AtomicUsize = AtomicUsize::new(0);
        static PERMISSIONS: AtomicUsize = AtomicUsize::new(0);
        fn bad_prepare(_: &LaunchCtx) -> Result<RuntimeInjection> {
            anyhow::bail!("bad caller --settings")
        }
        fn ok_prepare(_: &LaunchCtx) -> Result<RuntimeInjection> {
            Ok(RuntimeInjection::default())
        }
        fn cleanup(_: &LaunchCtx) -> Result<()> {
            CLEANUPS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn bad_cleanup(_: &LaunchCtx) -> Result<()> {
            anyhow::bail!("malformed /x/hooks.json")
        }
        fn permissions(_: &LaunchCtx) -> Result<()> {
            PERMISSIONS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        let ctx = LaunchCtx::ambient(Tool::Codex, true);

        let adapter = PerRunAdapter {
            prepare: bad_prepare,
            cleanup_legacy: cleanup,
            ensure_permissions: Some(permissions),
            managed_value_flags: &[],
            strip_legacy_args: None,
        };
        let err = format!("{:#}", plan(&adapter, &ctx).unwrap_err());
        assert!(err.contains("bad caller --settings"), "{err}");
        assert_eq!(CLEANUPS.load(Ordering::SeqCst), 0);

        let adapter = PerRunAdapter {
            prepare: ok_prepare,
            cleanup_legacy: bad_cleanup,
            ensure_permissions: Some(permissions),
            managed_value_flags: &[],
            strip_legacy_args: None,
        };
        plan(&adapter, &ctx).unwrap();
        assert_eq!(PERMISSIONS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn legacy_cleanup_warning_names_the_file_and_fix() {
        let path = Path::new("/x/hooks.json");
        let error = anyhow::anyhow!("permission denied")
            .context(LegacyFile::write(path, FIX_REMOVE_HCOM_HOOKS));
        let warning = legacy_cleanup_warning("codex", &error.context("outer"));
        assert!(warning.contains("/x/hooks.json"), "{warning}");
        assert!(warning.contains("permission denied"), "{warning}");
        assert!(warning.contains(FIX_REMOVE_HCOM_HOOKS), "{warning}");
        assert!(warning.contains("twice"), "{warning}");

        let error =
            anyhow::anyhow!("bad JSON").context(LegacyFile::read(path, FIX_REMOVE_HCOM_HOOKS));
        let warning = legacy_cleanup_warning("codex", &error);
        assert!(!warning.contains("twice"), "{warning}");
    }

    #[test]
    fn owned_file_removal_tries_every_path() {
        let dir = tempfile::tempdir().unwrap();
        let [a, b, c] = ["a.ts", "b.ts", "c.ts"].map(|n| dir.path().join(n));
        for path in [&a, &b, &c] {
            std::fs::write(path, "hcom").unwrap();
        }
        let owned = |path: &Path| -> std::io::Result<bool> {
            if path.ends_with("c.ts") {
                return Ok(true);
            }
            Err(std::io::Error::other(format!(
                "cannot read {}",
                path.display()
            )))
        };
        let error = remove_owned_files([a.clone(), b.clone(), c.clone()], owned).unwrap_err();
        let LegacyErrors(errors) = error.downcast::<LegacyErrors>().unwrap();
        assert_eq!(errors.len(), 2);
        assert!(
            errors
                .iter()
                .all(|e| !e.downcast_ref::<LegacyFile>().unwrap().still_loads)
        );
        assert!(!c.exists());
    }

    #[test]
    fn ctx_var_ignores_empty_values() {
        let mut ctx = LaunchCtx::ambient(Tool::Claude, false);
        ctx.env = HashMap::from([("A".into(), "".into()), ("B".into(), "b".into())]);
        assert_eq!(ctx.var("A"), None);
        assert_eq!(ctx.var("B"), Some("b"));
    }
}
