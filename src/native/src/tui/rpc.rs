//! Daemon RPC client — sends CLI requests over the Unix socket at ~/.hcom/hcomd.sock.
//!
//! The daemon expects JSON requests of the form:
//! ```json
//! { "version": 1, "request_id": "hex", "kind": "cli", "argv": [...], "env": {}, "cwd": "..." }
//! ```
//! and responds with:
//! ```json
//! { "exit_code": 0, "stdout": "...", "stderr": "..." }
//! ```

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::tui::model::Tool;

// ── Wire types ───────────────────────────────────────────────────

#[derive(Serialize)]
struct Request {
    version: u8,
    request_id: String,
    kind: &'static str,
    argv: Vec<String>,
    env: std::collections::HashMap<String, String>,
    cwd: String,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
}

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

fn socket_path() -> PathBuf {
    crate::paths::socket_path()
}

fn request_id() -> String {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}", nanos)
}

fn cwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Short timeout for interactive fire-and-forget actions.
const TIMEOUT_FAST: Duration = Duration::from_secs(3);
/// Longer timeout for commands that return meaningful output.
const TIMEOUT_SLOW: Duration = Duration::from_secs(10);

fn send_request(argv: Vec<String>) -> Result<Response, String> {
    send_request_with_timeout(argv, TIMEOUT_FAST)
}

fn send_request_slow(argv: Vec<String>) -> Result<Response, String> {
    send_request_with_timeout(argv, TIMEOUT_SLOW)
}

/// Build argv for launch: `hcom [count] [tool] [--tag T] [--terminal T] [tool-specific prompt]`
///
/// The daemon CLI dispatch recognises numeric argv[0] as a launch command.
/// `--tag` and `--terminal` are stripped by `cmd_launch_tool` before forwarding
/// to the tool's arg parser. Omitting `--terminal` uses the config.toml default.
///
/// Prompt handling varies per tool:
///   claude: `-p "prompt"` (headless) or bare positional (interactive)
///   gemini: `-i "prompt"` (interactive only, headless not supported)
///   codex:  bare positional (interactive)
///   opencode: `--prompt "prompt"`
fn build_launch_argv(
    tool: Tool,
    count: u8,
    tag: &str,
    headless: bool,
    terminal: &str,
    prompt: &str,
) -> Vec<String> {
    let mut argv: Vec<String> = vec![count.to_string(), tool.name().into()];
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
            Tool::OpenCode => {
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

fn parse_command_argv(cmd: &str) -> Result<Vec<String>, String> {
    let argv = shell_words::split(cmd).map_err(|e| format!("parse command: {}", e))?;
    if argv.is_empty() {
        return Err("empty command".into());
    }
    Ok(argv)
}

fn send_request_with_timeout(argv: Vec<String>, timeout: Duration) -> Result<Response, String> {
    let path = socket_path();
    let mut stream =
        UnixStream::connect(&path).map_err(|e| format!("connect {}: {}", path.display(), e))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("set timeout: {}", e))?;

    let req = Request {
        version: 1,
        request_id: request_id(),
        kind: "cli",
        argv,
        env: std::collections::HashMap::new(),
        cwd: cwd(),
        stdin_is_tty: false,
        stdout_is_tty: false,
    };

    let mut json = serde_json::to_string(&req).map_err(|e| format!("serialize: {}", e))?;
    json.push('\n');
    stream
        .write_all(json.as_bytes())
        .map_err(|e| format!("write: {}", e))?;
    stream.flush().map_err(|e| format!("flush: {}", e))?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read: {}", e))?;

    serde_json::from_str(&line).map_err(|e| format!("deserialize: {}", e))
}

// ── Public RPC functions ─────────────────────────────────────────

/// Send a message to one or more recipients.
pub fn rpc_send(
    recipients: &[&str],
    body: &str,
    intent: Option<&str>,
    reply_to: Option<u64>,
) -> Result<Response, String> {
    let mut argv: Vec<String> = vec!["send".into(), "--from".into(), "bigboss".into()];
    if let Some(i) = intent {
        argv.extend(["--intent".into(), i.into()]);
    }
    if let Some(id) = reply_to {
        argv.extend(["--reply-to".into(), id.to_string()]);
    }
    for r in recipients {
        argv.push(format!("@{}", r));
    }
    argv.push("--".into());
    argv.push(body.into());
    send_request(argv)
}

/// Hard-kill an agent (close pane + process).
pub fn rpc_kill(name: &str) -> Result<Response, String> {
    send_request(vec!["kill".into(), name.into()])
}

/// Fork an existing agent.
pub fn rpc_fork(name: &str) -> Result<Response, String> {
    send_request(vec!["f".into(), name.into()])
}

/// Set an agent's tag via instance config.
pub fn rpc_tag(name: &str, tag: &str) -> Result<Response, String> {
    send_request(vec![
        "config".into(),
        "-i".into(),
        name.into(),
        "tag".into(),
        tag.into(),
    ])
}

/// Launch new agent(s).
pub fn rpc_launch(
    tool: Tool,
    count: u8,
    tag: &str,
    headless: bool,
    terminal: &str,
    prompt: &str,
) -> Result<Response, String> {
    send_request(build_launch_argv(
        tool, count, tag, headless, terminal, prompt,
    ))
}

/// Kill a tracked orphan process by PID.
pub fn rpc_kill_pid(pid: u32) -> Result<Response, String> {
    send_request(vec!["kill".into(), pid.to_string()])
}

/// Relay status.
pub fn rpc_relay_status() -> Result<Response, String> {
    send_request_slow(vec!["relay".into()])
}

/// Create new relay group.
pub fn rpc_relay_new() -> Result<Response, String> {
    send_request_slow(vec!["relay".into(), "new".into()])
}

/// Connect to relay group with token.
pub fn rpc_relay_connect(token: &str) -> Result<Response, String> {
    send_request_slow(vec!["relay".into(), "connect".into(), token.into()])
}

/// Run an arbitrary hcom command (for :command mode).
pub fn rpc_command(cmd: &str) -> Result<Response, String> {
    let argv = parse_command_argv(cmd)?;
    send_request_slow(argv)
}

/// Enable or disable relay sync.
/// Uses slow timeout: disable publishes MQTT clear before returning.
pub fn rpc_relay_toggle(enable: bool) -> Result<Response, String> {
    let flag = if enable { "on" } else { "off" };
    send_request_slow(vec!["relay".into(), flag.into()])
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
        assert!(argv.contains(&"--tag".into()));
        assert!(argv.contains(&"review".into()));
        assert!(argv.contains(&"--terminal".into()));
        assert!(argv.contains(&"kitty".into()));
        assert!(argv.contains(&"-p".into()));
        assert!(argv.contains(&"hello".into()));
    }

    #[test]
    fn launch_argv_always_includes_terminal() {
        let argv = build_launch_argv(Tool::Gemini, 1, "", false, "default", "");
        assert_eq!(argv, vec!["1", "gemini", "--terminal", "default"]);
    }

    #[test]
    fn launch_argv_gemini_prompt_uses_dash_i() {
        let argv = build_launch_argv(Tool::Gemini, 1, "", false, "kitty", "fix the bug");
        assert_eq!(
            argv,
            vec!["1", "gemini", "--terminal", "kitty", "-i", "fix the bug"]
        );
    }

    #[test]
    fn launch_argv_codex_prompt_is_positional() {
        let argv = build_launch_argv(Tool::Codex, 1, "", false, "tmux", "do task");
        assert_eq!(argv, vec!["1", "codex", "--terminal", "tmux", "do task"]);
    }
}
