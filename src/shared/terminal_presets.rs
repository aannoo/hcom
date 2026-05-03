//! Terminal preset configuration for agent window launching.

use std::sync::LazyLock;

/// Terminal preset configuration.
#[derive(Debug, Clone)]
pub struct TerminalPreset {
    /// Binary to check for availability (None = check app bundle).
    pub binary: Option<&'static str>,
    /// App name for macOS bundle detection (e.g., "kitty", "WezTerm").
    pub app_name: Option<&'static str>,
    /// Command template with {script} placeholder.
    pub open: &'static str,
    /// Close command template with {pane_id} placeholder (None = no close API).
    pub close: Option<&'static str>,
    /// Env var that contains the pane ID.
    pub pane_id_env: Option<&'static str>,
    /// Supported platforms.
    pub platforms: &'static [&'static str],
}

const fn p(
    binary: Option<&'static str>,
    app_name: Option<&'static str>,
    open: &'static str,
    close: Option<&'static str>,
    pane_id_env: Option<&'static str>,
    platforms: &'static [&'static str],
) -> TerminalPreset {
    TerminalPreset {
        binary,
        app_name,
        open,
        close,
        pane_id_env,
        platforms,
    }
}

const DL: &[&str] = &["Darwin", "Linux"];
const DLW: &[&str] = &["Darwin", "Linux", "Windows"];

pub static TERMINAL_PRESETS: LazyLock<Vec<(&'static str, TerminalPreset)>> = LazyLock::new(|| {
    vec![
        // macOS native
        (
            "terminal.app",
            p(
                None,
                None,
                "open -a Terminal {script}",
                None,
                None,
                &["Darwin"],
            ),
        ),
        (
            "iterm",
            p(
                None,
                None,
                "open -a iTerm {script}",
                None,
                None,
                &["Darwin"],
            ),
        ),
        (
            "ghostty",
            p(
                None,
                None,
                "open -na Ghostty.app --args -e bash {script}",
                None,
                None,
                &["Darwin"],
            ),
        ),
        (
            "cmux",
            p(
                Some("cmux"),
                Some("cmux"),
                "cmux new-workspace --command 'bash {script}'",
                Some("cmux close-workspace --workspace {pane_id}"),
                Some("CMUX_WORKSPACE_ID"),
                &["Darwin"],
            ),
        ),
        // Cross-platform (smart presets)
        (
            "kitty",
            p(
                Some("kitty"),
                Some("kitty"),
                "kitty --env HCOM_PROCESS_ID={process_id} {script}",
                Some("kitten @ close-window --match id:{pane_id}"),
                None,
                DL,
            ),
        ),
        (
            "kitty-window",
            p(
                Some("kitty"),
                Some("kitty"),
                "kitty --env HCOM_PROCESS_ID={process_id} {script}",
                Some("kitten @ close-window --match id:{pane_id}"),
                None,
                DL,
            ),
        ),
        (
            "wezterm",
            p(
                Some("wezterm"),
                Some("WezTerm"),
                "wezterm start -- bash {script}",
                Some("wezterm cli kill-pane --pane-id {pane_id}"),
                Some("WEZTERM_PANE"),
                DLW,
            ),
        ),
        (
            "wezterm-window",
            p(
                Some("wezterm"),
                Some("WezTerm"),
                "wezterm start -- bash {script}",
                Some("wezterm cli kill-pane --pane-id {pane_id}"),
                Some("WEZTERM_PANE"),
                DLW,
            ),
        ),
        (
            "alacritty",
            p(
                Some("alacritty"),
                Some("Alacritty"),
                "alacritty -e bash {script}",
                None,
                None,
                DLW,
            ),
        ),
        (
            "warp",
            p(
                None,
                Some("Warp"),
                "open warp://launch/hcom-{process_id}",
                None,
                None,
                &["Darwin"],
            ),
        ),
        // Tab utilities
        (
            "ttab",
            p(Some("ttab"), None, "ttab {script}", None, None, &["Darwin"]),
        ),
        (
            "wttab",
            p(
                Some("wttab"),
                None,
                "wttab {script}",
                None,
                None,
                &["Windows"],
            ),
        ),
        // Linux terminals
        (
            "gnome-terminal",
            p(
                Some("gnome-terminal"),
                None,
                "gnome-terminal --window -- bash {script}",
                None,
                None,
                &["Linux"],
            ),
        ),
        (
            "konsole",
            p(
                Some("konsole"),
                None,
                "konsole -e bash {script}",
                None,
                None,
                &["Linux"],
            ),
        ),
        (
            "xterm",
            p(
                Some("xterm"),
                None,
                "xterm -e bash {script}",
                None,
                None,
                &["Linux"],
            ),
        ),
        (
            "tilix",
            p(
                Some("tilix"),
                None,
                "tilix -e bash {script}",
                None,
                None,
                &["Linux"],
            ),
        ),
        (
            "terminator",
            p(
                Some("terminator"),
                None,
                "terminator -x bash {script}",
                None,
                None,
                &["Linux"],
            ),
        ),
        (
            "zellij",
            p(
                Some("zellij"),
                None,
                "zellij action new-pane -- bash {script}",
                Some("zellij action close-pane --pane-id {pane_id}"),
                Some("ZELLIJ_PANE_ID"),
                DL,
            ),
        ),
        (
            "waveterm",
            p(
                Some("wsh"),
                None,
                "wsh run -- bash {script}",
                Some("wsh deleteblock -b {pane_id}"),
                Some("WAVETERM_BLOCKID"),
                DL,
            ),
        ),
        // Windows terminals
        (
            "windows-terminal",
            p(
                Some("wt"),
                None,
                "wt -- bash {script}",
                None,
                None,
                &["Windows"],
            ),
        ),
        (
            "mintty",
            p(
                Some("mintty"),
                None,
                "mintty bash {script}",
                None,
                None,
                &["Windows"],
            ),
        ),
        // Within-terminal splits/tabs
        (
            "tmux",
            p(
                Some("tmux"),
                None,
                "tmux new-session -d bash {script}",
                Some("tmux kill-pane -t {pane_id}"),
                Some("TMUX_PANE"),
                DL,
            ),
        ),
        (
            "tmux-split",
            p(
                Some("tmux"),
                None,
                "tmux split-window -h {script}",
                Some("tmux kill-pane -t {pane_id}"),
                Some("TMUX_PANE"),
                DL,
            ),
        ),
        (
            "wezterm-tab",
            p(
                Some("wezterm"),
                Some("WezTerm"),
                "wezterm cli spawn -- bash {script}",
                Some("wezterm cli kill-pane --pane-id {pane_id}"),
                Some("WEZTERM_PANE"),
                DLW,
            ),
        ),
        (
            "wezterm-split",
            p(
                Some("wezterm"),
                Some("WezTerm"),
                "wezterm cli split-pane --top-level --right -- bash {script}",
                Some("wezterm cli kill-pane --pane-id {pane_id}"),
                Some("WEZTERM_PANE"),
                DLW,
            ),
        ),
        (
            "kitty-tab",
            p(
                Some("kitten"),
                Some("kitty"),
                "kitten @ launch --type=tab --env HCOM_PROCESS_ID={process_id} -- bash {script}",
                Some("kitten @ close-window --match id:{pane_id}"),
                None,
                DL,
            ),
        ),
        (
            "kitty-split",
            p(
                Some("kitten"),
                Some("kitty"),
                "kitten @ launch --type=window --env HCOM_PROCESS_ID={process_id} -- bash {script}",
                Some("kitten @ close-window --match id:{pane_id}"),
                None,
                DL,
            ),
        ),
    ]
});

