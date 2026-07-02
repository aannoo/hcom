//! TUI RPC types and helpers.
//!
//! All TUI actions are dispatched via commands::run_native() (subprocess of
//! same binary). No daemon dependency.

use serde::Deserialize;

use crate::tui::model::Tool;

// ── Response type ────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
pub struct Response {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Response {
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }

    pub fn combined_output_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self.stdout.lines().map(String::from).collect();
        if !self.stderr.is_empty() {
            lines.extend(self.stderr.lines().map(String::from));
        }
        lines
    }

    pub fn error_message(&self) -> String {
        if let Some(line) = first_non_empty_line(&self.stderr) {
            return line.to_string();
        }
        if let Some(line) = first_non_empty_line(&self.stdout) {
            return line.to_string();
        }
        format!("exit code {}", self.exit_code)
    }
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines().find(|line| !line.trim().is_empty())
}

// ── Helpers ──────────────────────────────────────────────────────

/// Build argv for a TUI-initiated launch:
/// `hcom [count] <tool> --no-run-here [--tag T] [--terminal T] [--headless] [--hcom-prompt P]`.
///
/// Per-tool prompt shaping is NOT duplicated here: the prompt is passed through
/// the launcher's generic `--hcom-prompt`, which applies the tool's
/// `InitialPromptShape` from `integration_spec` (positional, `--prompt`,
/// `--prompt-interactive`, `-i`, claude's `-- <positional>`, …). This keeps the
/// tool argv contract in one place (the launcher) instead of drifting between
/// here and `integration_spec`.
///
/// `--headless` is offered for every tool (it routes through the PTY headless
/// wrapper). `headless_pty` is claude-only: claude's headless default is a live
/// PTY-backed session; when it is `false` the launch opts into `-p` print mode
/// instead (also kept alive by the stop-hook loop, but it draws from a separate
/// Agent SDK credit pool).
///
/// Always includes `--no-run-here` so the launcher opens a new terminal window/tab
/// instead of running the agent in the TUI's own terminal (which would cause the
/// agent to launch into a piped subprocess with no interactive I/O).
pub fn build_launch_argv(
    tool: Tool,
    count: u8,
    tag: &str,
    headless: bool,
    headless_pty: bool,
    terminal: &str,
    prompt: &str,
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        count.to_string(),
        tool.name().into(),
        "--no-run-here".into(),
    ];
    if !tag.is_empty() {
        argv.extend(["--tag".into(), tag.into()]);
    }
    if !terminal.is_empty() {
        argv.extend(["--terminal".into(), terminal.into()]);
    }
    if headless {
        argv.push("--headless".into());
        // claude-only: headless defaults to the live PTY session (no extra flag);
        // when headless_pty is false, opt into `-p` print mode.
        if matches!(tool, Tool::Claude) && !headless_pty {
            argv.push("-p".into());
        }
    }
    if !prompt.is_empty() {
        argv.extend(["--hcom-prompt".into(), prompt.into()]);
    }
    argv
}

