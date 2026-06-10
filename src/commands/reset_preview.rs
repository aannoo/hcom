use crate::db::HcomDb;

use super::reset::ResetTarget;

struct ResetPreviewState {
    instance_count: usize,
    event_count: i64,
    names_display: String,
    plural: &'static str,
}

fn load_preview_state(db: &HcomDb) -> ResetPreviewState {
    let event_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap_or(0);

    let mut local_instances = Vec::new();
    if let Ok(mut stmt) = db.conn().prepare(
        "SELECT name FROM instances WHERE origin_device_id IS NULL OR origin_device_id = ''",
    ) && let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0))
    {
        for name in rows.filter_map(|r| r.ok()) {
            local_instances.push(name);
        }
    }

    let instance_count = local_instances.len();
    let names_display = if local_instances.is_empty() {
        "(none)".to_string()
    } else {
        let shown: Vec<&str> = local_instances.iter().take(5).map(|s| s.as_str()).collect();
        let suffix = if local_instances.len() > 5 {
            " ..."
        } else {
            ""
        };
        format!("{}{suffix}", shown.join(", "))
    };

    ResetPreviewState {
        instance_count,
        event_count,
        names_display,
        plural: if instance_count != 1 { "s" } else { "" },
    }
}

fn hook_tool_specs() -> impl Iterator<Item = &'static crate::integration_spec::IntegrationSpec> {
    crate::integration_spec::ALL
        .iter()
        .copied()
        .filter(|spec| spec.released && !spec.hooks.names.is_empty())
}

fn hook_preview_lines() -> String {
    hook_tool_specs()
        .map(|spec| {
            format!(
                "  \u{2022} Remove hooks from {} ({})",
                spec.label,
                spec.tool.hooks_settings_path()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn hook_tool_labels(separator: &str) -> String {
    hook_tool_specs()
        .map(|spec| spec.label)
        .collect::<Vec<_>>()
        .join(separator)
}

fn render_hooks_preview() -> String {
    let hcom_cmd = "hcom";
    let actions = hook_preview_lines();
    format!(
        "\n== RESET HOOKS PREVIEW ==\n\
         This will remove hcom hooks from tool configs.\n\n\
         Actions:\n{actions}\n\n\
         To reinstall: hcom hooks add\n\n\
         Add --go flag and run again to proceed:\n  \
         {hcom_cmd} --go reset hooks\n"
    )
}

fn render_reset_all_preview(state: &ResetPreviewState) -> String {
    let hcom_cmd = "hcom";
    format!(
        "\n== RESET ALL PREVIEW ==\n\
         This will stop all instances, archive the database, remove hooks, and reset config.\n\n\
         Current state:\n  \
         \u{2022} {instance_count} local instance{plural}: {names_display}\n  \
         \u{2022} {event_count} events in database\n\n\
         Actions:\n  \
         1. Stop all {instance_count} local instances (kills processes, logs snapshots)\n  \
         2. Archive database to ~/.hcom/archive/session-<timestamp>/\n  \
         3. Delete database (hcom.db)\n  \
         4. Remove hooks from {hook_labels} configs\n  \
         5. Archive and delete config.toml + env\n  \
         6. Clear device identity (new UUID on next relay)\n\n\
         Add --go flag and run again to proceed:\n  \
         {hcom_cmd} --go reset all\n",
        instance_count = state.instance_count,
        plural = state.plural,
        names_display = state.names_display,
        event_count = state.event_count,
        hook_labels = hook_tool_labels("/"),
    )
}

fn render_reset_preview(state: &ResetPreviewState) -> String {
    let hcom_cmd = "hcom";
    format!(
        "\n== RESET PREVIEW ==\n\
         This will archive and clear the current hcom session.\n\n\
         Current state:\n  \
         \u{2022} {instance_count} instance{plural}: {names_display}\n  \
         \u{2022} {event_count} events in database\n\n\
         Actions:\n  \
         1. Archive database to ~/.hcom/archive/session-<timestamp>/\n  \
         2. Delete database (hcom.db, hcom.db-wal, hcom.db-shm)\n  \
         3. Log reset event to fresh database\n  \
         4. Sync with relay (push reset, pull fresh state)\n\n\
         Note: Instance rows are deleted but snapshots preserved in archive.\n      \
         Query archived sessions with: {hcom_cmd} archive\n\n\
         Add --go flag and run again to proceed:\n  \
         {hcom_cmd} --go reset\n",
        instance_count = state.instance_count,
        plural = state.plural,
        names_display = state.names_display,
        event_count = state.event_count,
    )
}

/// Print reset preview for AI tools (shows what will be destroyed).
pub(crate) fn print_reset_preview(target: Option<ResetTarget>, db: &HcomDb) {
    let state = load_preview_state(db);
    let preview = match target {
        Some(ResetTarget::Hooks) => render_hooks_preview(),
        Some(ResetTarget::All) => render_reset_all_preview(&state),
        _ => render_reset_preview(&state),
    };
    println!("{preview}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_previews_cover_all_released_hook_tools() {
        let labels = hook_tool_labels("/");
        assert!(labels.contains("Kimi"));
        for spec in hook_tool_specs() {
            assert!(labels.split('/').any(|label| label == spec.label));
        }

        let preview = render_hooks_preview();
        for spec in hook_tool_specs() {
            assert!(preview.contains(spec.label));
        }
    }
}