/// Look up a terminal preset by name (case-sensitive).
pub fn get_terminal_preset(name: &str) -> Option<&TerminalPreset> {
    TERMINAL_PRESETS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, p)| p)
}

/// Map environment variables to terminal presets for auto-detection.
/// Used for same-terminal PTY launches to enable close-on-kill.
pub const TERMINAL_ENV_MAP: &[(&str, &str)] = &[
    // Multiplexers first — more specific than bare terminals (run inside them)
    ("CMUX_WORKSPACE_ID", "cmux"),
    ("TMUX_PANE", "tmux-split"),
    ("ZELLIJ_PANE_ID", "zellij"),
    // GPU/rich terminals with split APIs
    ("WEZTERM_PANE", "wezterm-split"),
    ("KITTY_WINDOW_ID", "kitty-split"),
    ("WAVETERM_BLOCKID", "waveterm"),
    // Bare terminal emulators (no split API, but open in correct app)
    ("GHOSTTY_RESOURCES_DIR", "ghostty"),
    ("ITERM_SESSION_ID", "iterm"),
    ("ALACRITTY_WINDOW_ID", "alacritty"),
    ("GNOME_TERMINAL_SCREEN", "gnome-terminal"),
    ("KONSOLE_DBUS_WINDOW", "konsole"),
    ("TERMINATOR_UUID", "terminator"),
    ("TILIX_ID", "tilix"),
    ("WT_SESSION", "windows-terminal"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_terminal_presets_count() {
        assert_eq!(TERMINAL_PRESETS.len(), 27);
    }

    #[test]
    fn test_terminal_preset_lookup() {
        let preset = get_terminal_preset("kitty").unwrap();
        assert_eq!(preset.binary, Some("kitty"));
        assert!(preset.close.is_some());

        assert!(get_terminal_preset("nonexistent").is_none());
    }

    #[test]
    fn test_kitty_tab_close_matches_window_id() {
        let preset = get_terminal_preset("kitty-tab").unwrap();
        assert_eq!(
            preset.close,
            Some("kitten @ close-window --match id:{pane_id}")
        );
    }
}
