//! Real Codex CLI vertical slice.
//!
//! Run explicitly with:
//!   cargo test --test real_tool_codex -- --ignored --nocapture --test-threads=1
//!
//! Codex has no `--fake-responses` mode, so the test points a real, pinned
//! `codex` binary at a localhost mock of the OpenAI Responses API and scripts
//! every turn as `text/event-stream`. The external oracle (the real codex binary
//! parsing real SSE and driving its real hook/PTY plumbing) lives outside the
//! repo, so the test cannot be gamed by editing hcom to match it.
//!
//! The full lifecycle runs through the shared [`support::real_tool`] runner so
//! Codex and Claude assert one tool-independent contract; the Codex-specific
//! wire codec lives in [`support::codex_mock::CodexCase`]. Only the approval
//! gate stays here, because its trigger (hcom's `--sandbox untrusted` mode) and
//! terminal-scraped detection are Codex-specific.

mod support;

use serde_json::Value;
use serial_test::serial;
use std::fs;
use std::time::Duration;
use support::codex_mock::{
    CodexCase, MockResponses, Reply, completed, created, message, shell_call, sse,
};
use support::real_tool::{inject_prompt_until, require_pinned};
use support::{Hcom, parse_launch_names, unique_suffix};

#[test]
#[ignore = "requires the pinned real @openai/codex binary"]
#[serial]
fn real_codex_full_lifecycle_send_fork_kill_resume_and_cleanup() {
    support::real_tool::run_full_lifecycle(CodexCase);
}

