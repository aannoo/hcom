//! Cursor launch preprocessing: workspace trust markers.

use std::path::{Path, PathBuf};

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
        return Ok(());
    }
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
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
}
