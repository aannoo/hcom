//! Pipe clients must retain their transport through hcom's native registration.
#![cfg(unix)]

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use support::Hcom;

fn assert_no_registered_instances(h: &Hcom) {
    // Inspect before `hcom list`, whose stale-process cleanup could otherwise
    // conceal a missing supervisor finalizer.
    let db = rusqlite::Connection::open(h.hcom_dir.join("hcom.db")).unwrap();
    let count: i64 = db
        .query_row("SELECT COUNT(*) FROM instances", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0, "exited native process left a registered instance");
}

#[test]
fn claude_stdio_keeps_clean_channels_and_native_exit() {
    let h = Hcom::new();
    let bin = h.root_path().join("pipe-tools");
    fs::create_dir(&bin).unwrap();
    let tool = bin.join("claude");
    fs::write(&tool, r#"#!/bin/sh
if [ "$1" = --version ]; then printf '2.1.309 (Claude Code)\n'; exit 0; fi
if [ -t 0 ] || [ -t 1 ]; then printf 'unexpected PTY\n'; exit 31; fi
printf '{"launched":"%s","tool":"%s","stdio":"%s","pty":"%s"}\n' "$HCOM_LAUNCHED" "$HCOM_TOOL" "$HCOM_STDIO_MODE" "$HCOM_PTY_MODE"
IFS= read -r line
printf '%s\n' "$line"
printf 'native diagnostic\n' >&2
exit 23
"#).unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    h.set_launch_env("PATH", &path);
    let mut child = h
        .cmd()
        .args([
            "claude",
            "--stdio",
            "--go",
            "--",
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"type\":\"user\"}\n");
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(23), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "{\"launched\":\"1\",\"tool\":\"claude\",\"stdio\":\"1\",\"pty\":\"0\"}\n{\"type\":\"user\"}\n"
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "native diagnostic\n"
    );
    assert!(
        !h.hcom_dir.join(".tmp/launch").exists(),
        "stdio must not serialize a shell runner"
    );
    assert_no_registered_instances(&h);
}

#[test]
fn claude_stdio_forwards_cancellation_and_waits_for_native_exit() {
    let h = Hcom::new();
    let bin = h.root_path().join("pipe-tools");
    fs::create_dir(&bin).unwrap();
    let tool = bin.join("claude");
    fs::write(
        &tool,
        r#"#!/bin/sh
if [ "$1" = --version ]; then printf '2.1.309 (Claude Code)\n'; exit 0; fi
trap 'printf "cancelled\n" >&2; exit 37' TERM
printf 'ready\n'
while IFS= read -r line; do :; done
exit 32
"#,
    )
    .unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    h.set_launch_env(
        "PATH",
        &format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
    );
    let mut child = h
        .cmd()
        .args(["claude", "--stdio", "--go", "--", "-p"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line, "ready\n");
    // Keep the SDK input channel open; cancellation must reach the native
    // process, not merely close its pipe as wait_with_output would.
    let _stdin = child.stdin.take().unwrap();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(37));
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "cancelled\n");
    assert_no_registered_instances(&h);
}

#[test]
fn stdio_rejects_incompatible_launches_without_stdout() {
    for args in [
        vec!["claude", "--stdio"],
        vec!["codex", "--stdio", "-p"],
        vec!["2", "claude", "--stdio", "-p"],
        vec!["claude", "--stdio", "--headless", "-p"],
        vec!["claude", "--stdio", "--json", "-p"],
        vec!["claude", "--stdio", "--device", "peer", "-p"],
        vec!["claude", "--stdio", "--terminal", "here", "-p"],
        vec!["claude", "--stdio", "--no-run-here", "-p"],
        vec!["r", "nova", "--stdio"],
    ] {
        let h = Hcom::new();
        let (code, stdout, stderr) = h.run(args.clone());
        assert_ne!(code, 0, "{args:?}");
        assert!(stdout.is_empty(), "{args:?}: {stdout}");
        assert!(stderr.contains("--stdio"), "{args:?}: {stderr}");
    }
}

