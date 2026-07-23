//! Hermetic CLI smoke tests: invoke the `hcom` binary in a temp HCOM_DIR and
//! assert exit codes + stdout shape.

mod support;

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};
use support::{Hcom, parse_hcom_marker};

#[test]
fn fixture_drop_terminates_registered_process_group() {
    #[cfg(unix)]
    let mut child = Command::new("sh")
        .args(["-c", "sleep 60"])
        .process_group(0)
        .spawn()
        .expect("spawn cleanup test process group");
    #[cfg(windows)]
    let mut child = Command::new("powershell")
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"])
        .spawn()
        .expect("spawn cleanup test process group");
    let pid = i64::from(child.id());

    let h = Hcom::new();
    h.track_cleanup_pid(pid);
    assert!(
        h.process_group_alive(pid),
        "cleanup test process group did not start"
    );

    let reaper = std::thread::spawn(move || child.wait().expect("reap cleanup test process"));
    drop(h);
    let status = reaper.join().expect("cleanup reaper thread");
    assert!(
        !status.success(),
        "fixture cleanup should terminate the registered process"
    );

    let deadline = Instant::now() + Duration::from_secs(7);
    while Instant::now() < deadline && support::process_group_alive(pid) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !support::process_group_alive(pid),
        "fixture drop left process group {pid} alive"
    );
}

#[test]
fn help_prints_and_exits_zero() {
    let h = Hcom::new();
    let (code, stdout, _stderr) = h.run(["--help"]);
    assert_eq!(code, 0, "stdout={stdout}");
    assert!(stdout.contains("hcom"), "stdout={stdout}");
    assert!(
        stdout.contains("Commands:") || stdout.contains("Launch:"),
        "stdout={stdout}"
    );
}

