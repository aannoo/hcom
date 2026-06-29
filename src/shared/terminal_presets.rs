//! Terminal preset configuration for agent window launching.

use std::sync::LazyLock;

/// An argument-vector template: `argv[0]` is the executable, the rest are
/// argument templates (one element per argument; no shell splitting). Each
/// element may contain placeholders like `{script}` or `{pane_id}` that are
/// substituted per-element at launch time.
pub type ArgvTemplate = &'static [&'static str];

/// A per-platform argv template. `default` covers Unix (Darwin + Linux) and any
/// platform without a specific override; `windows` is a Windows-only override
/// (e.g. launching the generated `.ps1` directly via PowerShell). When both are
/// `None`, the API (open or close) is unavailable.
#[derive(Debug, Clone, Copy)]
pub struct PlatformArgv {
    pub default: Option<ArgvTemplate>,
    pub windows: Option<ArgvTemplate>,
}

impl PlatformArgv {
    /// Select the argv template for the given platform. Windows falls back to
    /// `default` when no Windows-specific override is present.
    pub const fn select(&self, is_windows: bool) -> Option<ArgvTemplate> {
        if is_windows {
            if self.windows.is_some() {
                self.windows
            } else {
                self.default
            }
        } else {
            self.default
        }
    }
}

/// Terminal preset configuration.
#[derive(Debug, Clone, Copy)]
pub struct TerminalPreset {
    /// Binary to check for availability (None = check app bundle).
    pub binary: Option<&'static str>,
    /// App name for macOS bundle detection (e.g., "kitty", "WezTerm").
    pub app_name: Option<&'static str>,
    /// Open command argv template (per-platform), with a `{script}` placeholder.
    pub open: PlatformArgv,
    /// Close command argv template (per-platform), with a `{pane_id}` placeholder
    /// (both slots None = no close API).
    pub close: PlatformArgv,
    /// Env var that contains the pane ID.
    pub pane_id_env: Option<&'static str>,
    /// Supported platforms.
    pub platforms: &'static [&'static str],
}

/// An argv template available on all platforms (Windows reuses the default).
const fn argv(default: ArgvTemplate) -> PlatformArgv {
    PlatformArgv {
        default: Some(default),
        windows: None,
    }
}

/// An argv template with a distinct Windows variant.
const fn argv_win(default: ArgvTemplate, windows: ArgvTemplate) -> PlatformArgv {
    PlatformArgv {
        default: Some(default),
        windows: Some(windows),
    }
}

/// No open/close API on any platform.
const NONE_ARGV: PlatformArgv = PlatformArgv {
    default: None,
    windows: None,
};

