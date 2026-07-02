//! Cursor launch preprocessing: workspace trust markers.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

/// Cursor stores per-workspace state under `~/.cursor/projects/<slug>`.
///
/// This mirrors Cursor's path slugging: path separators and punctuation become
/// dashes while ASCII letters, digits, underscores, and existing dashes survive.
pub(crate) fn cursor_project_slug(workspace: &Path) -> String {
    workspace
        .to_string_lossy()
        .split(std::path::MAIN_SEPARATOR)
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                        ch
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// Print/headless flags that would break hcom's PTY delivery model.
///
/// hcom always runs cursor-agent inside a PTY (interactive, or HeadlessPty for
/// background) — never `--print`. The `beforeSubmitPrompt`/`stop` hooks that
/// carry message delivery do **not** fire in `--print` mode, so a stray
/// `-p`/`--print` leaking in from `HCOM_CURSOR_ARGS` or a resumed instance's
/// baked `launch_args` would silently break delivery unless rejected.
/// `--stream-partial-output` only works with `--print` + stream-json, so reject
/// that companion flag as well.
const CURSOR_PRINT_FLAGS: &[&str] = &["-p", "--print", "--stream-partial-output"];

/// Reject print/headless flags that would break hcom's PTY delivery model.
pub(crate) fn validate_cursor_args(tokens: &[String]) -> Vec<String> {
    let found: Vec<&str> = CURSOR_PRINT_FLAGS
        .iter()
        .copied()
        .filter(|flag| {
            tokens
                .iter()
                .any(|token| crate::tools::launch_arg_validation::long_flag_matches(token, flag))
        })
        .collect();
    if found.is_empty() {
        return Vec::new();
    }
    vec![format!(
        "Cursor print mode is not supported by `hcom cursor-agent`: {} would disable the PTY hooks used for message delivery. Remove the print flag and launch the interactive or `--headless` PTY session instead.",
        found.join(", ")
    )]
}

fn cursor_projects_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".cursor")
        .join("projects")
}

pub(crate) fn cursor_trust_marker_path(workspace: &Path) -> PathBuf {
    let normalized = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    cursor_projects_dir()
        .join(cursor_project_slug(&normalized))
        .join(".workspace-trusted")
}

fn validate_existing_cursor_trust_marker(marker: &Path, workspace: &Path) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(marker).with_context(|| {
        format!(
            "failed to read Cursor workspace trust marker {}",
            marker.display()
        )
    })?;
    let value: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("invalid Cursor workspace trust marker {}", marker.display()))?;
    let recorded = value
        .get("workspacePath")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cursor workspace trust marker {} has no workspacePath",
                marker.display()
            )
        })?;
    let recorded_path = PathBuf::from(recorded);
    let normalized_recorded = recorded_path.canonicalize().unwrap_or(recorded_path);
    if normalized_recorded != workspace {
        bail!(
            "Cursor workspace trust marker collision at {}: marker records workspacePath '{}' but launch requested '{}'. Cursor's native project slug maps both paths to the same directory; remove or relocate the stale marker only after confirming which workspace should be trusted.",
            marker.display(),
            normalized_recorded.display(),
            workspace.display()
        );
    }
    Ok(())
}

/// Pre-seed Cursor's workspace trust marker for PTY launches.
///
/// Cursor's `--trust` flag only works in print mode. hcom keeps Cursor
/// interactive inside a PTY, so the marker must exist before process startup.
pub(crate) fn ensure_cursor_workspace_trusted(workspace: &Path) -> anyhow::Result<()> {
    let normalized = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let marker = cursor_trust_marker_path(&normalized);
    if marker.exists() {
        return validate_existing_cursor_trust_marker(&marker, &normalized);
    }
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    eprintln!(
        "[hcom] Auto-approving Cursor Agent folder trust prompt for {} (marker: {})",
        normalized.display(),
        marker.display()
    );
    let trusted_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let content = serde_json::to_string_pretty(&serde_json::json!({
        "trustedAt": trusted_at,
        "workspacePath": normalized.to_string_lossy(),
        "trustMethod": "hcom-launch",
    }))?;
    crate::paths::atomic_write_io(&marker, &content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unix-only: slugifies POSIX absolute paths; a Windows path (drive letter,
    // backslashes) produces a different, platform-specific slug.
    #[cfg(unix)]
    #[test]
    fn cursor_project_slug_matches_cursor_layout() {
        assert_eq!(
            cursor_project_slug(Path::new("/private/tmp/cursor-hook-probe.sdxJ")),
            "private-tmp-cursor-hook-probe-sdxJ"
        );
        assert_eq!(
            cursor_project_slug(Path::new("/Users/anno/Dev/hook-comms-public")),
            "Users-anno-Dev-hook-comms-public"
        );
    }

    #[test]
    fn validate_cursor_args_rejects_print_and_companions() {
        let tokens: Vec<String> = [
            "--model",
            "composer-2.5",
            "-p",
            "--print",
            "--stream-partial-output",
            "--force",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let errors = validate_cursor_args(&tokens);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("-p, --print, --stream-partial-output"));
        assert!(errors[0].contains("not supported"));
    }

    #[test]
    fn validate_cursor_args_rejects_equals_form_print() {
        let errors = validate_cursor_args(&["--print=json".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("--print"));
    }

    #[test]
    fn existing_trust_marker_accepts_matching_workspace_and_rejects_slug_collision() {
        let dir = tempfile::tempdir().unwrap();
        let trusted = dir.path().join("acme.prod");
        let colliding = dir.path().join("acme-prod");
        std::fs::create_dir_all(&trusted).unwrap();
        std::fs::create_dir_all(&colliding).unwrap();
        assert_eq!(
            cursor_project_slug(&trusted),
            cursor_project_slug(&colliding)
        );
        let marker = dir.path().join(".workspace-trusted");
        std::fs::write(
            &marker,
            serde_json::json!({ "workspacePath": trusted.canonicalize().unwrap() }).to_string(),
        )
        .unwrap();

        assert!(
            validate_existing_cursor_trust_marker(&marker, &trusted.canonicalize().unwrap())
                .is_ok()
        );
        let error =
            validate_existing_cursor_trust_marker(&marker, &colliding.canonicalize().unwrap())
                .unwrap_err();
        assert!(error.to_string().contains("trust marker collision"));
    }
}