#[test]
fn send_help_documents_broadcast_flag() {
    // Regression guard: `hcom send --help` must describe the current
    // broadcast mechanism (explicit --broadcast required from inside an AI
    // tool session), not the pre-fix "send -- message text" / "-- Broadcast
    // message to everyone" wording that no longer matches send.rs behavior.
    let h = Hcom::new();
    let (code, stdout, stderr) = h.run(["send", "--help"]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    assert!(stdout.contains("--broadcast"), "stdout={stdout}");
    assert!(
        !stdout.contains("send -- message text"),
        "stale no-flag broadcast usage still present: stdout={stdout}"
    );
    assert!(
        !stdout.contains("hcom send -- Broadcast message to everyone"),
        "stale broadcast example still present: stdout={stdout}"
    );
}

#[test]
fn status_json_in_fresh_dir() {
    let h = Hcom::new();
    let (code, stdout, _stderr) = h.run(["status", "--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("status json: {e}\n{stdout}"));
    assert_eq!(v["hcom_dir"].as_str(), Some(h.path().to_str().unwrap()));
    assert_eq!(v["instances"]["total"], 0);
}

#[test]
fn list_json_empty() {
    let h = Hcom::new();
    let (code, stdout, _stderr) = h.run(["list", "--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("list json");
    let arr = v.as_array().expect("list returns array");
    assert!(arr.is_empty(), "expected empty list, got {stdout}");
}

#[test]
fn events_empty_in_fresh_dir() {
    let h = Hcom::new();
    let (code, stdout, _stderr) = h.run(["events", "--last", "5"]);
    assert_eq!(code, 0);
    assert!(stdout.trim().is_empty(), "expected no events, got {stdout}");
}

#[test]
fn send_without_identity_errors_with_hint() {
    let h = Hcom::new();
    let (code, _stdout, stderr) = h.run(["send", "@nobody", "--", "hi"]);
    assert_ne!(code, 0, "send without identity must fail: stderr={stderr}");
    assert!(
        stderr.contains("identity not found"),
        "expected stable hint, got: {stderr}"
    );
}

#[test]
fn send_to_missing_agent_lists_available() {
    let h = Hcom::new();
    let me = h.start();

    let (code, _stdout, stderr) = h.run(["send", "@nope", "--name", &me, "--", "hi"]);
    assert_ne!(code, 0, "send to nonexistent must fail");
    assert!(
        stderr.contains("@nope") && stderr.contains("Available:"),
        "stderr={stderr}"
    );
}

#[test]
fn send_strips_redundant_trailing_name_from_auto_resolved_sender() {
    let h = Hcom::new();
    let recipient = h.start();
    let process_id = "send-trailing-name-process";

    let mut start = h.cmd();
    start.env("HCOM_PROCESS_ID", process_id).arg("start");
    let start_out = start.output().expect("spawn hcom start");
    let start_stdout = String::from_utf8_lossy(&start_out.stdout);
    let start_stderr = String::from_utf8_lossy(&start_out.stderr);
    assert!(
        start_out.status.success(),
        "stdout={start_stdout} stderr={start_stderr}"
    );
    let sender = parse_hcom_marker(&start_stdout).expect("sender marker");

    let mut send = h.cmd();
    send.env("HCOM_PROCESS_ID", process_id).args([
        "send",
        &format!("@{recipient}"),
        "--",
        "ack",
        "--name",
        &sender,
    ]);
    let send_out = send.output().expect("spawn hcom send");
    let send_stdout = String::from_utf8_lossy(&send_out.stdout);
    let send_stderr = String::from_utf8_lossy(&send_out.stderr);
    assert!(
        send_out.status.success(),
        "stdout={send_stdout} stderr={send_stderr}"
    );

    let (_, events, _) = h.run(["events", "--type", "message", "--last", "1"]);
    assert!(events.contains(r#""text":"ack""#), "events={events}");
    assert!(!events.contains("--name"), "events={events}");
}

#[test]
fn ai_tool_broadcast_requires_explicit_broadcast_flag() {
    let h = Hcom::new();
    let sender = h.start();
    // Keep the recipient count below the old >3 preview threshold so this
    // regression proves small accidental broadcasts are gated too.
    for _ in 0..2 {
        h.start();
    }

    let mut cmd = h.cmd();
    cmd.env("CODEX_SANDBOX", "1").args([
        "send",
        "--name",
        &sender,
        "--",
        "probably meant one person",
    ]);
    let out = cmd.output().expect("spawn hcom send");
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(
        code, 0,
        "send should preview first: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("BROADCAST SEND PREVIEW")
            && stdout.contains("broadcast to 2 agents")
            && stdout.contains("Did you mean to send this to everyone?")
            && stdout.contains("hcom send --broadcast"),
        "stdout={stdout}"
    );

    let (_, events_out, _) = h.run(["events", "--type", "message", "--last", "5"]);
    assert!(
        events_out.trim().is_empty(),
        "preview must not send a message: events={events_out}"
    );

    // --go must no longer authorize a broadcast (regression guard: accidental
    // no-@target sends with a habitual --go used to wake every agent).
    let mut go_cmd = h.cmd();
    go_cmd.env("CODEX_SANDBOX", "1").args([
        "--go",
        "send",
        "--name",
        &sender,
        "--",
        "go does not mean everyone",
    ]);
    let go_out = go_cmd.output().expect("spawn hcom --go send");
    let go_code = go_out.status.code().unwrap_or(-1);
    let go_stdout = String::from_utf8_lossy(&go_out.stdout);
    assert_ne!(go_code, 0, "--go must still preview: stdout={go_stdout}");
    assert!(
        go_stdout.contains("BROADCAST SEND PREVIEW"),
        "stdout={go_stdout}"
    );

    let mut bc_cmd = h.cmd();
    bc_cmd.env("CODEX_SANDBOX", "1").args([
        "send",
        "--broadcast",
        "--name",
        &sender,
        "--",
        "confirmed broadcast",
    ]);
    let bc_out = bc_cmd.output().expect("spawn hcom send --broadcast");
    let bc_code = bc_out.status.code().unwrap_or(-1);
    let bc_stdout = String::from_utf8_lossy(&bc_out.stdout);
    let bc_stderr = String::from_utf8_lossy(&bc_out.stderr);
    assert_eq!(bc_code, 0, "stdout={bc_stdout} stderr={bc_stderr}");
    assert!(
        bc_stdout.contains("Sent to:") || bc_stdout.contains("Sent to 2 agents"),
        "stdout={bc_stdout}"
    );
}

#[test]
fn start_send_events_roundtrip() {
    let h = Hcom::new();
    let sender = h.start();
    let recipient = h.start();
    assert_ne!(sender, recipient, "two starts must assign distinct names");

    let (c, stdout, stderr) = h.run([
        "send",
        &format!("@{recipient}"),
        "--name",
        &sender,
        "--",
        "hello there",
    ]);
    assert_eq!(c, 0, "stderr={stderr} stdout={stdout}");

    let (c4, events_out, _) = h.run(["events", "--last", "10"]);
    assert_eq!(c4, 0);
    let message_lines: Vec<_> = events_out
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["type"] == "message")
        .collect();
    assert_eq!(message_lines.len(), 1, "events={events_out}");
    let msg = &message_lines[0];
    assert_eq!(msg["instance"], sender.as_str(), "attribution = sender");
    assert_eq!(msg["data"]["from"], sender.as_str());
    assert_eq!(msg["data"]["text"], "hello there");

    // Recipient/scope contract: the message event only carries from/text, so
    // we check routing via per-instance unread on `list --json`. Recipient
    // must show unread=1, sender unread=0.
    let (c5, list_out, _) = h.run(["list", "--json"]);
    assert_eq!(c5, 0);
    let list: serde_json::Value = serde_json::from_str(&list_out).expect("list json");
    let by_name: std::collections::HashMap<_, _> = list
        .as_array()
        .expect("array")
        .iter()
        .map(|v| {
            (
                v["name"].as_str().unwrap().to_string(),
                v["unread_count"].as_u64().unwrap_or(0),
            )
        })
        .collect();
    assert_eq!(
        by_name.get(&recipient).copied(),
        Some(1),
        "recipient unread; list={list_out}"
    );
    assert_eq!(
        by_name.get(&sender).copied(),
        Some(0),
        "sender unread; list={list_out}"
    );

    let (c6, listen_out, listen_err) =
        h.run(["listen", "--name", &recipient, "--timeout", "1", "--json"]);
    assert_eq!(c6, 0, "listen failed: stderr={listen_err}");
    let delivered: Vec<serde_json::Value> = listen_out
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert_eq!(delivered.len(), 1, "listen output={listen_out}");
    assert_eq!(delivered[0]["from"], sender.as_str());
    assert_eq!(delivered[0]["text"], "hello there");

    let (c7, list_after_listen_out, _) = h.run(["list", "--json"]);
    assert_eq!(c7, 0);
    let list_after_listen: serde_json::Value =
        serde_json::from_str(&list_after_listen_out).expect("list json after listen");
    let after_by_name: std::collections::HashMap<_, _> = list_after_listen
        .as_array()
        .expect("array")
        .iter()
        .map(|v| {
            (
                v["name"].as_str().unwrap().to_string(),
                v["unread_count"].as_u64().unwrap_or(0),
            )
        })
        .collect();
    assert_eq!(
        after_by_name.get(&recipient).copied(),
        Some(0),
        "listen should advance recipient cursor; list={list_after_listen_out}"
    );
}

#[test]
fn intent_and_reply_to_roundtrip() {
    // Wiki contract (messaging.md §Intent + event-model.md `msg_intent`/`reply_to_local`):
    // request → ack with --reply-to flattens through `events_v` so threads/replies
    // can be traced. Locks: data.intent on send, and data.reply_to_local resolved
    // from the parent event id.
    let h = Hcom::new();
    let a = h.start();
    let b = h.start();

    let (c, _, e) = h.run([
        "send",
        &format!("@{b}"),
        "--name",
        &a,
        "--intent",
        "request",
        "--",
        "ping",
    ]);
    assert_eq!(c, 0, "request send failed: stderr={e}");

    let (_, req_out, _) = h.run(["events", "--type", "message", "--from", &a, "--last", "5"]);
    let req: serde_json::Value = req_out
        .lines()
        .find_map(|l| serde_json::from_str(l).ok())
        .expect("request event present");
    assert_eq!(req["data"]["intent"], "request");
    let req_id = req["id"].as_i64().expect("event id is i64");

    let (c2, _, e2) = h.run([
        "send",
        &format!("@{a}"),
        "--name",
        &b,
        "--intent",
        "ack",
        "--reply-to",
        &req_id.to_string(),
        "--",
        "pong",
    ]);
    assert_eq!(c2, 0, "ack send failed: stderr={e2}");

    let (_, ack_out, _) = h.run(["events", "--intent", "ack", "--last", "5"]);
    let ack: serde_json::Value = ack_out
        .lines()
        .find_map(|l| serde_json::from_str(l).ok())
        .expect("ack event present");
    assert_eq!(ack["data"]["intent"], "ack");
    assert_eq!(ack["data"]["from"], b.as_str());
    assert_eq!(
        ack["data"]["reply_to_local"].as_i64(),
        Some(req_id),
        "reply_to_local must resolve to request event id; ack={ack}"
    );
}

#[test]
fn lifecycle_events_emitted_for_start_and_stop() {
    // Wiki contract (agent-lifecycle.md + event-model.md): start emits
    // life.started, stop emits life.stopped — filterable via --action.
    // events table is the lifecycle source of truth (see
    // feedback_events_are_source_of_truth memory).
    let h = Hcom::new();
    let a = h.start();

    let (c, started_out, _) = h.run([
        "events", "--action", "started", "--agent", &a, "--last", "5",
    ]);
    assert_eq!(c, 0);
    let started: Vec<serde_json::Value> = started_out
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert_eq!(
        started.len(),
        1,
        "expected 1 life.started for {a}, got: {started_out}"
    );
    assert_eq!(started[0]["instance"], a.as_str());
    assert_eq!(started[0]["data"]["action"], "started");

    let (cs, _, es) = h.run(["stop", &a]);
    assert_eq!(cs, 0, "stop failed: {es}");

    let (_, stopped_out, _) = h.run([
        "events", "--action", "stopped", "--agent", &a, "--last", "5",
    ]);
    let stopped: Vec<serde_json::Value> = stopped_out
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert_eq!(
        stopped.len(),
        1,
        "expected 1 life.stopped for {a}, got: {stopped_out}"
    );
    assert_eq!(stopped[0]["data"]["action"], "stopped");
    // Snapshot lives on the event but is streamlined out by default;
    // --full surfaces it. Rebind relies on it (see start_as_reclaims_stopped_identity).
    let (_, full_out, _) = h.run([
        "events", "--action", "stopped", "--agent", &a, "--last", "5", "--full",
    ]);
    let full: serde_json::Value = full_out
        .lines()
        .find_map(|l| serde_json::from_str(l).ok())
        .expect("stopped event under --full");
    assert!(
        full["data"]["snapshot"].is_object(),
        "stop must preserve snapshot for rebind; full={full_out}"
    );
}

#[test]
fn start_as_reclaims_stopped_identity() {
    // Wiki contract (identity.md §--as + hcom-start.md Path B): after stop,
    // `start --as <name>` rebinds the same name (no random reallocation).
    // Distinct from bare `start`, which would draw a fresh name.
    let h = Hcom::new();
    let a = h.start();

    let (cs, _, es) = h.run(["stop", &a]);
    assert_eq!(cs, 0, "stop failed: {es}");

    let (cr, stdout, stderr) = h.run(["start", "--as", &a]);
    assert_eq!(cr, 0, "start --as failed: stderr={stderr}");
    assert!(
        stdout.contains(&format!("[hcom:{a}]")),
        "reclaim marker missing; stdout={stdout}"
    );

    // Reclaimed instance is alive again under the same name.
    // (Reclaim is a quiet rebind: no new life.started event, just a logged
    // rebind.complete. The marker + a re-populated instances row is the
    // observable contract.)
    let (_, names_out, _) = h.run(["list", "--names"]);
    assert!(
        names_out.lines().any(|l| l.trim() == a),
        "list --names missing {a} after reclaim: {names_out}"
    );

    // And the stopped snapshot must still be on record — that's what made
    // the cursor-preserving rebind possible.
    let (_, full_out, _) = h.run([
        "events", "--action", "stopped", "--agent", &a, "--last", "5", "--full",
    ]);
    let snap_present = full_out
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|v| v["data"]["snapshot"].is_object());
    assert!(snap_present, "stopped snapshot missing; full={full_out}");
}

#[test]
fn bigboss_send_bypasses_identity_gate() {
    // Wiki contract (messaging.md §@bigboss + reference_send_bigboss_flag memory):
    // `send -b` is sender-as-bigboss and bypasses the identity gate that
    // normally requires `--name` / a bound session. Sender_kind=external
    // distinguishes the message from instance-to-instance traffic.
    let h = Hcom::new();
    let recipient = h.start();

    // Note: no --name. -b is the sole identity signal.
    let (c, _, stderr) = h.run(["send", "-b", &format!("@{recipient}"), "--", "from above"]);
    assert_eq!(c, 0, "send -b must bypass identity gate; stderr={stderr}");
    assert!(
        !stderr.contains("identity not found"),
        "gate should not fire under -b; stderr={stderr}"
    );

    // --full bypasses streamlining so sender_kind is visible.
    let (_, events_out, _) = h.run([
        "events", "--type", "message", "--from", "bigboss", "--last", "5", "--full",
    ]);
    let msg: serde_json::Value = events_out
        .lines()
        .find_map(|l| serde_json::from_str(l).ok())
        .expect("bigboss message event");
    assert_eq!(msg["data"]["from"], "bigboss");
    assert_eq!(msg["data"]["text"], "from above");
    assert_eq!(
        msg["data"]["sender_kind"], "external",
        "bigboss must record as external sender; msg={msg}"
    );
}

#[test]
fn config_unknown_key_is_not_set() {
    let h = Hcom::new();
    let (code, stdout, _stderr) = h.run(["config", "no_such_key"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("(not set)"), "stdout={stdout}");
}

#[test]
fn unknown_command_errors() {
    let h = Hcom::new();
    let (code, _stdout, stderr) = h.run(["nonsense-not-a-command"]);
    assert_ne!(code, 0);
    assert!(!stderr.is_empty(), "expected error message on stderr");
}

#[test]
fn antigravity_e2e_hook_dispatch() {
    let h = Hcom::new();
    let transcript = tempfile::NamedTempFile::new().expect("temp transcript");
    let transcript_path = transcript.path().to_string_lossy().to_string();

    // Spawn hcom start with HCOM_PROCESS_ID to register a process binding
    let mut start_cmd = h.cmd();
    start_cmd.arg("start");
    start_cmd.env("HCOM_PROCESS_ID", "pid-agy-123");
    let start_out = start_cmd.output().expect("failed to run hcom start");
    let me = support::parse_hcom_marker(&String::from_utf8_lossy(&start_out.stdout))
        .expect("no [hcom:NAME] marker");

    // 1. Pipe PreInvocation (session start) to gemini-sessionstart.
    // This will bind the session_id "sess-agy-1" to the active instance.
    let session_start_payload = serde_json::json!({
        "conversationId": "sess-agy-1",
        "transcriptPath": transcript_path,
    });

    use std::io::Write;
    use std::process::Stdio;

    let mut cmd = h.cmd();
    cmd.args(["gemini-sessionstart"]);
    cmd.env("ANTIGRAVITY_AGENT", "1");
    cmd.env("HCOM_PROCESS_ID", "pid-agy-123");
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn hcom sessionstart");
    {
        let mut stdin = child.stdin.take().expect("failed to open stdin");
        stdin
            .write_all(
                serde_json::to_string(&session_start_payload)
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
    }
    let out = child
        .wait_with_output()
        .expect("failed to wait sessionstart");
    assert_eq!(
        out.status.code().unwrap_or(-1),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Verify session_id binding matches in the DB via hcom list --json
    let (code, stdout, stderr) = h.run(["list", &me, "--json"]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("failed to parse list json");
    assert_eq!(v["session_id"].as_str(), Some("sess-agy-1"));

    // 2. Now pipe PreToolUse to gemini-beforetool.
    // Since the session is bound, it should resolve the instance and execute successfully.
    let before_tool_payload = serde_json::json!({
        "conversationId": "sess-agy-1",
        "transcriptPath": transcript_path,
        "toolCall": {
            "name": "run_command",
            "args": { "CommandLine": "echo hello", "Cwd": "/tmp" }
        }
    });

    let mut cmd = h.cmd();
    cmd.args(["gemini-beforetool"]);
    cmd.env("ANTIGRAVITY_AGENT", "1");
    cmd.env("HCOM_PROCESS_ID", "pid-agy-123");
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn hcom beforetool");
    {
        let mut stdin = child.stdin.take().expect("failed to open stdin");
        stdin
            .write_all(
                serde_json::to_string(&before_tool_payload)
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
    }
    let out = child.wait_with_output().expect("failed to wait beforetool");
    assert_eq!(
        out.status.code().unwrap_or(-1),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("beforetool json");
    assert_eq!(parsed, serde_json::json!({ "decision": "allow" }));

    // 3. AfterTool cannot inject context for Antigravity, so it must not ack delivery.
    let (send_code, _, send_stderr) = h.run([
        "send",
        &format!("@{me}"),
        "--name",
        &me,
        "--intent",
        "request",
        "--",
        "ping",
    ]);
    assert_eq!(send_code, 0, "send stderr={send_stderr}");

    let after_tool_payload = serde_json::json!({
        "conversationId": "sess-agy-1",
        "transcriptPath": transcript_path,
        "toolCall": {
            "name": "run_command",
            "args": { "CommandLine": "echo done", "Cwd": "/tmp" }
        }
    });

    let mut cmd = h.cmd();
    cmd.args(["gemini-aftertool"]);
    cmd.env("ANTIGRAVITY_AGENT", "1");
    cmd.env("HCOM_PROCESS_ID", "pid-agy-123");
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn hcom aftertool");
    {
        let mut stdin = child.stdin.take().expect("failed to open stdin");
        stdin
            .write_all(
                serde_json::to_string(&after_tool_payload)
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
    }
    let out = child.wait_with_output().expect("failed to wait aftertool");
    assert_eq!(
        out.status.code().unwrap_or(-1),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after_stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(after_stdout.trim()).expect("aftertool json");
    assert_eq!(parsed, serde_json::json!({}));
}

/// Pipe a JSON payload to a native cursor hook and return its parsed stdout.
///
/// Cursor hook command names route directly to `Tool::Cursor` (no shared-prefix
/// disambiguation like Antigravity's `ANTIGRAVITY_AGENT`), so the only env the
/// gate check needs is `HCOM_PROCESS_ID` to resolve the bound instance.
fn run_cursor_hook(
    h: &Hcom,
    hook: &str,
    process_id: &str,
    payload: &serde_json::Value,
) -> serde_json::Value {
    use std::io::Write;
    use std::process::Stdio;

    let mut cmd = h.cmd();
    cmd.args([hook]);
    cmd.env("HCOM_PROCESS_ID", process_id);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().unwrap_or_else(|e| panic!("spawn {hook}: {e}"));
    {
        let mut stdin = child.stdin.take().expect("open stdin");
        stdin
            .write_all(serde_json::to_string(payload).unwrap().as_bytes())
            .unwrap();
    }
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait {hook}: {e}"));
    assert_eq!(
        out.status.code().unwrap_or(-1),
        0,
        "{hook} stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("{hook} json: {e}\nstdout={stdout}"))
}

/// End-to-end cursor-agent native hook lifecycle over JSON-on-stdin.
///
/// Mirrors `antigravity_e2e_hook_dispatch`, but exercises cursor's real payload
/// shape (`conversation_id`/`tool_input`/`tool_output`) and its distinct
/// delivery contract: unlike Antigravity (whose aftertool cannot inject and
/// must return `{}`), cursor's `postToolUse` injects pending messages via
/// `additional_context` and acks delivery.
#[test]
fn cursor_e2e_hook_dispatch() {
    let h = Hcom::new();
    let transcript = tempfile::NamedTempFile::new().expect("temp transcript");
    let transcript_path = transcript.path().to_string_lossy().to_string();
    let pid = "pid-cur-123";
    let session_id = "sess-cur-1";

    // Register a process binding so the hooks can resolve an instance.
    let mut start_cmd = h.cmd();
    start_cmd.arg("start");
    start_cmd.env("HCOM_PROCESS_ID", pid);
    let start_out = start_cmd.output().expect("failed to run hcom start");
    let me = support::parse_hcom_marker(&String::from_utf8_lossy(&start_out.stdout))
        .expect("no [hcom:NAME] marker");

    // 1. sessionStart binds the conversation to the active instance. Cursor reads
    //    the id from `conversation_id` (snake_case, per the docs' common schema)
    //    and the handler always returns an `env` object.
    let session_start = run_cursor_hook(
        &h,
        "cursor-sessionstart",
        pid,
        &serde_json::json!({
            "conversation_id": session_id,
            "transcript_path": transcript_path,
            "workspace_roots": ["/tmp"],
            "is_background_agent": false,
            "composer_mode": "agent",
        }),
    );
    assert!(
        session_start.get("env").is_some(),
        "sessionStart should emit env block: {session_start}"
    );

    // Binding is visible via list --json.
    let (code, stdout, stderr) = h.run(["list", &me, "--json"]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("list json");
    assert_eq!(v["session_id"].as_str(), Some(session_id));

    // 2. beforeSubmitPrompt marks the instance active and must not block the
    //    prompt (`continue: true`).
    let before_submit = run_cursor_hook(
        &h,
        "cursor-beforesubmitprompt",
        pid,
        &serde_json::json!({
            "conversation_id": session_id,
            "transcript_path": transcript_path,
            "prompt": "do a thing",
        }),
    );
    assert_eq!(before_submit, serde_json::json!({ "continue": true }));

    let (code, stdout, _) = h.run(["list", &me, "--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("list json");
    assert_eq!(v["status"].as_str(), Some("active"));

    // 3. preToolUse records tool status and returns an empty object.
    let pre_tool = run_cursor_hook(
        &h,
        "cursor-pretooluse",
        pid,
        &serde_json::json!({
            "conversation_id": session_id,
            "transcript_path": transcript_path,
            "tool_name": "Shell",
            "tool_input": { "command": "echo hello", "working_directory": "/tmp" },
        }),
    );
    assert_eq!(pre_tool, serde_json::json!({}));

    // 4. Queue a message, then postToolUse delivers it via additional_context.
    //    Send from an external sender (bigboss), not `me`: the DB delivery
    //    filter (`should_deliver_to`) drops any message whose `from` equals the
    //    receiver, so a self-addressed send would never be pending and the
    //    postToolUse assertion below would pass vacuously.
    let (send_code, _, send_stderr) = h.run([
        "send",
        "--from",
        "bigboss",
        &format!("@{me}"),
        "--intent",
        "request",
        "--",
        "ping",
    ]);
    assert_eq!(send_code, 0, "send stderr={send_stderr}");

    let post_tool = run_cursor_hook(
        &h,
        "cursor-posttooluse",
        pid,
        &serde_json::json!({
            "conversation_id": session_id,
            "transcript_path": transcript_path,
            "tool_name": "Shell",
            "tool_input": { "command": "echo hello" },
            "tool_output": "{\"exitCode\":0,\"stdout\":\"hello\"}",
        }),
    );
    let injected = post_tool
        .get("additional_context")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("postToolUse should inject additional_context: {post_tool}"));
    assert!(
        injected.contains("ping"),
        "delivered context should carry the message text: {injected:?}"
    );
}

/// End-to-end GitHub Copilot CLI native hook lifecycle over JSON-on-stdin.
///
/// Mirrors `cursor_e2e_hook_dispatch` but exercises copilot's real payload shape
/// (`session_id`/`tool_name`/`tool_input`/`tool_result`, Claude-style `command`
/// hooks). Copilot's `SessionStart` returns `additionalContext`/`{}` (no `env`
/// block), `PostToolUse` injects pending messages via `additionalContext` and
/// acks delivery. Reuses `run_cursor_hook` — it is a generic "pipe JSON to a
/// native hook" runner, not cursor-specific.
#[test]
fn copilot_e2e_hook_dispatch() {
    let h = Hcom::new();
    let transcript = tempfile::NamedTempFile::new().expect("temp transcript");
    let transcript_path = transcript.path().to_string_lossy().to_string();
    let pid = "pid-cop-123";
    let session_id = "sess-cop-1";

    // Register a process binding so the hooks can resolve an instance.
    let mut start_cmd = h.cmd();
    start_cmd.arg("start");
    start_cmd.env("HCOM_PROCESS_ID", pid);
    let start_out = start_cmd.output().expect("failed to run hcom start");
    let me = support::parse_hcom_marker(&String::from_utf8_lossy(&start_out.stdout))
        .expect("no [hcom:NAME] marker");

    // 1. SessionStart binds the session to the active instance.
    let _ = run_cursor_hook(
        &h,
        "copilot-sessionstart",
        pid,
        &serde_json::json!({
            "session_id": session_id,
            "transcript_path": transcript_path,
            "cwd": "/tmp",
        }),
    );

    // Binding is visible via list --json.
    let (code, stdout, stderr) = h.run(["list", &me, "--json"]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("list json");
    assert_eq!(v["session_id"].as_str(), Some(session_id));

    // 2. UserPromptSubmit marks the instance active and returns an empty object.
    let prompt_submit = run_cursor_hook(
        &h,
        "copilot-userpromptsubmit",
        pid,
        &serde_json::json!({
            "session_id": session_id,
            "transcript_path": transcript_path,
            "prompt": "do a thing",
        }),
    );
    assert_eq!(prompt_submit, serde_json::json!({}));

    let (code, stdout, _) = h.run(["list", &me, "--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("list json");
    assert_eq!(v["status"].as_str(), Some("active"));

    // 3. PreToolUse records tool status and returns an empty object.
    let pre_tool = run_cursor_hook(
        &h,
        "copilot-pretooluse",
        pid,
        &serde_json::json!({
            "session_id": session_id,
            "transcript_path": transcript_path,
            "tool_name": "bash",
            "tool_input": { "command": "echo hello" },
        }),
    );
    assert_eq!(pre_tool, serde_json::json!({}));

    // 4. Queue a message from an external sender, then PostToolUse delivers it
    //    via additionalContext. (Self-addressed sends are dropped by the DB
    //    delivery filter, so the assertion below would pass vacuously.)
    let (send_code, _, send_stderr) = h.run([
        "send",
        "--from",
        "bigboss",
        &format!("@{me}"),
        "--intent",
        "request",
        "--",
        "ping",
    ]);
    assert_eq!(send_code, 0, "send stderr={send_stderr}");

    let post_tool = run_cursor_hook(
        &h,
        "copilot-posttooluse",
        pid,
        &serde_json::json!({
            "session_id": session_id,
            "transcript_path": transcript_path,
            "tool_name": "bash",
            "tool_input": { "command": "echo hello" },
            "tool_result": { "text_result_for_llm": "hello" },
        }),
    );
    let injected = post_tool
        .get("additionalContext")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("PostToolUse should inject additionalContext: {post_tool}"));
    assert!(
        injected.contains("ping"),
        "delivered context should carry the message text: {injected:?}"
    );
}

/// Pipe argv to a native argv-style hook and return its parsed stdout.
fn run_argv_hook(
    h: &Hcom,
    hook: &str,
    process_id: Option<&str>,
    args: &[&str],
) -> serde_json::Value {
    let mut cmd = h.cmd();
    cmd.arg(hook);
    cmd.args(args);
    if let Some(process_id) = process_id {
        cmd.env("HCOM_PROCESS_ID", process_id);
    }

    let out = cmd.output().unwrap_or_else(|e| panic!("spawn {hook}: {e}"));
    assert_eq!(
        out.status.code().unwrap_or(-1),
        0,
        "{hook} stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("{hook} json: {e}\nstdout={stdout}"))
}

/// End-to-end Pi argv hook lifecycle.
///
/// Pi's extension invokes hcom with argv, not JSON stdin. This test mirrors the
/// native hook smoke tests above while staying hermetic: no real Pi process is
/// launched, only a fake process binding plus the `pi-*` hook commands.
#[test]
fn pi_e2e_hook_dispatch() {
    let h = Hcom::new();
    let transcript = tempfile::NamedTempFile::new().expect("temp transcript");
    let transcript_path = transcript.path().to_string_lossy().to_string();
    let pid = "pid-pi-123";
    let session_id = "sess-pi-1";

    // Register a process binding so pi-start can resolve an instance.
    let mut start_cmd = h.cmd();
    start_cmd.arg("start");
    start_cmd.env("HCOM_PROCESS_ID", pid);
    let start_out = start_cmd.output().expect("failed to run hcom start");
    let me = support::parse_hcom_marker(&String::from_utf8_lossy(&start_out.stdout))
        .expect("no [hcom:NAME] marker");

    // 1. pi-start binds the session and returns bootstrap context to the plugin.
    let cwd = h.root.path().to_string_lossy().to_string();
    let start = run_argv_hook(
        &h,
        "pi-start",
        Some(pid),
        &[
            "--session-id",
            session_id,
            "--transcript-path",
            &transcript_path,
            "--cwd",
            &cwd,
        ],
    );
    assert_eq!(start["name"].as_str(), Some(me.as_str()));
    assert_eq!(start["session_id"].as_str(), Some(session_id));
    assert!(
        start["bootstrap"]
            .as_str()
            .is_some_and(|text| text.contains(&format!("[hcom:{me}]"))),
        "pi-start should return bootstrap with the hcom marker: {start}"
    );

    let (code, stdout, stderr) = h.run(["list", &me, "--json"]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("list json");
    assert_eq!(v["tool"].as_str(), Some("pi"));
    assert_eq!(v["session_id"].as_str(), Some(session_id));
    assert_eq!(
        v["transcript_path"].as_str(),
        Some(transcript_path.as_str())
    );
    assert_eq!(v["directory"].as_str(), Some(cwd.as_str()));

    // 2. pi-status marks active/listening transitions.
    let status = run_argv_hook(
        &h,
        "pi-status",
        None,
        &[
            "--name",
            &me,
            "--status",
            "active",
            "--context",
            "prompt",
            "--detail",
            "working",
        ],
    );
    assert_eq!(status, serde_json::json!({ "ok": true }));
    let (code, stdout, _) = h.run(["list", &me, "--json"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("list json");
    assert_eq!(v["status"].as_str(), Some("active"));

    // 3. pi-beforetool records tool status and allows the tool call.
    let before_tool = run_argv_hook(
        &h,
        "pi-beforetool",
        None,
        &[
            "--name",
            &me,
            "--tool",
            "bash",
            "--input-json",
            r#"{"command":"echo hello"}"#,
        ],
    );
    assert_eq!(before_tool, serde_json::json!({ "decision": "allow" }));
    let (code, stdout, _) = h.run(["events", "--agent", &me, "--type", "status", "--last", "5"]);
    assert_eq!(code, 0);
    let tool_status: serde_json::Value = stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .find(|event: &serde_json::Value| event["data"]["context"] == "tool:bash")
        .unwrap_or_else(|| panic!("tool:bash status event missing: {stdout}"));
    assert!(
        tool_status["data"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("echo hello")),
        "tool detail should include bash command: {tool_status}"
    );

    // 4. pi-read exposes pending messages and can ack the cursor.
    let (send_code, _, send_stderr) = h.run([
        "send",
        "--from",
        "bigboss",
        &format!("@{me}"),
        "--intent",
        "request",
        "--",
        "ping",
    ]);
    assert_eq!(send_code, 0, "send stderr={send_stderr}");

    let check = h.run(["pi-read", "--name", &me, "--check"]);
    assert_eq!(check.0, 0, "pi-read --check stderr={}", check.2);
    assert_eq!(check.1.trim(), "true");

    let read = h.run(["pi-read", "--name", &me]);
    assert_eq!(read.0, 0, "pi-read stderr={}", read.2);
    let messages: serde_json::Value = serde_json::from_str(&read.1).expect("pi-read json");
    assert!(
        messages
            .as_array()
            .is_some_and(|items| items.iter().any(|m| m["message"] == "ping")),
        "pi-read should return pending ping: {messages}"
    );

    let ack = run_argv_hook(&h, "pi-read", None, &["--name", &me, "--ack"]);
    assert_eq!(ack["acked"].as_u64(), Some(1));
    let check = h.run(["pi-read", "--name", &me, "--check"]);
    assert_eq!(check.0, 0, "pi-read --check after ack stderr={}", check.2);
    assert_eq!(check.1.trim(), "false");

    // 5. pi-stop finalizes the session.
    let stop = run_argv_hook(&h, "pi-stop", None, &["--name", &me, "--reason", "done"]);
    assert_eq!(stop, serde_json::json!({ "ok": true }));
    let (code, stdout, _) = h.run([
        "events", "--agent", &me, "--action", "stopped", "--last", "5",
    ]);
    assert_eq!(code, 0);
    let stopped: serde_json::Value = stdout
        .lines()
        .find_map(|line| serde_json::from_str(line).ok())
        .unwrap_or_else(|| panic!("stopped event missing: {stdout}"));
    assert_eq!(stopped["data"]["action"].as_str(), Some("stopped"));
}
