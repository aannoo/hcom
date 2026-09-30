//! Anthropic Messages codec + Claude adapter for the shared real-tool lifecycle.
//!
//! Claude Code has no fake-response mode, so a real, pinned `claude` TUI is
//! routed at `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>` and every turn is
//! scripted as Messages SSE. Unlike Codex's single Responses route, Claude also
//! POSTs `/v1/messages/count_tokens` with the same payload, so this codec routes
//! by path and classifies the NEWEST user turn (the latest message's
//! `tool_result.tool_use_id`, else its current text) — never the whole body,
//! because Claude resends the full history plus a large system prompt each turn.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::Hcom;
use super::mock_http::{RecordedRequest, Reply};
use super::real_tool::{
    FORK_PROOF, INBOUND_PROOF, INITIAL_PROOF, RESUME_PROOF, ScenarioIds, ToolCase, ToolMeta,
};

const CLAUDE_META: ToolMeta = ToolMeta {
    tool: "claude",
    binary: "claude",
    package: "@anthropic-ai/claude-code",
};

pub const MODEL: &str = "claude-sonnet-4-6";
const TOOL_FILE: &str = "toolu_lifecycle_file";
const TOOL_SHELL: &str = "toolu_lifecycle_shell";
const TOOL_SEND: &str = "toolu_lifecycle_send";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaudeStartupGate {
    Theme,
    Trust,
    Continue,
}

impl ClaudeStartupGate {
    fn label(self) -> &'static str {
        match self {
            Self::Theme => "theme prompt",
            Self::Trust => "workspace trust prompt",
            Self::Continue => "continue prompt",
        }
    }
}

pub fn claude_startup_gate(screen: &str) -> Option<ClaudeStartupGate> {
    let low = screen.to_lowercase();
    if low.contains("text style") {
        Some(ClaudeStartupGate::Theme)
    } else if low.contains("i trust this folder")
        || low.contains("is this a project")
        || low.contains("accessing workspace")
        || low.contains("do you trust")
    {
        // Check trust before the generic continue prompt: Claude may render
        // both phrases on one screen, but they still describe one gate.
        Some(ClaudeStartupGate::Trust)
    } else if low.contains("press enter to continue") {
        Some(ClaudeStartupGate::Continue)
    } else {
        None
    }
}

/// Whether the trust dialog's cursor sits on the option that accepts. Claude
/// draws the cursor as `❯`, or `>` on Windows consoles; `hcom term` prefixes
/// each row with its number (`14:`).
pub fn trust_accept_selected(screen: &str) -> bool {
    screen
        .lines()
        .map(|line| {
            let line = line.trim_start();
            line.split_once(':')
                .filter(|(row, _)| !row.is_empty() && row.bytes().all(|b| b.is_ascii_digit()))
                .map_or(line, |(_, rest)| rest)
                .trim_start()
        })
        .find(|line| line.starts_with(['❯', '>']))
        .is_some_and(|line| line.to_lowercase().contains("yes"))
}

#[test]
fn trust_accept_selected_follows_the_cursor() {
    assert!(!trust_accept_selected(
        "❯ No, exit\n  Yes, I trust this folder"
    ));
    assert!(trust_accept_selected(
        "  No, exit\n❯ Yes, I trust this folder"
    ));
    // Windows console glyph, as `hcom term` prints it (row numbers included).
    assert!(trust_accept_selected(
        "   13:    No, exit\n   14:  > Yes, I trust this folder"
    ));
    assert!(!trust_accept_selected(
        "   13:  > No, exit\n   14:    Yes, I trust this folder"
    ));
    assert!(trust_accept_selected(
        "❯ 1. Yes, I trust this folder\n  2. No, exit"
    ));
}

#[derive(Default)]
pub struct ClaudeStartupAnswers {
    theme: bool,
    trust: bool,
    continue_prompt: bool,
}

impl ClaudeStartupAnswers {
    pub fn answer_once(&mut self, gate: ClaudeStartupGate) -> bool {
        let answered = match gate {
            ClaudeStartupGate::Theme => &mut self.theme,
            ClaudeStartupGate::Trust => &mut self.trust,
            ClaudeStartupGate::Continue => &mut self.continue_prompt,
        };
        if *answered {
            false
        } else {
            *answered = true;
            true
        }
    }
}

