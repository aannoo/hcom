use anyhow::{Context, Result};

use crate::hooks::runtime::{self, LaunchCtx, PerRunAdapter, RuntimeInjection};

pub const PLUGIN_SOURCE: &str = include_str!("../../omp_plugin/hcom.ts");
const PLUGIN_FILENAME: &str = "hcom.ts";

pub static PER_RUN: PerRunAdapter = PerRunAdapter {
    prepare: prepare_per_run,
    cleanup_legacy: cleanup_legacy_per_run,
    ensure_permissions: None,
    managed_value_flags: &["-e", "--extension"],
    strip_legacy_args: Some(strip_managed_extension_args),
};

pub fn get_omp_plugin_path() -> std::path::PathBuf {
    effective_plugin_path(&LaunchCtx::ambient(crate::tool::Tool::Omp, false))
}

fn raw_ctx_var<'a>(ctx: &'a LaunchCtx, key: &str) -> Option<&'a str> {
    if cfg!(windows) {
        ctx.env
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.as_str())
    } else {
        ctx.env.get(key).map(String::as_str)
    }
}

fn active_profile(ctx: &LaunchCtx) -> Option<String> {
    let value = match raw_ctx_var(ctx, "OMP_PROFILE") {
        Some(value) => Some(value),
        None => raw_ctx_var(ctx, "PI_PROFILE"),
    }?;
    let normalized = value.trim();
    (!normalized.is_empty() && normalized != "default").then(|| normalized.to_string())
}

fn effective_plugin_path(ctx: &LaunchCtx) -> std::path::PathBuf {
    let home = ctx
        .var("HOME")
        .map(std::path::PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_default();
    let config_name = ctx.var("PI_CONFIG_DIR").unwrap_or(".omp");
    let profile = active_profile(ctx);
    let agent_dir = match profile {
        Some(name) => home
            .join(config_name)
            .join("profiles")
            .join(name)
            .join("agent"),
        None => ctx
            .path_var("PI_CODING_AGENT_DIR")
            .unwrap_or_else(|| home.join(config_name).join("agent")),
    };
    agent_dir.join("extensions").join(PLUGIN_FILENAME)
}

fn prepare_per_run(ctx: &LaunchCtx) -> Result<RuntimeInjection> {
    let path = runtime::publish_file("omp", PLUGIN_FILENAME, PLUGIN_SOURCE.as_bytes())
        .context("Cannot publish OMP runtime extension")?;
    let mut args = ctx.args.clone();
    runtime::insert_before_separator(
        &mut args,
        ["-e".to_string(), path.to_string_lossy().into_owned()],
    );
    Ok(RuntimeInjection {
        args,
        env: Vec::new(),
    })
}

/// Under a project-local HCOM_DIR the old installer wrote to
/// `<tool root>/.omp/extensions/` instead of the agent dir.
fn project_local_legacy_path() -> Option<std::path::PathBuf> {
    let root = crate::runtime_env::tool_config_root();
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default());
    (root != home).then(|| root.join(".omp").join("extensions").join(PLUGIN_FILENAME))
}

fn remove_owned(paths: impl IntoIterator<Item = std::path::PathBuf>) -> Result<()> {
    runtime::remove_owned_files(paths, is_hcom_owned)
}

fn cleanup_legacy_per_run(ctx: &LaunchCtx) -> Result<()> {
    remove_owned(std::iter::once(effective_plugin_path(ctx)).chain(project_local_legacy_path()))
}

/// Remove hcom's managed OMP extension injection (`-e <hcom.ts>` /
/// `--extension …`, incl. the `=` forms) from a stored or replayed launch-arg
/// vector, preserving every user-supplied extension and its ordering. An entry
/// is treated as managed when its path is the current plugin path, an existing
/// hcom-owned file, or — for a moved/missing managed file — a narrow lexical
/// match (basename `hcom.ts` directly under an `extensions` directory).
///
/// Idempotent. Callers strip stored args before snapshotting and reinjecting so
/// a stale plugin path from an older hcom/config layout is not replayed
/// alongside the freshly injected current path (which could fail startup or load
/// hcom twice). A genuine `-e other.ts` user extension always survives.
pub fn strip_managed_extension_args(args: &mut Vec<String>) {
    let current = get_omp_plugin_path();
    let is_managed = |value: &str| -> bool {
        let path = std::path::Path::new(value);
        if runtime::is_hcom_runtime_path(value) || path == current.as_path() {
            return true;
        }
        if is_hcom_owned(path).unwrap_or(false)
            || crate::hooks::pi::is_hcom_owned(path).unwrap_or(false)
        {
            return true;
        }
        // Moved/missing managed file only: basename hcom.ts under an `extensions`
        // dir. Gated on !exists so an EXISTING user `-e …/extensions/hcom.ts`
        // with unrelated contents (which is_hcom_owned already rejected) is kept
        // — only exact-current and hcom-owned files are removed when present.
        !path.exists()
            && path.file_name().and_then(|n| n.to_str()) == Some(PLUGIN_FILENAME)
            && path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                == Some("extensions")
    };
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let tok = args[i].as_str();
        // Everything after `--` is prompt text, not flags.
        if tok == "--" {
            out.extend_from_slice(&args[i..]);
            break;
        }
        // Two-token forms: `-e PATH` / `--extension PATH`.
        if (tok == "-e" || tok == "--extension") && i + 1 < args.len() && args[i + 1] != "--" {
            if is_managed(&args[i + 1]) {
                i += 2;
                continue;
            }
            out.push(args[i].clone());
            out.push(args[i + 1].clone());
            i += 2;
            continue;
        }
        // Equals forms: `--extension=PATH` / `-e=PATH`.
        if let Some(value) = tok
            .strip_prefix("--extension=")
            .or_else(|| tok.strip_prefix("-e="))
            && is_managed(value)
        {
            i += 1;
            continue;
        }
        out.push(args[i].clone());
        i += 1;
    }
    *args = out;
}

pub fn is_hcom_owned(path: &std::path::Path) -> std::io::Result<bool> {
    runtime::file_is_hcom_owned(path, |content| {
        content == PLUGIN_SOURCE || content.contains("customType: \"hcom-bootstrap\"")
    })
}

pub fn remove_omp_plugin() -> Result<()> {
    remove_owned(std::iter::once(get_omp_plugin_path()).chain(project_local_legacy_path()))
}
