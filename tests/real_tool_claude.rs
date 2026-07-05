//! Real Claude Code CLI vertical slice.
//!
//! Run explicitly with:
//!   cargo test --test real_tool_claude -- --ignored --nocapture --test-threads=1
//!
//! A real, pinned Claude Code interactive TUI is routed to a localhost Anthropic
//! Messages provider ([`support::claude_mock`]). The provider scripts
//! deterministic SSE turns; Claude performs its real built-in tools (`Write`,
//! `Bash`) and emits its real hcom hooks. The full lifecycle runs through the
//! shared [`support::real_tool`] runner, so Claude and Codex assert one
//! tool-independent contract — including fork, where the shared
//! `child.session_id != parent.session_id` assertion is exactly what proves
//! hcom's `CLAUDE_ENV_FILE` fork-UUID recovery, with no Claude special case.

mod support;

use serde_json::Value;
use serial_test::serial;
use std::fs;
use std::time::Duration;
use support::Hcom;
use support::claude_mock::{ClaudeCase, MODEL, claude_text, claude_tool_use, latest_user_turn};
use support::mock_http::{MockHttp, Reply};
use support::real_tool::{ToolCase, require_pinned};
use support::{parse_launch_names, unique_suffix};

#[test]
#[ignore = "requires the pinned real @anthropic-ai/claude-code binary"]
#[serial]
fn real_claude_full_lifecycle_send_fork_kill_resume_and_cleanup() {
    support::real_tool::run_full_lifecycle(ClaudeCase);
}