#[test]
fn startup_gate_treats_overlapping_trust_and_continue_text_as_one_gate() {
    assert_eq!(
        claude_startup_gate("Do you trust this folder? Press Enter to continue"),
        Some(ClaudeStartupGate::Trust)
    );
}

#[test]
fn startup_answers_each_gate_once() {
    let mut answers = ClaudeStartupAnswers::default();
    assert!(answers.answer_once(ClaudeStartupGate::Trust));
    assert!(!answers.answer_once(ClaudeStartupGate::Trust));
    assert!(answers.answer_once(ClaudeStartupGate::Continue));
    assert!(!answers.answer_once(ClaudeStartupGate::Continue));
}

/// Frame `(event, json)` pairs into a Messages SSE body.
fn sse(events: &[(&str, Value)]) -> Vec<u8> {
    let mut out = String::new();
    for (event, data) in events {
        out.push_str("event: ");
        out.push_str(event);
        out.push_str("\ndata: ");
        out.push_str(&serde_json::to_string(data).expect("serialize SSE event"));
        out.push_str("\n\n");
    }
    out.into_bytes()
}

fn message_start(id: &str) -> (&'static str, Value) {
    (
        "message_start",
        json!({"type":"message_start","message":{
            "id": id, "type":"message", "role":"assistant", "content":[],
            "model": MODEL, "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }}),
    )
}

/// A complete assistant text turn (`stop_reason: end_turn`).
pub fn claude_text(id: &str, text: &str) -> Vec<u8> {
    sse(&[
        message_start(id),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
        (
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}),
        ),
        ("message_stop", json!({"type":"message_stop"})),
    ])
}

/// A complete assistant tool-use turn (`stop_reason: tool_use`). The tool input
/// is sent as a single `input_json_delta`.
pub fn claude_tool_use(id: &str, tool_id: &str, name: &str, input: &Value) -> Vec<u8> {
    let partial = serde_json::to_string(input).expect("serialize tool input");
    sse(&[
        message_start(id),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":tool_id,"name":name,"input":{}}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":partial}}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
        (
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":10}}),
        ),
        ("message_stop", json!({"type":"message_stop"})),
    ])
}

/// The newest user turn: `(tool_result id if any, concatenated current text)`.
/// Public so tool-specific tests (e.g. the approval gate) can classify turns
/// the same way without re-matching the whole body.
pub fn latest_user_turn(body: &str) -> Option<(Option<String>, String)> {
    let json: Value = serde_json::from_str(body).ok()?;
    let messages = json.get("messages")?.as_array()?;
    let last = messages.last()?;
    if last.get("role").and_then(Value::as_str) != Some("user") {
        return Some((None, String::new()));
    }
    match last.get("content") {
        Some(Value::String(text)) => Some((None, text.clone())),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    let id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    return Some((Some(id), String::new()));
                }
            }
            let text = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            Some((None, text))
        }
        _ => Some((None, String::new())),
    }
}

fn is_count_tokens(path: &str) -> bool {
    path.contains("count_tokens")
}

fn is_messages(path: &str) -> bool {
    path.contains("/v1/messages") && !is_count_tokens(path)
}

/// Claude adapter for the shared real-tool lifecycle.
#[derive(Clone)]
pub struct ClaudeCase;

/// Seed Claude's global state in an isolated `CLAUDE_CONFIG_DIR`: onboarding
/// done (no theme picker) and workspace trust accepted for `trusted` dirs
/// (subdirectories inherit it). Pass `&[]` to keep the trust dialog.
pub fn seed_claude_state(claude_home: &Path, trusted: &[&Path]) {
    let mut projects = serde_json::Map::new();
    for dir in trusted {
        // Claude keys projects by its resolved cwd; macOS temp dirs resolve
        // through /private, so record both spellings.
        let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        for path in [dir.to_path_buf(), canonical] {
            let key = path.to_string_lossy().replace('\\', "/");
            let key = key.strip_prefix("//?/").unwrap_or(&key).to_string();
            projects.insert(key, json!({"hasTrustDialogAccepted": true}));
        }
    }
    std::fs::create_dir_all(claude_home).expect("create Claude config dir");
    std::fs::write(
        claude_home.join(".claude.json"),
        serde_json::to_vec(&json!({"hasCompletedOnboarding": true, "projects": projects}))
            .expect("serialize Claude state"),
    )
    .expect("seed Claude state");
}

