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

/// Build argv for launch: `hcom [count] [tool] [--tag T] [--terminal T] [tool-specific prompt]`
///
/// Prompt handling varies per tool:
///   claude: `-p "prompt"` (headless) or bare positional (interactive)
///   gemini: `-i "prompt"` (interactive only, headless not supported)
///   codex:  bare positional (interactive)
///   opencode: `--prompt "prompt"`
///
/// Always includes `--no-run-here` so the launcher opens a new terminal window/tab
/// instead of running the agent in the TUI's own terminal (which would cause the
/// agent to launch into a piped subprocess with no interactive I/O).
pub fn build_launch_argv(
    tool: Tool,
    count: u8,
    tag: &str,
    headless: bool,
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
    // Tool-specific prompt flags
    if !prompt.is_empty() {
        match tool {
            Tool::Claude | Tool::Codex => {
                if headless {
                    argv.push("-p".into());
                }
                argv.push(prompt.into());
            }
            Tool::Gemini => {
                // gemini uses -i for initial prompt; headless not supported
                argv.extend(["-i".into(), prompt.into()]);
            }
            Tool::OpenCode | Tool::Kilo | Tool::Cline => {
                argv.extend(["--prompt".into(), prompt.into()]);
            }
            Tool::Adhoc => {}
        }
    } else if headless {
        // headless without prompt (claude only)
        argv.push("-p".into());
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
        let argv = build_launch_argv(Tool::Claude, 2, "review", true, "kitty", "hello");
        assert_eq!(argv[0], "2");
        assert_eq!(argv[1], "claude");
        assert!(argv.contains(&"--no-run-here".into()));
        assert!(argv.contains(&"--tag".into()));
        assert!(argv.contains(&"review".into()));
        assert!(argv.contains(&"--terminal".into()));
        assert!(argv.contains(&"kitty".into()));
        assert!(argv.contains(&"-p".into()));
        assert!(argv.contains(&"hello".into()));
    }

    #[test]
    fn launch_argv_always_includes_no_run_here() {
        let argv = build_launch_argv(Tool::Gemini, 1, "", false, "default", "");
        assert_eq!(
            argv,
            vec!["1", "gemini", "--no-run-here", "--terminal", "default"]
        );
    }

    #[test]
    fn launch_argv_gemini_prompt_uses_dash_i() {
        let argv = build_launch_argv(Tool::Gemini, 1, "", false, "kitty", "fix the bug");
        assert_eq!(
            argv,
            vec![
                "1",
                "gemini",
                "--no-run-here",
                "--terminal",
                "kitty",
                "-i",
                "fix the bug"
            ]
        );
    }

    #[test]
    fn launch_argv_codex_prompt_is_positional() {
        let argv = build_launch_argv(Tool::Codex, 1, "", false, "tmux", "do task");
        assert_eq!(
            argv,
            vec![
                "1",
                "codex",
                "--no-run-here",
                "--terminal",
                "tmux",
                "do task"
            ]
        );
    }
}