/// Claude's approval gate is hcom's hook-driven block path (a real
/// `PermissionRequest` hook, NOT the terminal scrape Codex needs). This drives
/// the same contract as the Codex approval test through Claude's mechanism:
/// launch in the DEFAULT permission mode so a non-allowlisted `Bash` call fires
/// `PermissionRequest` → hcom marks the instance `blocked`/`approval`; an inbound
/// hcom message is held while the prompt is up; a real approval keystroke runs
/// the command exactly once and releases the held message. The gate comes from
/// Claude's own permission UI + hook (not an `approval_policy` we hand-wrote), so
/// a regression in hcom's PermissionRequest handling fails this test.
#[test]
#[ignore = "requires the pinned real @anthropic-ai/claude-code binary"]
#[serial]
fn real_claude_approval_gate_blocks_pending_message_then_clears_on_approval() {
    let h = Hcom::new();
    let case = ClaudeCase;
    require_pinned(&h, &case);

    let suffix = unique_suffix();
    let approval_token = format!("HCOM_CLAUDE_APPROVAL_{suffix}");
    let gated_token = format!("HCOM_CLAUDE_GATED_{suffix}");
    let held_token = format!("HCOM_CLAUDE_HELD_{suffix}");
    let sender_process_id = format!("hcom-claude-approver-{suffix}");
    let sender = h.start_listening_with_process_id(&sender_process_id);

    let approval_result = h.workspace.join("approval-result.txt");
    let approval_result_text = approval_result
        .to_str()
        .expect("UTF-8 approval result path")
        .replace('\\', "/");
    // Append (not overwrite) so a duplicate execution is detectable as a second
    // line rather than an idempotent rewrite.
    let gated_cmd = format!(
        "node -e \"require('fs').appendFileSync('{approval_result_text}', '{gated_token}\\\\n')\""
    );
    const GATED_TOOL: &str = "toolu_claude_gated";

    // Two scripted turns, classified by the NEWEST turn (Claude resends full
    // history): the gated Bash call, then the final proof after its tool_result.
    // The held hcom message is released through PostToolUse and is included in
    // that tool_result; it does not create a separate user turn.
    let scenario_approval = approval_token.clone();
    let scenario_gated = gated_cmd.clone();
    let mock = MockHttp::start(move |req| {
        if req.method.eq_ignore_ascii_case("HEAD") {
            return Reply::Empty(200);
        }
        if req.path.contains("count_tokens") {
            return Reply::Json(serde_json::json!({"input_tokens": 1}).to_string());
        }
        if !req.path.contains("/v1/messages") {
            return Reply::Status(404);
        }
        let (tool_result, text) = latest_user_turn(&req.body).unwrap_or((None, String::new()));
        if let Some(id) = tool_result {
            if id == GATED_TOOL {
                Reply::Sse(claude_text(
                    "msg_approval",
                    &format!("APPROVAL_PROOF {scenario_approval}"),
                ))
            } else {
                Reply::Status(500)
            }
        } else if text.contains(&scenario_approval) {
            Reply::Sse(claude_tool_use(
                "msg_gated",
                GATED_TOOL,
                "Bash",
                &serde_json::json!({ "command": scenario_gated, "description": "gated command" }),
            ))
        } else {
            Reply::Status(500)
        }
    })
    .expect("start localhost mock provider");

    let base_url = case.provider_base_url(mock.port());
    case.prepare(&h, &base_url);

    // Default permission mode (no bypassPermissions) so a non-allowlisted Bash
    // call is gated by Claude's permission UI and PermissionRequest hook.
    let (launch_code, launch_stdout, launch_stderr) = h.run([
        "claude",
        "--headless",
        "--dir",
        h.workspace.to_str().expect("UTF-8 workspace path"),
        "--",
        "--model",
        MODEL,
        "--setting-sources",
        "user",
    ]);
    assert_eq!(
        launch_code,
        0,
        "real Claude launch failed:\n-- stdout --\n{launch_stdout}\n-- stderr --\n{launch_stderr}\n{}",
        h.diagnostics()
    );
    let launched_names = parse_launch_names(&launch_stdout);
    assert_eq!(
        launched_names.len(),
        1,
        "expected one launch placeholder name; stdout={launch_stdout}"
    );
    let name = launched_names[0].clone();

    h.eventually(
        "Claude process-bound launch",
        Duration::from_secs(40),
        || {
            let Some(instance) = h.instance_json(&name)? else {
                return Ok(None);
            };
            Ok(instance
                .get("process_bound")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                .then_some(()))
        },
    );
    // Clear the onboarding/trust gate (no bypass-mode warning in default mode).
    case.drive_startup(&h, &name);
    h.eventually(
        "Claude PTY inject endpoint",
        Duration::from_secs(40),
        || {
            let (code, stdout, _stderr) = h.run(["term", &name, "--json"]);
            if code == 0 && stdout.contains("\"prompt_empty\":true") {
                Ok(Some(()))
            } else {
                Ok(None)
            }
        },
    );

    // Drive the gated turn. The command is gated, so "accepted" is the model turn
    // arriving at the mock, never a proof file.
    let saw_gated_turn = || {
        mock.requests().iter().any(|req| {
            req.path.contains("/v1/messages")
                && !req.path.contains("count_tokens")
                && matches!(
                    latest_user_turn(&req.body),
                    Some((None, text)) if text.contains(&approval_token)
                )
        })
    };
    support::real_tool::inject_prompt_until(
        &h,
        &name,
        &format!("Run the gated approval command {approval_token}"),
        "gated approval prompt",
        saw_gated_turn,
        saw_gated_turn,
    );

    // The command must stay un-run while approval is pending.
    assert!(
        !approval_result.exists(),
        "gated command ran before approval was granted"
    );

    // An idle Claude at an approval prompt stays active; the block only latches
    // once a message is queued behind the prompt. Send one and prove hcom's
    // PermissionRequest hook flips the instance to blocked(approval).
    let (send_code, send_stdout, send_stderr) = h.run_as_process(
        &sender_process_id,
        [
            "send",
            &format!("@{name}"),
            "--intent",
            "request",
            "--",
            &held_token,
        ],
    );
    assert_eq!(
        send_code, 0,
        "held-message send failed: stdout={send_stdout} stderr={send_stderr}"
    );

    h.eventually(
        "blocked(approval) to latch from Claude's PermissionRequest hook",
        Duration::from_secs(40),
        || {
            let (code, stdout, stderr) = h.run([
                "events", "--agent", &name, "--type", "status", "--last", "50",
            ]);
            if code != 0 {
                return Err(format!("status event lookup failed: {stderr}"));
            }
            let blocked = stdout
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|event| {
                    event["data"]["status"].as_str() == Some("blocked")
                        && event["data"]["context"].as_str() == Some("approval")
                });
            Ok(blocked.then_some(()))
        },
    );
    let blocked_instance = h
        .instance_json(&name)
        .expect("list Claude while blocked")
        .expect("Claude active while blocked");
    assert_eq!(
        blocked_instance["unread_count"].as_u64(),
        Some(1),
        "held message must stay queued while the approval gate is up: {blocked_instance}"
    );
    assert!(
        !approval_result.exists(),
        "gated command ran while still blocked on approval"
    );

    // Approve. Claude's permission prompt defaults to "1. Yes", so a bare Enter
    // accepts it; this runs the command once and releases the held message.
    let (approve_code, approve_stdout, approve_stderr) =
        h.run(["term", "inject", &name, "", "--enter"]);
    assert_eq!(
        approve_code, 0,
        "approval keystroke failed: stdout={approve_stdout} stderr={approve_stderr}"
    );

    h.eventually(
        "approved command executed with the exact gated content",
        Duration::from_secs(40),
        || match fs::read_to_string(&approval_result) {
            Ok(content) if content.trim() == gated_token => Ok(Some(())),
            _ => Ok(None),
        },
    );
    let approval_transcript = h.eventually(
        "post-approval assistant turn reached the transcript",
        Duration::from_secs(40),
        || {
            let (code, stdout, stderr) = h.run(["transcript", &name, "--full"]);
            if code != 0 {
                return Err(format!("transcript failed: {stderr}"));
            }
            Ok(stdout.contains("APPROVAL_PROOF").then_some(stdout))
        },
    );
    assert!(
        approval_transcript.contains(&approval_token),
        "approval proof token missing from transcript: {approval_transcript}"
    );

    // Approval clears the block and consumes the held message.
    h.eventually(
        "held message delivered after approval",
        Duration::from_secs(40),
        || {
            let Some(instance) = h.instance_json(&name)? else {
                return Ok(None);
            };
            Ok((instance["unread_count"].as_u64() == Some(0)).then_some(()))
        },
    );
    let delivery_events = h.eventually(
        "held message delivery acknowledgement",
        Duration::from_secs(20),
        || {
            let (code, stdout, stderr) = h.run([
                "events", "--agent", &name, "--type", "status", "--last", "50",
            ]);
            if code != 0 {
                return Err(format!("status event lookup failed: {stderr}"));
            }
            let delivered = stdout
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|event| {
                    event["data"]["context"]
                        .as_str()
                        .is_some_and(|context| context == format!("deliver:{sender}"))
                })
                .count();
            Ok((delivered > 0).then_some(delivered))
        },
    );
    assert_eq!(
        delivery_events, 1,
        "held message must be acknowledged exactly once"
    );

    // Exactly-once contract — guard against duplicate execution / re-submission.
    let gated_executions = fs::read_to_string(&approval_result)
        .expect("read gated command output")
        .lines()
        .filter(|line| line.trim() == gated_token)
        .count();
    assert_eq!(
        gated_executions, 1,
        "approved command must run exactly once (append marker counts executions)"
    );
    let gated_tool_results: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(
            |req| matches!(latest_user_turn(&req.body), Some((Some(id), _)) if id == GATED_TOOL),
        )
        .collect();
    assert_eq!(
        gated_tool_results.len(),
        1,
        "Claude must POST exactly one tool_result for the approved command"
    );
    let gated_result = &gated_tool_results[0].body;
    assert!(
        gated_result.contains(&held_token)
            && gated_result.contains("<hcom>")
            && gated_result.contains(&sender),
        "approved tool_result must carry the released hcom message envelope"
    );

    let unexpected = mock.unexpected();
    assert!(
        unexpected.is_empty(),
        "mock received {} unexpected request(s):\n{}",
        unexpected.len(),
        unexpected
            .iter()
            .map(|r| format!("  {} {}", r.method, r.path))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let transport_errors = mock.transport_errors();
    assert!(
        transport_errors.is_empty(),
        "mock hit {} transport error(s):\n  {}",
        transport_errors.len(),
        transport_errors.join("\n  ")
    );
}

/// Fast regression for the fixture fix the Claude test depends on: a provider
/// var set via `set_launch_env` must land in the `$HCOM_DIR/env` passthrough,
/// which hcom overlays last so it survives the `CI=1` clean-shell launch
/// rebuild. Without this, `ANTHROPIC_BASE_URL` would be dropped and Claude would
/// call the real API. Runs without the pinned binary.
#[test]
fn fixture_launch_env_persists_to_passthrough_file() {
    let h = Hcom::new();
    h.set_launch_env("ANTHROPIC_BASE_URL", "http://127.0.0.1:65535");
    let env = fs::read_to_string(h.path().join("env")).expect("read $HCOM_DIR/env");
    assert!(
        env.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:65535"),
        "provider var missing from passthrough file:\n{env}"
    );
    assert!(
        !env.contains("HCOM_"),
        "passthrough file must not carry hcom-owned vars:\n{env}"
    );
}