impl ToolCase for ClaudeCase {
    fn meta(&self) -> &ToolMeta {
        &CLAUDE_META
    }

    fn file_context(&self) -> &'static str {
        "tool:Write"
    }

    fn file_detail(&self, ids: &ScenarioIds) -> String {
        // Claude's Write tool reports the absolute file_path.
        ids.file_path.clone()
    }

    fn provider_base_url(&self, port: u16) -> String {
        // Claude appends `/v1/messages` itself, so the base URL is bare.
        format!("http://127.0.0.1:{port}")
    }

    fn prepare(&self, h: &Hcom, base_url: &str) {
        // Skip Claude's global first-run theme picker. On Windows, hcom's
        // synchronous launch can remain inside that picker until its readiness
        // timeout, after which the test no longer has an inject endpoint to
        // drive it. Also pre-trust the workspace: startup screens are not what
        // the lifecycle tests, and each costs a delivery-start fallback. The
        // approval test re-seeds without trust so the trust gate stays covered.
        seed_claude_state(&h.claude_home, &[&h.workspace]);

        // Provider routing + isolated config must survive hcom's CI=1 clean-shell
        // launch rebuild, so they go through the `$HCOM_DIR/env` passthrough.
        let claude_home = h
            .claude_home
            .to_str()
            .expect("UTF-8 claude home")
            .to_string();
        h.set_launch_envs(&[
            ("ANTHROPIC_BASE_URL", base_url),
            ("ANTHROPIC_AUTH_TOKEN", "hcom-real-test-dummy-token"),
            ("CLAUDE_CONFIG_DIR", &claude_home),
            ("DISABLE_LOGIN_COMMAND", "1"),
            ("DISABLE_UPDATES", "1"),
            ("DISABLE_TELEMETRY", "1"),
            ("DISABLE_GROWTHBOOK", "1"),
            ("DISABLE_ERROR_REPORTING", "1"),
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
            ("CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL", "1"),
            ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1"),
            ("CLAUDE_CODE_DISABLE_THINKING", "1"),
            ("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", "1"),
            ("DISABLE_PROMPT_CACHING", "1"),
            ("ENABLE_TOOL_SEARCH", "false"),
            ("CLAUDE_CODE_FORCE_SESSION_PERSISTENCE", "1"),
            // Debug log lands in claude-home/debug/, inside the preserved
            // failure dir, so a stalled tool call can be traced afterwards.
            ("DEBUG", "1"),
        ]);
    }

    fn launch_args(&self, _h: &Hcom) -> Vec<String> {
        // The lifecycle only needs Write and Bash. Auto-allow those tools while
        // denying every other prompt; the separate approval test uses Claude's
        // default mode. --bare/--safe-mode would disable the hooks under test.
        vec![
            "--model".to_string(),
            MODEL.to_string(),
            "--permission-mode".to_string(),
            "dontAsk".to_string(),
            "--allowedTools".to_string(),
            "Write,Bash".to_string(),
            // hcom installs its hooks into the user settings.json under
            // CLAUDE_CONFIG_DIR; load that source so they activate.
            "--setting-sources".to_string(),
            "user".to_string(),
        ]
    }

    fn drive_startup(&self, h: &Hcom, name: &str) {
        // Onboarding and trust are pre-seeded in prepare(), but the approval
        // test re-seeds without trust, so its fresh workspace surfaces the trust
        // dialog, which hcom reports as `launch_blocked`. Accepting trust here is
        // what lets Claude register hooks at all ("Skipping ... hook execution -
        // workspace trust not accepted" otherwise). Keep the theme handling as a
        // compatibility fallback for Claude versions that ignore the seeded state.
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut last_screen = String::new();
        let mut answers = ClaudeStartupAnswers::default();
        while Instant::now() < deadline {
            let (_, json, _) = h.run(["term", name, "--json"]);
            let (screen_code, screen, _) = h.run(["term", name]);
            last_screen = screen.clone();
            let gate = claude_startup_gate(&screen);
            // Ready once the empty input prompt is up and no gate is visible —
            // mode-agnostic, so it also serves the default-mode approval test.
            if screen_code == 0 && json.contains("\"prompt_empty\":true") && gate.is_none() {
                return;
            }
            // Claude >= 2.1.2xx preselects "No, exit" on the trust dialog, so a
            // bare Enter would quit. Move the cursor onto the accepting option
            // first; the next frame re-checks before anything is submitted.
            if gate == Some(ClaudeStartupGate::Trust) && !trust_accept_selected(&screen) {
                let (code, stdout, stderr) = h.run(["term", "inject", name, "\u{1b}[B"]);
                assert_eq!(
                    code, 0,
                    "drive_startup: moving to the trust option failed: stdout={stdout} stderr={stderr}"
                );
                std::thread::sleep(Duration::from_millis(800));
                continue;
            }
            if let Some(gate) = gate.filter(|gate| answers.answer_once(*gate)) {
                let what = gate.label();
                // term inject reports delivery to the PTY. Send each gate answer
                // once so a stale frame cannot leak an extra Enter into the
                // ready input prompt.
                let (code, stdout, stderr) = h.run(["term", "inject", name, "", "--enter"]);
                assert_eq!(
                    code, 0,
                    "drive_startup: inject for {what} failed: stdout={stdout} stderr={stderr}"
                );
            }
            std::thread::sleep(Duration::from_millis(800));
        }
        panic!(
            "drive_startup: Claude did not reach the input prompt within 90s; last screen:\n{last_screen}\n{}",
            h.diagnostics()
        );
    }

    fn is_followup_turn(&self, body: &str) -> bool {
        // A tool-result follow-up, OR a count_tokens request (same payload, no
        // `stream`), so neither is mistaken for a fresh user turn.
        matches!(latest_user_turn(body), Some((Some(_), _))) || !body.contains("\"stream\"")
    }

    fn is_turn_request(&self, req: &RecordedRequest) -> bool {
        is_messages(&req.path)
    }

    fn delivery_envelope_markers(&self) -> &'static [&'static str] {
        &["<hcom>"]
    }

    fn respond(&self, req: &RecordedRequest, ids: &ScenarioIds) -> Reply {
        // Claude probes the gateway's reachability with `HEAD /` once per launch.
        if req.method.eq_ignore_ascii_case("HEAD") {
            return Reply::Empty(200);
        }
        if is_count_tokens(&req.path) {
            return Reply::Json(json!({"input_tokens": 1}).to_string());
        }
        if !is_messages(&req.path) {
            return Reply::Status(404);
        }
        let Some((tool_result, text)) = latest_user_turn(&req.body) else {
            return Reply::Status(500);
        };
        if let Some(tool_id) = tool_result {
            return match tool_id.as_str() {
                TOOL_FILE => Reply::Sse(claude_tool_use(
                    "msg_shell",
                    TOOL_SHELL,
                    "Bash",
                    &json!({
                        "command": format!("echo {} > {}", ids.initial, ids.shell_rel),
                        "description": "write the lifecycle shell marker",
                    }),
                )),
                TOOL_SHELL => Reply::Sse(claude_tool_use(
                    "msg_send",
                    TOOL_SEND,
                    "Bash",
                    &json!({ "command": ids.send_cmd, "description": "send the hcom message" }),
                )),
                TOOL_SEND => Reply::Sse(claude_text(
                    "msg_done",
                    &format!("{INITIAL_PROOF} {}", ids.initial),
                )),
                _ => Reply::Status(500),
            };
        }
        if text.contains(&ids.resume) {
            Reply::Sse(claude_text(
                "msg_resume",
                &format!("{RESUME_PROOF} {}", ids.resume),
            ))
        } else if text.contains(&ids.fork) {
            Reply::Sse(claude_text(
                "msg_fork",
                &format!("{FORK_PROOF} {}", ids.fork),
            ))
        } else if text.contains(&ids.inbound) {
            Reply::Sse(claude_text(
                "msg_inbound",
                &format!("{INBOUND_PROOF} {}", ids.inbound),
            ))
        } else if text.contains(&ids.initial) {
            Reply::Sse(claude_tool_use(
                "msg_file",
                TOOL_FILE,
                "Write",
                &json!({ "file_path": ids.file_path, "content": ids.initial }),
            ))
        } else {
            Reply::Status(500)
        }
    }
}