/// The real native CLI speaks the same newline JSON control protocol used by
/// the Agent SDK. No hosted model is contacted: all turns use the local mock.
#[test]
#[ignore = "requires the real Claude CLI installed by scripts/install-mock-tools.sh"]
fn real_claude_stdio_two_turns_permission_channel_and_eof() {
    use serde_json::{Value, json};
    use std::sync::mpsc;
    use std::time::Duration;
    use support::claude_mock::{ClaudeCase, MODEL, claude_text, claude_tool_use, latest_user_turn};
    use support::mock_http::{MockHttp, Reply};
    use support::real_tool::ToolCase;

    let h = Hcom::new();
    let proof = h.workspace.join("approved.txt");
    // The deployed SDK build coexists with stock hcom. Its hooks must use the
    // exact build that understands stdio, irrespective of PATH resolution.
    let ordinary_bin = h.root_path().join("ordinary-bin");
    fs::create_dir(&ordinary_bin).unwrap();
    let ordinary_hcom = ordinary_bin.join("hcom");
    fs::write(
        &ordinary_hcom,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$SDK_TEST_OLD_CALLS_FILE\"\nexit 86\n",
    )
    .unwrap();
    fs::set_permissions(&ordinary_hcom, fs::Permissions::from_mode(0o755)).unwrap();
    let old_calls = h.root_path().join("old-hcom-calls");
    h.set_launch_env("SDK_TEST_OLD_CALLS_FILE", old_calls.to_str().unwrap());
    h.set_launch_env(
        "PATH",
        &format!(
            "{}:{}:{}",
            ordinary_bin.display(),
            support::pins::mock_tools_bin().display(),
            std::env::var("PATH").unwrap()
        ),
    );
    let command = format!("printf approved > '{}'", proof.display());
    let mock = MockHttp::start(move |request| {
        if request.method.eq_ignore_ascii_case("HEAD") {
            return Reply::Empty(200);
        }
        if request.path.contains("count_tokens") {
            return Reply::Json(json!({"input_tokens": 1}).to_string());
        }
        if !request.path.contains("/v1/messages") {
            return Reply::Status(404);
        }
        let (tool, text) = latest_user_turn(&request.body).unwrap_or_default();
        if tool.as_deref() == Some("toolu_stdio") {
            Reply::Sse(claude_text("msg_one", "STDIO_ONE_DONE"))
        } else if text.contains("STDIO_ONE") {
            Reply::Sse(claude_tool_use(
                "msg_permission",
                "toolu_stdio",
                "Bash",
                &json!({"command": command, "description": "write permission proof"}),
            ))
        } else if text.contains("STDIO_TWO") {
            Reply::Sse(claude_text("msg_two", "STDIO_TWO_DONE"))
        } else {
            Reply::Status(500)
        }
    })
    .unwrap();
    ClaudeCase.prepare(&h, &ClaudeCase.provider_base_url(mock.port()));
    h.set_launch_env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts");
    let mut child = h
        .cmd()
        .args([
            "claude",
            "--stdio",
            "--go",
            "--",
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-prompt-tool",
            "stdio",
            "--permission-mode",
            "manual",
            "--model",
            MODEL,
            "--setting-sources",
            "user",
            "--tools",
            "Bash",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let errors = std::thread::spawn(move || {
        use std::io::Read;
        let mut text = String::new();
        let mut reader = stderr;
        reader.read_to_string(&mut text).unwrap();
        text
    });
    let (send, receive) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if send.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let send_json = |input: &mut std::process::ChildStdin, value: Value| {
        writeln!(input, "{value}").unwrap();
        input.flush().unwrap();
    };
    send_json(
        &mut stdin,
        json!({"type": "control_request", "request_id": "init", "request": {"subtype": "initialize"}}),
    );
    let mut session_id = None;
    let mut permission_requests = 0;
    for turn in ["STDIO_ONE", "STDIO_TWO"] {
        send_json(
            &mut stdin,
            json!({"type": "user", "message": {"role": "user", "content": turn}, "parent_tool_use_id": null}),
        );
        loop {
            let line = receive
                .recv_timeout(Duration::from_secs(90))
                .expect("native SDK frame (turn must not wait in hcom Stop)");
            let frame: Value =
                serde_json::from_str(&line).expect("stdout contains only native JSON frames");
            if frame["type"] == "control_request" && frame["request"]["subtype"] == "can_use_tool" {
                permission_requests += 1;
                assert_eq!(frame["request"]["tool_name"], "Bash");
                send_json(
                    &mut stdin,
                    json!({"type": "control_response", "response": {
                        "subtype": "success", "request_id": frame["request_id"],
                        "response": {"behavior": "allow", "updatedInput": frame["request"]["input"]}
                    }}),
                );
            }
            if frame["type"] == "result" {
                assert_eq!(frame["is_error"], false, "{frame}");
                assert!(
                    frame["result"]
                        .as_str()
                        .unwrap()
                        .contains(&format!("{turn}_DONE")),
                    "{frame}"
                );
                let id = frame["session_id"].as_str().unwrap().to_string();
                if let Some(ref expected) = session_id {
                    assert_eq!(expected, &id);
                }
                session_id = Some(id);
                break;
            }
        }
        let rows = h.list_json().unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0]["session_id"].as_str(), session_id.as_deref());
        assert_eq!(rows[0]["status"], "listening", "{rows:?}");
    }
    assert_eq!(permission_requests, 1);
    assert_eq!(fs::read_to_string(proof).unwrap(), "approved");
    drop(stdin);
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
    let stderr = errors.join().unwrap();
    assert!(!stderr.contains("requires --print"), "{stderr}");
    assert_no_registered_instances(&h);
    let calls = fs::read_to_string(old_calls).unwrap_or_default();
    assert!(
        calls.lines().all(|line| line == "relay push"),
        "native hooks used ordinary hcom from PATH: {calls}"
    );
}