/// Codex's approval gate is hcom's only PTY-driven block path, and
/// `blocked(pty:approval)` only latches when a message is pending behind a
/// visible approval prompt. This drives that exact race: hcom's `--sandbox
/// untrusted` mode (translated to `-a untrusted`) gates a scripted shell call,
/// an inbound hcom message is held while the prompt is up, then a real approval
/// keystroke must both run the command and release the held message.
///
/// Detection does NOT rely on Codex's OSC9 notification. Codex 0.139 only writes
/// `\x1b]9;Approval requested…` when its `auto` backend selects OSC9 (Ghostty/
/// iTerm2/Kitty/Warp/WezTerm only) AND the terminal is unfocused — neither holds
/// under hcom's headless PTY, so Codex falls back to the Bel backend and emits
/// no OSC9. hcom instead scrapes the approval surface from the terminal
/// (`src/pty/screen.rs`), publishes the `pty:approval` block / `pty:approval_cleared`
/// release as status events (`publish_approval_status`), and clears the gate
/// synchronously when an injected keystroke answers the prompt
/// (`clear_injected_approval`) — do not regress by reconfiguring Codex to force
/// a signal.
#[test]
#[ignore = "requires the pinned real @openai/codex binary"]
#[serial]
fn real_codex_approval_gate_blocks_pending_message_then_clears_on_approval() {
    let h = Hcom::new();
    require_pinned(&h, &CodexCase);

    let suffix = unique_suffix();
    let token = format!("HCOM_CODEX_APPROVAL_{suffix}");
    let gated_token = format!("HCOM_CODEX_GATED_{suffix}");
    let message_token = format!("HCOM_CODEX_HELD_{suffix}");
    let sender_process_id = format!("hcom-codex-approver-{suffix}");
    let sender = h.start_listening_with_process_id(&sender_process_id);

    let approval_result = h.workspace.join("approval-result.txt");
    let approval_result_text = approval_result
        .to_str()
        .expect("UTF-8 approval result path")
        .to_string();
    let gated_cmd = if cfg!(windows) {
        format!(
            "node -e \"require('fs').writeFileSync('{}', '{}')\"",
            approval_result_text
                .replace('\\', "\\\\")
                .replace('\'', "\\'"),
            gated_token.replace('\\', "\\\\").replace('\'', "\\'")
        )
    } else {
        format!("echo {gated_token} > {approval_result_text}")
    };

    // Two scripted turns: the first requests a gated shell command; the second
    // (released only after Codex runs the approved command and POSTs its
    // function_call_output) is the final assistant message.
    let scenario_token = token.clone();
    let scenario_gated = gated_cmd.clone();
    let mock = MockResponses::start(move |body: &str| {
        if body.contains("function_call_output") && body.contains("CALLG") {
            Reply::Sse(sse(&[
                created("RESP_A2"),
                message("ITEM_A2", &format!("APPROVAL_PROOF {scenario_token}")),
                completed("RESP_A2"),
            ]))
        } else if body.contains(&scenario_token) {
            Reply::Sse(sse(&[
                created("RESP_A1"),
                shell_call("CALLG", &scenario_gated),
                completed("RESP_A1"),
            ]))
        } else {
            Reply::Status(500)
        }
    })
    .expect("start mock Responses server");

    h.prepare_codex_config(&mock.base_url());

    // Drive the approval gate through hcom's real sandbox-mode translation: the
    // `untrusted` mode is `get_sandbox_flags("untrusted")` →
    // `--sandbox workspace-write -a untrusted`, gating every non-safe command on
    // user approval. The localhost model call itself is never sandboxed, so the
    // turn still reaches the mock. The mode is selected exactly as a real user
    // would — via the `codex_sandbox_mode` config knob (`HCOM_CODEX_SANDBOX_MODE`)
    // — so a regression in that translation fails this test. Setting
    // `approval_policy` directly in config.toml would force the same gate while
    // bypassing the translation, so we don't.
    let (config_code, config_stdout, config_stderr) =
        h.run(["config", "codex_sandbox_mode", "untrusted"]);
    assert_eq!(
        config_code, 0,
        "set codex_sandbox_mode failed: stdout={config_stdout} stderr={config_stderr}"
    );

    let (launch_code, launch_stdout, launch_stderr) = h.run([
        "codex",
        "--headless",
        "--dir",
        h.workspace.to_str().expect("UTF-8 workspace path"),
    ]);
    assert_eq!(
        launch_code,
        0,
        "real Codex launch failed:\n-- stdout --\n{launch_stdout}\n-- stderr --\n{launch_stderr}\n{}",
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
        "Codex process-bound launch",
        Duration::from_secs(40),
        || {
            let Some(instance) = h.instance_json(&name)? else {
                return Ok(None);
            };
            if instance
                .get("process_bound")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                Ok(Some(()))
            } else {
                Ok(None)
            }
        },
    );
    h.eventually("Codex PTY inject endpoint", Duration::from_secs(40), || {
        let (code, stdout, _stderr) = h.run(["term", &name, "--json"]);
        if code == 0 && stdout.contains("\"ready\":true") {
            Ok(Some(()))
        } else {
            Ok(None)
        }
    });

    // Drive the gated turn. The Codex TUI can drop the first keystroke right
    // after launch, so retry until the request actually reaches the mock — the
    // command is gated, so "accepted" is the turn arriving, never a proof file.
    let saw_gated_turn = || {
        mock.requests()
            .iter()
            .any(|body| body.contains(&token) && !body.contains("function_call_output"))
    };
    inject_prompt_until(
        &h,
        &name,
        &format!("Run the gated approval command {token}"),
        "gated approval prompt",
        saw_gated_turn,
        saw_gated_turn,
    );

    // The command must stay un-run while approval is pending.
    assert!(
        !approval_result.exists(),
        "gated command ran before approval was granted"
    );

    // An idle Codex sitting at an approval prompt stays `active`; the block only
    // latches once a message is queued behind the prompt. Send one and prove the
    // delivery loop flips the instance to blocked(pty:approval).
    let (send_code, send_stdout, send_stderr) = h.run_as_process(
        &sender_process_id,
        [
            "send",
            &format!("@{name}"),
            "--intent",
            "request",
            "--",
            &message_token,
        ],
    );
    assert_eq!(
        send_code, 0,
        "held-message send failed: stdout={send_stdout} stderr={send_stderr}"
    );

    // hcom must flip the instance to blocked(pty:approval) once the held message
    // is queued behind the visible prompt — driven by the terminal scrape, not
    // Codex's (absent) OSC9 notification.
    h.eventually(
        "blocked(pty:approval) to latch from the terminal-scraped approval prompt",
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
                        && event["data"]["context"].as_str() == Some("pty:approval")
                });
            if blocked { Ok(Some(())) } else { Ok(None) }
        },
    );
    // While blocked, the held message must not have been delivered yet and the
    // command must still be gated.
    let blocked_instance = h
        .instance_json(&name)
        .expect("list Codex while blocked")
        .expect("Codex active while blocked");
    assert_eq!(
        blocked_instance["unread_count"].as_u64(),
        Some(1),
        "held message must stay queued while the approval gate is up: {blocked_instance}"
    );
    assert!(
        !approval_result.exists(),
        "gated command ran while still blocked on approval"
    );

    // Approve. Codex's approval prompt defaults to the affirmative option, so a
    // bare Enter accepts it; this runs the command and releases the held message.
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
            Ok(_) => Ok(None),
            Err(_) => Ok(None),
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
            if stdout.contains("APPROVAL_PROOF") {
                Ok(Some(stdout))
            } else {
                Ok(None)
            }
        },
    );
    assert!(
        approval_transcript.contains(&token),
        "approval proof token missing from transcript: {approval_transcript}"
    );

    // Approval clears the PTY block and the held message finally delivers.
    h.eventually(
        "approval gate cleared after the keystroke",
        Duration::from_secs(40),
        || {
            let (code, stdout, stderr) = h.run([
                "events", "--agent", &name, "--type", "status", "--last", "50",
            ]);
            if code != 0 {
                return Err(format!("status event lookup failed: {stderr}"));
            }
            let cleared = stdout
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|event| event["data"]["context"].as_str() == Some("pty:approval_cleared"));
            if cleared { Ok(Some(())) } else { Ok(None) }
        },
    );
    h.eventually(
        "held message delivered after approval",
        Duration::from_secs(40),
        || {
            let Some(instance) = h.instance_json(&name)? else {
                return Ok(None);
            };
            if instance["unread_count"].as_u64() == Some(0) {
                Ok(Some(()))
            } else {
                Ok(None)
            }
        },
    );
    h.eventually(
        "held message body reached the Codex model",
        Duration::from_secs(40),
        || {
            let saw = mock.requests().iter().any(|body| {
                body.contains(&message_token) && body.contains("<hcom>") && body.contains(&sender)
            });
            if saw { Ok(Some(())) } else { Ok(None) }
        },
    );

    let unexpected = mock.unexpected();
    assert!(
        unexpected.is_empty(),
        "mock received {} unexpected request(s); first body:\n{}",
        unexpected.len(),
        unexpected.first().map(String::as_str).unwrap_or("")
    );
    let transport_errors = mock.transport_errors();
    assert!(
        transport_errors.is_empty(),
        "mock hit {} transport error(s):\n  {}",
        transport_errors.len(),
        transport_errors.join("\n  ")
    );
    for request in mock.requests() {
        serde_json::from_str::<Value>(&request).unwrap_or_else(|error| {
            panic!("mock received invalid request JSON: {error}\n{request}")
        });
    }
}