pub fn parse_command_argv(cmd: &str) -> Result<Vec<String>, String> {
    let argv = shell_words::split(cmd).map_err(|e| format!("parse command: {}", e))?;
    if argv.is_empty() {
        return Err("empty command".into());
    }
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::{Response, build_launch_argv, parse_command_argv};
    use crate::tui::model::Tool;

    #[test]
    fn response_error_prefers_stderr() {
        let resp = Response {
            exit_code: 2,
            stdout: "stdout line\n".into(),
            stderr: "stderr line\n".into(),
        };
        assert_eq!(resp.error_message(), "stderr line");
    }

    #[test]
    fn response_error_falls_back_to_stdout_then_exit_code() {
        let with_stdout = Response {
            exit_code: 4,
            stdout: "only stdout\n".into(),
            stderr: "".into(),
        };
        assert_eq!(with_stdout.error_message(), "only stdout");

        let empty = Response {
            exit_code: 7,
            stdout: "".into(),
            stderr: "".into(),
        };
        assert_eq!(empty.error_message(), "exit code 7");
    }

    #[test]
    fn parse_command_argv_respects_quotes() {
        let argv = parse_command_argv(r#"config -i nova tag "my tag""#).unwrap();
        assert_eq!(argv, vec!["config", "-i", "nova", "tag", "my tag"]);
    }

    #[test]
    fn parse_command_argv_rejects_empty() {
        let err = parse_command_argv("   ").unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn launch_argv_numeric_count_first() {
        let argv = build_launch_argv(Tool::Claude, 2, "review", true, false, "kitty", "hello");
        assert_eq!(argv[0], "2");
        assert_eq!(argv[1], "claude");
        assert!(argv.contains(&"--no-run-here".into()));
        assert!(argv.contains(&"--tag".into()));
        assert!(argv.contains(&"review".into()));
        assert!(argv.contains(&"--terminal".into()));
        assert!(argv.contains(&"kitty".into()));
        // Headless → --headless; prompt → generic --hcom-prompt.
        assert!(argv.contains(&"--headless".into()));
        assert!(argv.contains(&"--hcom-prompt".into()));
        assert!(argv.contains(&"hello".into()));
    }

    #[test]
    fn launch_argv_always_includes_no_run_here() {
        let argv = build_launch_argv(Tool::Gemini, 1, "", false, false, "default", "");
        assert_eq!(
            argv,
            vec!["1", "gemini", "--no-run-here", "--terminal", "default"]
        );
    }

    #[test]
    fn launch_argv_prompt_is_tool_agnostic() {
        // Per-tool prompt shaping now lives in the launcher (InitialPromptShape);
        // the TUI always forwards the prompt via the generic --hcom-prompt flag,
        // regardless of tool.
        for tool in [
            Tool::Gemini,
            Tool::Pi,
            Tool::Omp,
            Tool::Antigravity,
            Tool::Codex,
            Tool::Cursor,
            Tool::Copilot,
            Tool::OpenCode,
        ] {
            let argv = build_launch_argv(tool.clone(), 1, "", false, false, "kitty", "fix the bug");
            assert_eq!(
                argv,
                vec![
                    "1".to_string(),
                    tool.name().to_string(),
                    "--no-run-here".to_string(),
                    "--terminal".to_string(),
                    "kitty".to_string(),
                    "--hcom-prompt".to_string(),
                    "fix the bug".to_string(),
                ],
                "{tool:?} should forward prompt via --hcom-prompt"
            );
        }
    }

    #[test]
    fn launch_argv_headless_for_non_claude_tool() {
        // Headless is now offered for every tool; non-claude routes through the
        // PTY headless wrapper via a plain --headless (no --pty).
        let argv = build_launch_argv(Tool::Gemini, 1, "", true, false, "kitty", "do task");
        assert_eq!(
            argv,
            vec![
                "1",
                "gemini",
                "--no-run-here",
                "--terminal",
                "kitty",
                "--headless",
                "--hcom-prompt",
                "do task"
            ]
        );
    }

    #[test]
    fn launch_argv_claude_headless_pty_is_default_no_extra_flag() {
        // Claude's headless default is the live PTY session — just --headless, no
        // extra flag (headless_pty = true).
        let argv = build_launch_argv(Tool::Claude, 1, "", true, true, "kitty", "do task");
        assert_eq!(
            argv,
            vec![
                "1",
                "claude",
                "--no-run-here",
                "--terminal",
                "kitty",
                "--headless",
                "--hcom-prompt",
                "do task"
            ]
        );
    }

    #[test]
    fn launch_argv_claude_headless_print_adds_dash_p() {
        // Claude print mode (headless_pty = false) opts into `-p`.
        let argv = build_launch_argv(Tool::Claude, 1, "", true, false, "kitty", "do task");
        assert_eq!(
            argv,
            vec![
                "1",
                "claude",
                "--no-run-here",
                "--terminal",
                "kitty",
                "--headless",
                "-p",
                "--hcom-prompt",
                "do task"
            ]
        );
    }
}