const fn p(
    binary: Option<&'static str>,
    app_name: Option<&'static str>,
    open: PlatformArgv,
    close: PlatformArgv,
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
                argv(&["open", "-a", "Terminal", "{script}"]),
                NONE_ARGV,
                None,
                &["Darwin"],
            ),
        ),
        (
            "iterm",
            p(
                None,
                None,
                argv(&["open", "-a", "iTerm", "{script}"]),
                NONE_ARGV,
                None,
                &["Darwin"],
            ),
        ),
        (
            "ghostty",
            p(
                None,
                None,
                argv(&["open", "-na", "Ghostty.app", "--args", "-e", "bash", "{script}"]),
                NONE_ARGV,
                None,
                &["Darwin"],
            ),
        ),
        (
            "cmux",
            p(
                Some("cmux"),
                Some("cmux"),
                // `bash {script}` is a single argv element on purpose — cmux
                // re-parses its `--command` value internally.
                argv(&["cmux", "new-workspace", "--command", "bash {script}"]),
                argv(&["cmux", "close-workspace", "--workspace", "{pane_id}"]),
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
                argv(&["kitty", "--env", "HCOM_PROCESS_ID={process_id}", "{script}"]),
                argv(&["kitten", "@", "close-window", "--match", "id:{pane_id}"]),
                None,
                DL,
            ),
        ),
        (
            "kitty-window",
            p(
                Some("kitty"),
                Some("kitty"),
                argv(&["kitty", "--env", "HCOM_PROCESS_ID={process_id}", "{script}"]),
                argv(&["kitten", "@", "close-window", "--match", "id:{pane_id}"]),
                None,
                DL,
            ),
        ),
        (
            "wezterm",
            p(
                Some("wezterm"),
                Some("WezTerm"),
                argv_win(
                    &["wezterm", "start", "--", "bash", "{script}"],
                    &[
                        "wezterm", "start", "--", "powershell", "-ExecutionPolicy", "Bypass",
                        "-NoExit", "-File", "{script}",
                    ],
                ),
                argv(&["wezterm", "cli", "kill-pane", "--pane-id", "{pane_id}"]),
                Some("WEZTERM_PANE"),
                DLW,
            ),
        ),
        (
            "wezterm-window",
            p(
                Some("wezterm"),
                Some("WezTerm"),
                argv_win(
                    &["wezterm", "start", "--", "bash", "{script}"],
                    &[
                        "wezterm", "start", "--", "powershell", "-ExecutionPolicy", "Bypass",
                        "-NoExit", "-File", "{script}",
                    ],
                ),
                argv(&["wezterm", "cli", "kill-pane", "--pane-id", "{pane_id}"]),
                Some("WEZTERM_PANE"),
                DLW,
            ),
        ),
        (
            "alacritty",
            p(
                Some("alacritty"),
                Some("Alacritty"),
                argv_win(
                    &["alacritty", "-e", "bash", "{script}"],
                    &[
                        "alacritty", "-e", "powershell", "-ExecutionPolicy", "Bypass", "-NoExit",
                        "-File", "{script}",
                    ],
                ),
                NONE_ARGV,
                None,
                DLW,
            ),
        ),
        (
            "warp",
            p(
                None,
                Some("Warp"),
                argv(&["open", "warp://launch/hcom-{process_id}"]),
                NONE_ARGV,
                None,
                &["Darwin"],
            ),
        ),
        // Tab utilities
        (
            "ttab",
            p(
                Some("ttab"),
                None,
                argv(&["ttab", "{script}"]),
                NONE_ARGV,
                None,
                &["Darwin"],
            ),
        ),
        (
            "wttab",
            p(
                Some("wttab"),
                None,
                argv(&["wttab", "{script}"]),
                NONE_ARGV,
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
                argv(&["gnome-terminal", "--window", "--", "bash", "{script}"]),
                NONE_ARGV,
                None,
                &["Linux"],
            ),
        ),
        (
            "konsole",
            p(
                Some("konsole"),
                None,
                argv(&["konsole", "-e", "bash", "{script}"]),
                NONE_ARGV,
                None,
                &["Linux"],
            ),
        ),
        (
            "xterm",
            p(
                Some("xterm"),
                None,
                argv(&["xterm", "-e", "bash", "{script}"]),
                NONE_ARGV,
                None,
                &["Linux"],
            ),
        ),
        (
            "tilix",
            p(
                Some("tilix"),
                None,
                argv(&["tilix", "-e", "bash", "{script}"]),
                NONE_ARGV,
                None,
                &["Linux"],
            ),
        ),
        (
            "terminator",
            p(
                Some("terminator"),
                None,
                argv(&["terminator", "-x", "bash", "{script}"]),
                NONE_ARGV,
                None,
                &["Linux"],
            ),
        ),
        (
            "zellij",
            p(
                Some("zellij"),
                None,
                argv(&["zellij", "action", "new-pane", "--", "bash", "{script}"]),
                argv(&["zellij", "action", "close-pane", "--pane-id", "{pane_id}"]),
                Some("ZELLIJ_PANE_ID"),
                DL,
            ),
        ),
        (
            "waveterm",
            p(
                Some("wsh"),
                None,
                argv(&["wsh", "run", "--", "bash", "{script}"]),
                argv(&["wsh", "deleteblock", "-b", "{pane_id}"]),
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
                argv(&[
                    "wt", "--", "powershell", "-ExecutionPolicy", "Bypass", "-NoExit", "-File",
                    "{script}",
                ]),
                NONE_ARGV,
                None,
                &["Windows"],
            ),
        ),
        (
            "mintty",
            p(
                Some("mintty"),
                None,
                // Run the generated PowerShell script directly — never hand a
                // `.ps1` to bash (the old `mintty bash {script}` bug).
                argv(&[
                    "mintty", "powershell", "-ExecutionPolicy", "Bypass", "-NoExit", "-File",
                    "{script}",
                ]),
                NONE_ARGV,
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
                argv(&["tmux", "new-session", "-d", "bash", "{script}"]),
                argv(&["tmux", "kill-pane", "-t", "{pane_id}"]),
                Some("TMUX_PANE"),
                DL,
            ),
        ),
        (
            "tmux-split",
            p(
                Some("tmux"),
                None,
                argv(&["tmux", "split-window", "-h", "{script}"]),
                argv(&["tmux", "kill-pane", "-t", "{pane_id}"]),
                Some("TMUX_PANE"),
                DL,
            ),
        ),
        (
            "wezterm-tab",
            p(
                Some("wezterm"),
                Some("WezTerm"),
                argv_win(
                    &["wezterm", "cli", "spawn", "--", "bash", "{script}"],
                    &[
                        "wezterm", "cli", "spawn", "--", "powershell", "-ExecutionPolicy", "Bypass",
                        "-NoExit", "-File", "{script}",
                    ],
                ),
                argv(&["wezterm", "cli", "kill-pane", "--pane-id", "{pane_id}"]),
                Some("WEZTERM_PANE"),
                DLW,
            ),
        ),
        (
            "wezterm-split",
            p(
                Some("wezterm"),
                Some("WezTerm"),
                argv_win(
                    &["wezterm", "cli", "split-pane", "--top-level", "--right", "--", "bash", "{script}"],
                    &[
                        "wezterm", "cli", "split-pane", "--top-level", "--right", "--", "powershell",
                        "-ExecutionPolicy", "Bypass", "-NoExit", "-File", "{script}",
                    ],
                ),
                argv(&["wezterm", "cli", "kill-pane", "--pane-id", "{pane_id}"]),
                Some("WEZTERM_PANE"),
                DLW,
            ),
        ),
        (
            "kitty-tab",
            p(
                Some("kitten"),
                Some("kitty"),
                argv(&[
                    "kitten", "@", "launch", "--type=tab", "--env",
                    "HCOM_PROCESS_ID={process_id}", "--", "bash", "{script}",
                ]),
                argv(&["kitten", "@", "close-window", "--match", "id:{pane_id}"]),
                None,
                DL,
            ),
        ),
        (
            "kitty-split",
            p(
                Some("kitten"),
                Some("kitty"),
                argv(&[
                    "kitten", "@", "launch", "--type=window", "--env",
                    "HCOM_PROCESS_ID={process_id}", "--", "bash", "{script}",
                ]),
                argv(&["kitten", "@", "close-window", "--match", "id:{pane_id}"]),
                None,
                DL,
            ),
        ),
        // Herdr workspace manager. `agent start <name>` sets both the herdr
        // agent name AND the manual pane label, so we pass a stable
        // `{instance_name}` here (e.g. `luna`) — that keeps `herdr agent send
        // luna ...` working. The styled status label (`◉ luna [claude]`) is
        // pushed separately via `pane.rename` from the delivery loop.
        (
            "herdr",
            p(
                Some("herdr"),
                None,
                argv(&[
                    "herdr", "agent", "start", "{instance_name}", "--cwd", "{cwd}", "--no-focus",
                    "--", "bash", "{script}",
                ]),
                argv(&["herdr", "pane", "close", "{pane_id}"]),
                Some("HERDR_PANE_ID"),
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
    // Herdr — most specific, manages its own terminal panes
    ("HERDR_PANE_ID", "herdr"),
    // Multiplexers — more specific than bare terminals (run inside them)
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
        assert_eq!(TERMINAL_PRESETS.len(), 28);
    }

    #[test]
    fn test_terminal_preset_lookup() {
        let preset = get_terminal_preset("kitty").unwrap();
        assert_eq!(preset.binary, Some("kitty"));
        assert!(preset.close.select(false).is_some());

        assert!(get_terminal_preset("nonexistent").is_none());
    }

    #[test]
    fn test_kitty_tab_close_matches_window_id() {
        let preset = get_terminal_preset("kitty-tab").unwrap();
        assert_eq!(
            preset.close.select(false),
            Some(&["kitten", "@", "close-window", "--match", "id:{pane_id}"] as ArgvTemplate)
        );
    }

    #[test]
    fn test_platform_argv_select_falls_back_to_default_when_no_windows() {
        let pa = argv(&["foo", "bar"]);
        assert_eq!(pa.select(false), Some(&["foo", "bar"] as ArgvTemplate));
        assert_eq!(pa.select(true), Some(&["foo", "bar"] as ArgvTemplate));
    }

    #[test]
    fn test_wezterm_open_selects_powershell_variant_on_windows() {
        let preset = get_terminal_preset("wezterm").unwrap();
        let unix = preset.open.select(false).unwrap();
        let win = preset.open.select(true).unwrap();
        // Unix runs bash; Windows runs the .ps1 via PowerShell.
        assert!(unix.contains(&"bash"));
        assert!(!unix.contains(&"powershell"));
        assert!(win.contains(&"powershell"));
        assert!(!win.contains(&"bash"));
        assert!(win.contains(&"-File"));
    }

    #[test]
    fn test_mintty_open_contains_no_bash() {
        let preset = get_terminal_preset("mintty").unwrap();
        // mintty is Windows-only; the selected argv must avoid bash.
        let win = preset.open.select(true).unwrap();
        assert!(
            !win.contains(&"bash"),
            "mintty open must not hand a .ps1 to bash"
        );
        assert!(win.contains(&"powershell"));
        assert_eq!(win.first(), Some(&"mintty"));
    }
}
