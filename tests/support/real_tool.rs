//! Shared real-tool lifecycle runner.
//!
//! Every real CLI (Codex, Claude, ...) is driven through ONE lifecycle here, so
//! the test asserts a single tool-independent contract: launch → bind → file
//! tool → shell tool → `hcom send` → transcript → idle inbound delivery → live
//! fork → kill parent → resume → cleanup + request audit. Tools that meet the
//! contract by different mechanisms (Codex's native fork vs Claude's
//! `CLAUDE_ENV_FILE` session-UUID recovery) are NOT special-cased: the shared
//! fork assertion (`child.session_id != parent.session_id`, both alive, child
//! transcript ⊇ parent history + fork proof, parent ⊉ fork proof) is exactly
//! what proves either mechanism, and regression-guards Claude's workaround for
//! free. A per-tool fork assertion would be checking the mechanism instead of
//! the contract.
//!
//! The only per-tool code is the [`ToolCase`] seam: wire codec + latest-turn
//! classification ([`ToolCase::respond`]), version pin, launch args/env, config
//! routing, and a couple of capability strings. Everything else lives here.

use std::collections::HashSet;
use std::fs;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::mock_http::{MockHttp, RecordedRequest, Reply};
use super::{Hcom, parse_launch_names, unique_suffix};

/// Final assistant-text proofs every tool emits verbatim, so the runner can
/// assert them without knowing the wire format. The case builds each full line
/// as `"{PREFIX} {token}"`.
pub const INITIAL_PROOF: &str = "INITIAL_LIFECYCLE_COMPLETE";
pub const INBOUND_PROOF: &str = "INBOUND_PROOF";
pub const FORK_PROOF: &str = "FORK_PROOF";
pub const RESUME_PROOF: &str = "RESUME_PROOF";

/// Pin/identity metadata for the external oracle binary.
pub struct ToolMeta {
    /// hcom launch keyword and `tool` column value (`codex`, `claude`).
    pub tool: &'static str,
    /// Executable to version-check (`codex`, `claude`).
    pub binary: &'static str,
    /// Exact pinned version string the oracle must report.
    pub pinned_version: &'static str,
    /// Copy-pasteable install command shown when the pin is missing.
    pub install_command: &'static str,
}

/// Tokens, file paths, and the send command the scenario is built from. Owned
/// and cloneable so it can move into the mock responder (which runs on worker
/// threads, before launch, so it cannot borrow the fixture).
#[derive(Clone)]
pub struct ScenarioIds {
    pub initial: String,
    pub inbound: String,
    pub fork: String,
    pub resume: String,
    pub recipient: String,
    /// Absolute path the file tool must create with `initial` as its content.
    pub file_path: String,
    /// Workspace-relative form of `file_path` (Codex `apply_patch` Add File).
    pub file_rel: String,
    /// Absolute path the shell tool must create with `initial` as its content.
    pub shell_path: String,
    /// Workspace-relative form of `shell_path` for shell commands.
    pub shell_rel: String,
    /// Shell command that sends the one outgoing hcom message.
    pub send_cmd: String,
}

/// The per-tool seam: the only code that differs between tools.
pub trait ToolCase: Clone + Send + Sync + 'static {
    fn meta(&self) -> &ToolMeta;

    /// hcom status context for this tool's file edit (`tool:Write`,
    /// `tool:apply_patch`).
    fn file_context(&self) -> &'static str;

    /// Event `detail` the file edit surfaces — Codex stores the relative path,
    /// Claude the absolute one. Used for the file-edit event assertion.
    fn file_detail(&self, ids: &ScenarioIds) -> String;

    /// Provider base URL for the given mock port (Codex: `.../v1`; Claude: bare
    /// `http://127.0.0.1:PORT`).
    fn provider_base_url(&self, port: u16) -> String;

    /// Write config / set env so the launched tool routes to `base_url` and
    /// starts headless against the localhost mock.
    fn prepare(&self, h: &Hcom, base_url: &str);

    /// Tool arguments appended after `hcom <tool> --headless --dir <ws> --`.
    /// Empty for Codex (hcom supplies its flags); Claude needs model/permission/
    /// tool-restriction flags.
    fn launch_args(&self, h: &Hcom) -> Vec<String>;

    /// Whether a request body is a tool-result follow-up rather than a fresh
    /// user turn — used to count turns and detect prompt acceptance without
    /// matching stale history markers.
    fn is_followup_turn(&self, body: &str) -> bool;

    /// Whether a recorded request is a model-generation turn (vs an auxiliary
    /// route like Claude's `/v1/messages/count_tokens`, which carries the same
    /// payload and would otherwise inflate exactly-once turn counts). Codex has
    /// only one route, so it is always a turn.
    fn is_turn_request(&self, req: &RecordedRequest) -> bool;

    /// Static substrings the native inbound-delivery envelope must contain
    /// (beyond the sender name and message token the runner always checks).
    fn delivery_envelope_markers(&self) -> &'static [&'static str];

    /// Script one turn. Classify the latest turn (newest user message /
    /// tool_result), return the SSE/JSON reply, or `Reply::Status(500)` for an
    /// unexpected request so a mis-scripted turn fails loudly.
    fn respond(&self, req: &RecordedRequest, ids: &ScenarioIds) -> Reply;

    /// Drive any one-time startup gate hcom surfaces in the PTY as
    /// `launch_blocked` before the tool is ready — e.g. Claude's onboarding +
    /// workspace-trust prompts, which gate hook registration. Default no-op
    /// (Codex starts straight into its TUI). This deliberately exercises the
    /// real surfaced-prompt path rather than pre-seeding trust state.
    fn drive_startup(&self, _h: &Hcom, _name: &str) {}
}

fn has_exact_version(version_output: &str, expected: &str) -> bool {
    version_output
        .split_whitespace()
        .any(|token| token.trim_start_matches('v') == expected)
}

/// Panic with install instructions unless exactly the pinned oracle is present.
pub fn require_pinned<C: ToolCase>(h: &Hcom, case: &C) {
    let meta = case.meta();
    let version = match h.external_version(meta.binary) {
        Ok(version) => version,
        Err(reason) => panic!(
            "real {tool} integration test requires {binary} {version}: {reason}. Install with: {install}",
            tool = meta.tool,
            binary = meta.binary,
            version = meta.pinned_version,
            install = meta.install_command,
        ),
    };
    if !has_exact_version(&version, meta.pinned_version) {
        panic!(
            "real {tool} integration test requires {binary} {expected}, found `{version}`. \
             Install the pinned version with: {install}",
            tool = meta.tool,
            binary = meta.binary,
            expected = meta.pinned_version,
            install = meta.install_command,
        );
    }
}

/// Inject a prompt until the tool accepts it, then wait for the resulting proof.
///
/// `term --json` reports `ready:true` from the PTY ready pattern, but a TUI can
/// still drop a keystroke right after launch or a resume re-render. Once the
/// mock observes the prompt, never inject it again: the accepted turn may still
/// be running, and retrying could duplicate side effects.
pub fn inject_prompt_until(
    h: &Hcom,
    name: &str,
    prompt: &str,
    description: &str,
    mut accepted: impl FnMut() -> bool,
    mut proof: impl FnMut() -> bool,
) {
    let inject_deadline = Instant::now() + Duration::from_secs(40);
    let mut prompt_attempts = 0;
    while prompt_attempts < 5 && Instant::now() < inject_deadline {
        let (code, stdout, stderr) = h.run(["term", "inject", name, prompt, "--enter"]);
        if code != 0 {
            let retryable = stdout.contains("No inject port")
                || stdout.contains("No response from")
                || stderr.contains("No inject port")
                || stderr.contains("No response from");
            if retryable {
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
            assert_eq!(
                code, 0,
                "{description}: inject failed: stdout={stdout} stderr={stderr}"
            );
        }
        prompt_attempts += 1;
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if proof() {
                return;
            }
            if accepted() {
                let proof_deadline = Instant::now() + Duration::from_secs(40);
                let mut last_approval_drive = Instant::now() - Duration::from_secs(10);
                while Instant::now() < proof_deadline {
                    if proof() {
                        return;
                    }
                    if last_approval_drive.elapsed() >= Duration::from_secs(1)
                        && instance_status_context(h, name).as_deref() == Some("pty:approval")
                    {
                        let (code, stdout, stderr) = h.run(["term", "inject", name, "--enter"]);
                        assert_eq!(
                            code, 0,
                            "{description}: approval confirm failed: stdout={stdout} stderr={stderr}"
                        );
                        last_approval_drive = Instant::now();
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                panic!(
                    "{description}: tool accepted the prompt but the turn never produced proof\n{}",
                    h.diagnostics()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    panic!(
        "{description}: injected prompt never produced the expected turn\n{}",
        h.diagnostics()
    );
}

fn instance_status_context(h: &Hcom, name: &str) -> Option<String> {
    h.instance_json(name).ok().flatten().and_then(|instance| {
        instance
            .get("status_context")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    })
}

/// Wait until a tool instance is process-bound; return its canonical name.
///
/// 90s (not 40s): on Windows CI, npm-shimmed tools (codex in particular, whose
/// launcher bypasses the shim to invoke node directly, see
/// `terminal::resolve_windows_tool_launcher`) cold-start slower than on Unix
/// runners and have been observed landing just past a 40s deadline.
fn wait_process_bound<C: ToolCase>(h: &Hcom, case: &C, name: &str, what: &str) -> Value {
    h.eventually(what, Duration::from_secs(90), || {
        let Some(instance) = h.instance_json(name)? else {
            return Ok(None);
        };
        let bound = instance
            .get("process_bound")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if bound { Ok(Some(instance)) } else { Ok(None) }
    });
    // Re-read so callers always see the latest snapshot.
    let _ = case;
    h.instance_json(name)
        .expect("list bound instance")
        .expect("bound instance present")
}

fn wait_pty_ready(h: &Hcom, name: &str, what: &str) {
    h.eventually(what, Duration::from_secs(90), || {
        let (code, stdout, _stderr) = h.run(["term", name, "--json"]);
        // `ready` matches the tool's ready pattern (Codex), but tools whose
        // status bar hides that pattern at an idle prompt (Claude in
        // `bypassPermissions`, `require_ready_prompt=false`) report `ready:false`
        // while still accepting injection. `prompt_empty` is the cross-tool idle
        // signal — the inject endpoint exists once `term --json` exits 0.
        let injectable =
            stdout.contains("\"ready\":true") || stdout.contains("\"prompt_empty\":true");
        if code == 0 && injectable {
            Ok(Some(()))
        } else {
            Ok(None)
        }
    });
}

/// The full tool-independent lifecycle. One long serial test by design: fork
/// needs a live parent and resume needs a prior kill, so the phases share live
/// process state and cannot be split without re-launching the tool per phase.
pub fn run_full_lifecycle<C: ToolCase>(case: C) {
    let h = Hcom::new();
    require_pinned(&h, &case);
    let tool = case.meta().tool;

    let suffix = unique_suffix();
    let recipient_process_id = format!("hcom-{tool}-recipient-{suffix}");
    let recipient = h.start_listening_with_process_id(&recipient_process_id);

    let canonical_workspace =
        fs::canonicalize(&h.workspace).unwrap_or_else(|_| h.workspace.clone());
    // Windows canonicalize returns a `\\?\` verbatim path. That form is useful
    // for filesystem syscalls but Claude's Write tool rejects it as a file URL.
    // Model-visible paths must use the normal child-process representation.
    #[cfg(windows)]
    let canonical_workspace = {
        let value = canonical_workspace.to_string_lossy();
        std::path::PathBuf::from(value.strip_prefix(r"\\?\").unwrap_or(&value))
    };
    let file_path = canonical_workspace.join("lifecycle-file.txt");
    let shell_path = canonical_workspace.join("lifecycle-shell.txt");
    let ids = ScenarioIds {
        initial: format!("HCOM_{}_PHASE1_{suffix}", tool.to_uppercase()),
        inbound: format!("HCOM_{}_INBOUND_{suffix}", tool.to_uppercase()),
        fork: format!("HCOM_{}_FORK_{suffix}", tool.to_uppercase()),
        resume: format!("HCOM_{}_RESUME_{suffix}", tool.to_uppercase()),
        recipient: recipient.clone(),
        file_path: file_path.to_str().expect("UTF-8 file path").to_string(),
        file_rel: "lifecycle-file.txt".to_string(),
        shell_path: shell_path.to_str().expect("UTF-8 shell path").to_string(),
        shell_rel: "lifecycle-shell.txt".to_string(),
        send_cmd: String::new(), // filled below
    };
    let ids = ScenarioIds {
        // PowerShell treats a bare @name as splatting syntax, so quote the
        // hcom target on Windows. Unix shells accept the same target bare.
        send_cmd: format!(
            "{} send {} --intent inform -- {token}",
            if cfg!(windows) && tool == "claude" {
                h.bash_hcom_command()
            } else {
                h.shell_hcom_command()
            },
            if cfg!(windows) {
                format!("'@{recipient}'")
            } else {
                format!("@{recipient}")
            },
            token = ids.initial
        ),
        ..ids
    };

    // Content-addressed scenario: each turn is selected by what the latest turn
    // contains, never by arrival ordinal. The mock responder runs on worker
    // threads, so it owns a clone of the case + ids.
    let mock = {
        let case = case.clone();
        let ids = ids.clone();
        MockHttp::start(move |request: &RecordedRequest| case.respond(request, &ids))
            .expect("start localhost mock provider")
    };
    let base_url = case.provider_base_url(mock.port());
    case.prepare(&h, &base_url);

    // --- Phase 1: launch + bind ------------------------------------------------
    let mut launch_argv: Vec<String> = vec![
        tool.to_string(),
        "--headless".to_string(),
        "--dir".to_string(),
        h.workspace
            .to_str()
            .expect("UTF-8 workspace path")
            .to_string(),
    ];
    let tool_args = case.launch_args(&h);
    if !tool_args.is_empty() {
        launch_argv.push("--".to_string());
        launch_argv.extend(tool_args);
    }
    let (launch_code, launch_stdout, launch_stderr) = h.run(&launch_argv);
    assert_eq!(
        launch_code,
        0,
        "real {tool} launch failed:\n-- stdout --\n{launch_stdout}\n-- stderr --\n{launch_stderr}\n{}",
        h.diagnostics()
    );
    let launched_names = parse_launch_names(&launch_stdout);
    assert_eq!(
        launched_names.len(),
        1,
        "expected one launch placeholder name; stdout={launch_stdout}"
    );
    let name = launched_names[0].clone();

    wait_process_bound(&h, &case, &name, "process-bound launch");
    // Verify the placeholder's PID is tracked before hook/session binding
    // replaces it below (this early-window registration is a separate code
    // path from the later hook-bound PID, see `initial_pid` in Phase 2).
    h.eventually("tracked process id", Duration::from_secs(10), || {
        h.instance_pid(&name).map(|pid| pid.filter(|p| *p > 1))
    });
    // Clear any surfaced startup gate (Claude onboarding/trust) before readiness.
    case.drive_startup(&h, &name);
    wait_pty_ready(&h, &name, "PTY inject endpoint");

    // --- Phase 2: first turn (file tool -> shell tool -> hcom send -> proof) ---
    {
        let mock = &mock;
        let case = &case;
        let ids = &ids;
        let file_path = file_path.clone();
        let shell_path = shell_path.clone();
        inject_prompt_until(
            &h,
            &name,
            &format!(
                "Execute the deterministic phase-one lifecycle {}",
                ids.initial
            ),
            "initial lifecycle prompt",
            || {
                mock.request_bodies()
                    .iter()
                    .any(|body| body.contains(&ids.initial) && !case.is_followup_turn(body))
            },
            || {
                matches!(fs::read_to_string(&file_path), Ok(c) if c.trim() == ids.initial)
                    && matches!(fs::read_to_string(&shell_path), Ok(c) if c.trim() == ids.initial)
            },
        );
    }
    assert_eq!(
        fs::read_to_string(&file_path)
            .expect("read file-tool output")
            .trim(),
        ids.initial,
        "file tool must produce the exact token"
    );
    h.eventually("shell-tool output written", Duration::from_secs(90), || {
        Ok(
            matches!(fs::read_to_string(&shell_path), Ok(c) if c.trim() == ids.initial)
                .then_some(()),
        )
    });

    // Binding: wait for hooks_bound + session_id regardless of WHEN the tool
    // binds (Claude at SessionStart, Codex after the first turn) — the runner
    // asserts the bound state, never the timing.
    let bound = h.eventually("hook binding", Duration::from_secs(30), || {
        let Some(instance) = h.instance_json(&name)? else {
            return Ok(None);
        };
        let session_id = instance
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let hooks_bound = instance
            .get("hooks_bound")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if hooks_bound && !session_id.is_empty() {
            Ok(Some(instance))
        } else {
            Ok(None)
        }
    });
    let session_id = bound["session_id"]
        .as_str()
        .expect("bound session id")
        .to_string();
    // The launcher runner is initially process-bound; the native hook then
    // replaces it with the actual tool PID. Snapshot the canonical PID only
    // after hook/session binding so fork assertions compare like with like.
    let initial_pid = h
        .instance_pid(&name)
        .expect("query hook-bound process id")
        .filter(|pid| *pid > 1)
        .expect("hook-bound process id");
    let initial_request = mock
        .requests()
        .into_iter()
        .filter(|r| case.is_turn_request(r))
        .map(|r| r.body)
        .find(|body| body.contains(&ids.initial) && !case.is_followup_turn(body))
        .expect("mock did not receive the initial turn");
    assert!(
        initial_request.contains(&format!("[hcom:{name}]")),
        "fresh request did not contain the hcom bootstrap identity marker"
    );
    assert_eq!(
        h.instances_for_tool(tool).expect("list after bind").len(),
        1,
        "initial placeholder must collapse into one canonical instance"
    );

    // Shell op normalizes to tool:Bash for every tool.
    h.eventually(
        "shell op surfaced as tool:Bash",
        Duration::from_secs(20),
        || {
            let (code, stdout, stderr) = h.run(["events", "--last", "100"]);
            if code != 0 {
                return Err(format!("events failed: {stderr}"));
            }
            Ok(stdout.contains("tool:Bash").then_some(()))
        },
    );

    // File op surfaces with this tool's file context + detail.
    let file_detail = case.file_detail(&ids);
    let file_event = h.eventually(
        "file op surfaced as a file event",
        Duration::from_secs(20),
        || {
            let (code, stdout, stderr) = h.run([
                "events",
                "--agent",
                &name,
                "--context",
                case.file_context(),
                "--file",
                &file_detail,
                "--last",
                "20",
            ]);
            if code != 0 {
                return Err(format!("file event lookup failed: {stderr}"));
            }
            let events: Vec<Value> = stdout
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            if events.len() == 1 {
                Ok(Some(events[0].clone()))
            } else {
                Ok(None)
            }
        },
    );
    assert_eq!(
        file_event["data"]["detail"].as_str(),
        Some(file_detail.as_str()),
        "file event must retain the edited path"
    );

    // Public --collision query must join two instances' edits on one path.
    h.log_file_edit_for_test(&recipient, case.file_context(), &file_detail)
        .expect("insert peer file-edit event");
    let (collision_code, collision_stdout, collision_stderr) = h.run([
        "events",
        "--collision",
        "--file",
        &file_detail,
        "--last",
        "20",
    ]);
    assert_eq!(
        collision_code, 0,
        "collision lookup failed: {collision_stderr}"
    );
    let collision_instances: HashSet<String> = collision_stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| event["instance"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        collision_instances,
        HashSet::from([name.clone(), recipient.clone()]),
        "collision query must expose both edits: {collision_stdout}"
    );

    // --- Phase 3: outgoing hcom message ---------------------------------------
    let listen_stdout = h.eventually(
        "message received by the live recipient",
        Duration::from_secs(40),
        || {
            let output = h.recipient_output(&recipient_process_id);
            Ok(output.contains(&ids.initial).then_some(output))
        },
    );
    let delivered: Vec<Value> = listen_stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let message_value = delivered
        .iter()
        .find(|value| value.get("text").and_then(Value::as_str) == Some(ids.initial.as_str()))
        .unwrap_or_else(|| panic!("token message not delivered: {listen_stdout}"));
    assert_eq!(
        message_value.get("from").and_then(Value::as_str),
        Some(name.as_str()),
        "message sender must be the bound identity"
    );

    let (_, message_events_stdout, message_events_stderr) =
        h.run(["events", "--type", "message", "--last", "20"]);
    assert!(message_events_stderr.is_empty() || !message_events_stdout.is_empty());
    let event = message_events_stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|value| value["data"]["text"].as_str() == Some(ids.initial.as_str()))
        .unwrap_or_else(|| panic!("message event not found: {message_events_stdout}"));
    assert_eq!(event["instance"].as_str(), Some(name.as_str()));
    assert_eq!(event["data"]["from"].as_str(), Some(name.as_str()));
    assert_eq!(event["data"]["intent"].as_str(), Some("inform"));
    let delivered_sql = format!(
        "EXISTS (SELECT 1 FROM json_each(json_extract(data,'$.delivered_to')) \
         WHERE json_each.value = '{recipient}')"
    );
    let (recipient_events_code, recipient_events_stdout, _) = h.run([
        "events",
        "--type",
        "message",
        "--last",
        "20",
        "--sql",
        &delivered_sql,
    ]);
    assert_eq!(
        recipient_events_code, 0,
        "recipient-filtered event lookup failed"
    );
    assert!(
        recipient_events_stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|value| value["data"]["text"].as_str() == Some(ids.initial.as_str())),
        "token message not visible via recipient-filtered events: {recipient_events_stdout}"
    );

    // --- Phase 4: transcript ---------------------------------------------------
    let initial_transcript =
        h.eventually("discoverable transcript", Duration::from_secs(40), || {
            let (code, stdout, stderr) = h.run(["transcript", &name, "--full"]);
            if code == 0 && stdout.contains(INITIAL_PROOF) {
                Ok(Some(stdout))
            } else if code == 0 {
                Ok(None)
            } else {
                Err(format!("transcript failed: {stderr}"))
            }
        });
    assert!(
        initial_transcript.contains(INITIAL_PROOF),
        "final scripted message missing from transcript: {initial_transcript}"
    );
    let initial_bound = h
        .instance_json(&name)
        .expect("list initial bound")
        .expect("initial instance must remain active");
    let initial_transcript_path = initial_bound["transcript_path"]
        .as_str()
        .filter(|path| !path.is_empty())
        .expect("initial transcript path")
        .to_string();

    // --- Phase 5: idle inbound delivery ---------------------------------------
    h.eventually(
        "returned to listening before inbound",
        Duration::from_secs(20),
        || {
            let Some(instance) = h.instance_json(&name)? else {
                return Ok(None);
            };
            Ok((instance["status"].as_str() == Some("listening")).then_some(()))
        },
    );
    let (inbound_send_code, inbound_send_stdout, inbound_send_stderr) = h.run_as_process(
        &recipient_process_id,
        [
            "send",
            &format!("@{name}"),
            "--intent",
            "request",
            "--",
            &ids.inbound,
        ],
    );
    assert_eq!(
        inbound_send_code, 0,
        "inbound send failed: stdout={inbound_send_stdout} stderr={inbound_send_stderr}"
    );
    let inbound_transcript = h.eventually(
        "inbound reached model + transcript",
        Duration::from_secs(40),
        || {
            let (code, stdout, stderr) = h.run(["transcript", &name, "--full"]);
            if code != 0 {
                return Err(format!("inbound transcript failed: {stderr}"));
            }
            Ok(stdout.contains(INBOUND_PROOF).then_some(stdout))
        },
    );
    assert!(
        inbound_transcript.contains(&ids.inbound),
        "inbound message body missing from transcript: {inbound_transcript}"
    );
    let inbound_requests: Vec<String> = mock
        .requests()
        .into_iter()
        .filter(|r| case.is_turn_request(r))
        .map(|r| r.body)
        .filter(|body| {
            body.contains(&ids.inbound) && !body.contains(&ids.fork) && !body.contains(&ids.resume)
        })
        .collect();
    assert_eq!(
        inbound_requests.len(),
        1,
        "inbound message must produce exactly one model turn"
    );
    let inbound_request = &inbound_requests[0];
    assert!(
        inbound_request.contains(&recipient)
            && case
                .delivery_envelope_markers()
                .iter()
                .all(|marker| inbound_request.contains(marker)),
        "request did not contain the native hcom delivery envelope"
    );
    let inbound_delivery_events =
        h.eventually("inbound delivery ack", Duration::from_secs(20), || {
            let (code, stdout, stderr) = h.run(["events", "--last", "100"]);
            if code != 0 {
                return Err(format!("events failed: {stderr}"));
            }
            let delivered = stdout
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|event| {
                    event["instance"].as_str() == Some(name.as_str())
                        && event["data"]["context"]
                            .as_str()
                            .is_some_and(|context| context == format!("deliver:{recipient}"))
                })
                .count();
            Ok((delivered > 0).then_some(delivered))
        });
    assert_eq!(
        inbound_delivery_events, 1,
        "inbound must be acknowledged exactly once"
    );
    let inbound_consumed = h
        .instance_json(&name)
        .expect("list after inbound")
        .expect("active after inbound");
    assert_eq!(
        inbound_consumed["unread_count"].as_u64(),
        Some(0),
        "delivered inbound must advance the cursor exactly once"
    );

    // --- Phase 6: live fork ----------------------------------------------------
    assert!(
        h.process_group_alive(initial_pid),
        "parent died before fork"
    );
    let (fork_code, fork_stdout, fork_stderr) = h.run([
        "f",
        &name,
        "--dir",
        h.workspace.to_str().expect("UTF-8 workspace path"),
        "--hcom-prompt",
        &format!("Confirm the live fork {}", ids.fork),
    ]);
    assert_eq!(
        fork_code,
        0,
        "real {tool} fork failed:\n-- stdout --\n{fork_stdout}\n-- stderr --\n{fork_stderr}\n{}",
        h.diagnostics()
    );
    let fork_names = parse_launch_names(&fork_stdout);
    assert_eq!(
        fork_names.len(),
        1,
        "expected one fork child name; stdout={fork_stdout}"
    );
    let fork_name = fork_names[0].clone();
    assert_ne!(
        fork_name, name,
        "fork must receive a distinct hcom identity"
    );

    // The fork relaunches the tool; clear any startup gate it surfaces (Claude's
    // bypass-mode confirmation reappears per launch even in a trusted workspace).
    case.drive_startup(&h, &fork_name);
    let fork_bound = h.eventually(
        "forked process + hook bindings",
        Duration::from_secs(40),
        || {
            let Some(instance) = h.instance_json(&fork_name)? else {
                return Ok(None);
            };
            let session_id = instance
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let process_bound = instance
                .get("process_bound")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let hooks_bound = instance
                .get("hooks_bound")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if process_bound && hooks_bound && !session_id.is_empty() {
                Ok(Some(instance))
            } else {
                Ok(None)
            }
        },
    );
    let fork_session_id = fork_bound["session_id"]
        .as_str()
        .expect("fork session id")
        .to_string();
    // THE fork contract: a distinct child session. For Claude this is exactly
    // the assertion that proves the CLAUDE_ENV_FILE UUID recovery worked.
    assert_ne!(
        fork_session_id, session_id,
        "fork must create a new session"
    );
    let fork_pid = h.eventually("tracked fork process id", Duration::from_secs(10), || {
        h.instance_pid(&fork_name)
            .map(|pid| pid.filter(|pid| *pid > 1 && *pid != initial_pid))
    });

    let parent_during_fork = h
        .instance_json(&name)
        .expect("list parent during fork")
        .expect("parent must remain active while fork launches");
    assert!(
        parent_during_fork["process_bound"]
            .as_bool()
            .unwrap_or(false),
        "parent lost its process binding during fork: {parent_during_fork}"
    );
    assert_eq!(
        h.instance_pid(&name).expect("query parent pid during fork"),
        Some(initial_pid),
        "fork must not replace the live parent process"
    );
    assert_eq!(
        parent_during_fork["session_id"].as_str(),
        Some(session_id.as_str()),
        "fork must not rebind the parent session"
    );
    let active_names: HashSet<String> = h
        .instances_for_tool(tool)
        .expect("list active during fork")
        .into_iter()
        .filter_map(|instance| {
            instance
                .get("base_name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    assert_eq!(
        active_names,
        HashSet::from([name.clone(), fork_name.clone()]),
        "fork must leave exactly the parent and child active"
    );
    assert!(
        h.process_group_alive(initial_pid) && h.process_group_alive(fork_pid),
        "parent and fork must be alive concurrently"
    );

    let fork_transcript = h.eventually(
        "fork transcript inherited + fork-only turn",
        Duration::from_secs(40),
        || {
            let (code, stdout, stderr) = h.run(["transcript", &fork_name, "--full"]);
            if code != 0 {
                return Err(format!("fork transcript failed: {stderr}"));
            }
            Ok((stdout.contains(INITIAL_PROOF) && stdout.contains(FORK_PROOF)).then_some(stdout))
        },
    );
    assert!(
        fork_transcript.contains(&ids.initial) && fork_transcript.contains(&ids.fork),
        "fork transcript must contain inherited and fork markers: {fork_transcript}"
    );
    let fork_transcript_path =
        h.eventually("fork transcript path", Duration::from_secs(20), || {
            let Some(instance) = h.instance_json(&fork_name)? else {
                return Ok(None);
            };
            Ok(instance["transcript_path"]
                .as_str()
                .filter(|p| !p.is_empty())
                .map(str::to_string))
        });
    assert_ne!(
        fork_transcript_path, initial_transcript_path,
        "fork must write to an independent transcript"
    );
    let (parent_after_fork_code, parent_after_fork, parent_after_fork_stderr) =
        h.run(["transcript", &name, "--full"]);
    assert_eq!(
        parent_after_fork_code, 0,
        "parent transcript after fork failed: {parent_after_fork_stderr}"
    );
    assert!(
        parent_after_fork.contains(INITIAL_PROOF) && !parent_after_fork.contains(FORK_PROOF),
        "fork-only turn leaked into parent transcript: {parent_after_fork}"
    );
    let fork_request = mock
        .requests()
        .into_iter()
        .filter(|r| case.is_turn_request(r))
        .map(|r| r.body)
        .find(|body| body.contains(&ids.fork))
        .expect("mock did not receive the fork turn");
    assert!(
        fork_request.contains(&ids.initial) && fork_request.contains(&ids.inbound),
        "fork request did not inherit the complete parent conversation"
    );
    // Identity reset, tool-agnostically: the child's name can only appear via the
    // fork's own identity bootstrap/reset (it is brand new, absent from inherited
    // history), while the parent's name appears from the inherited history. Codex
    // delivers this as a "new hcom identity {child}" prompt; Claude as a fresh
    // SessionStart bootstrap naming the child after the parent's inherited one.
    assert!(
        fork_request.contains(&fork_name) && fork_request.contains(&name),
        "fork request must carry the child identity bootstrap ({fork_name}) \
         alongside inherited parent history ({name})"
    );

    // --- Phase 7: kill parent, child survives ---------------------------------
    let (kill_code, kill_stdout, kill_stderr) = h.run(["kill", &name]);
    assert_eq!(
        kill_code, 0,
        "parent kill failed: stdout={kill_stdout} stderr={kill_stderr}"
    );
    h.eventually(
        "killed parent inactive + group gone",
        Duration::from_secs(10),
        || {
            let inactive = h.instance_json(&name)?.is_none();
            Ok((inactive && !h.process_group_alive(initial_pid)).then_some(()))
        },
    );
    assert!(
        h.instance_json(&fork_name)
            .expect("list fork after parent kill")
            .is_some()
            && h.process_group_alive(fork_pid),
        "killing the parent must not stop its fork"
    );
    assert_eq!(
        h.instances_for_tool(tool)
            .expect("list after parent kill")
            .into_iter()
            .filter_map(|instance| instance["base_name"].as_str().map(str::to_string))
            .collect::<Vec<_>>(),
        vec![fork_name.clone()],
        "only the fork should remain active after killing the parent"
    );
    let (stopped_code, stopped_stdout, stopped_stderr) = h.run(["list", "--stopped", &name]);
    assert_eq!(
        stopped_code, 0,
        "stopped snapshot lookup failed: {stopped_stderr}"
    );
    assert!(
        stopped_stdout.contains(&session_id) && stopped_stdout.contains("killed"),
        "killed snapshot did not preserve session {session_id} and reason: {stopped_stdout}"
    );

    // --- Phase 8: resume parent under the same identity -----------------------
    let resume_prompt = format!("Confirm the resumed session {}", ids.resume);
    let (resume_code, resume_stdout, resume_stderr) =
        h.run(["r", &name, "--hcom-prompt", &resume_prompt]);
    assert_eq!(
        resume_code,
        0,
        "real {tool} resume failed:\n-- stdout --\n{resume_stdout}\n-- stderr --\n{resume_stderr}\n{}",
        h.diagnostics()
    );
    wait_process_bound(
        &h,
        &case,
        &name,
        "resumed process-bound under same identity",
    );
    // Resume also relaunches the tool; clear any startup gate it re-surfaces.
    case.drive_startup(&h, &name);
    let resumed_pid = h.eventually("new resumed process id", Duration::from_secs(10), || {
        h.instance_pid(&name)
            .map(|pid| pid.filter(|pid| *pid > 1 && *pid != initial_pid))
    });
    wait_pty_ready(&h, &name, "resumed PTY inject endpoint");
    let resumed_transcript = {
        let mock = &mock;
        let ids = &ids;
        h.eventually("resumed lifecycle prompt", Duration::from_secs(60), || {
            let resume_reached_model = mock
                .request_bodies()
                .iter()
                .any(|body| body.contains(&ids.resume));
            let (code, stdout, stderr) = h.run(["transcript", &name, "--full"]);
            if code != 0 {
                return Err(format!("resumed transcript failed: {stderr}"));
            }
            Ok((resume_reached_model && stdout.contains(RESUME_PROOF)).then_some(stdout))
        })
    };
    assert!(
        resumed_transcript.contains(INITIAL_PROOF)
            && resumed_transcript.contains(RESUME_PROOF)
            && !resumed_transcript.contains(FORK_PROOF),
        "resumed parent transcript lost history or absorbed fork history: {resumed_transcript}"
    );
    let resume_request = mock
        .requests()
        .into_iter()
        .filter(|r| case.is_turn_request(r))
        .map(|r| r.body)
        .find(|body| body.contains(&ids.resume))
        .expect("mock did not receive the resume turn");
    assert!(
        resume_request.contains(&ids.initial)
            && resume_request.contains(&ids.inbound)
            && !resume_request.contains(&ids.fork),
        "resumed request must inherit parent history without fork-only history"
    );
    assert!(
        resume_request.contains(&format!("[hcom:{name}]")),
        "resume request did not retain the original hcom identity bootstrap"
    );
    let rebound_parent = h
        .instance_json(&name)
        .expect("list rebound parent")
        .expect("resumed parent must be active");
    assert_eq!(
        rebound_parent["session_id"].as_str(),
        Some(session_id.as_str()),
        "resume must restore the original session id"
    );
    assert_eq!(
        rebound_parent["transcript_path"].as_str(),
        Some(initial_transcript_path.as_str()),
        "resume must continue the original transcript"
    );
    assert!(
        rebound_parent["hooks_bound"].as_bool().unwrap_or(false)
            && rebound_parent["process_bound"].as_bool().unwrap_or(false),
        "resumed parent must restore both bindings: {rebound_parent}"
    );
    assert!(
        h.process_group_alive(resumed_pid) && h.process_group_alive(fork_pid),
        "resumed parent and fork must remain independently alive"
    );
    assert_eq!(
        h.instances_for_tool(tool).expect("list after resume").len(),
        2,
        "resume must restore the parent without duplicating either identity"
    );

    // --- Phase 9: cleanup + request audit -------------------------------------
    let (final_kill_code, final_kill_stdout, final_kill_stderr) = h.run(["kill", &name]);
    assert_eq!(
        final_kill_code, 0,
        "final parent kill failed: stdout={final_kill_stdout} stderr={final_kill_stderr}"
    );
    h.eventually(
        "killed parent inactive + group gone",
        Duration::from_secs(10),
        || {
            let inactive = h.instance_json(&name)?.is_none();
            Ok((inactive && !h.process_group_alive(resumed_pid)).then_some(()))
        },
    );
    let (fork_kill_code, fork_kill_stdout, fork_kill_stderr) = h.run(["kill", &fork_name]);
    assert_eq!(
        fork_kill_code, 0,
        "fork cleanup failed: stdout={fork_kill_stdout} stderr={fork_kill_stderr}"
    );
    h.eventually(
        "fork inactive + group gone",
        Duration::from_secs(10),
        || {
            let inactive = h.instance_json(&fork_name)?.is_none();
            Ok((inactive && !h.process_group_alive(fork_pid)).then_some(()))
        },
    );
    assert!(
        h.instances_for_tool(tool)
            .expect("list after cleanup")
            .is_empty(),
        "all identities must be inactive after cleanup"
    );
    assert!(
        !h.process_group_alive(initial_pid)
            && !h.process_group_alive(resumed_pid)
            && !h.process_group_alive(fork_pid),
        "all process groups must be gone after cleanup"
    );

    let unexpected = mock.unexpected();
    assert!(
        unexpected.is_empty(),
        "mock received {} unexpected request(s):\n{}",
        unexpected.len(),
        unexpected
            .iter()
            .map(|r| format!("  {} {} (body {} bytes)", r.method, r.path, r.body.len()))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let transport_errors = mock.transport_errors();
    assert!(
        transport_errors.is_empty(),
        "mock hit {} transport error(s) (truncated/malformed requests):\n  {}",
        transport_errors.len(),
        transport_errors.join("\n  ")
    );
    let bodies = mock.request_bodies();
    let turns: Vec<String> = mock
        .requests()
        .into_iter()
        .filter(|r| case.is_turn_request(r))
        .map(|r| r.body)
        .collect();
    let initial_turns = turns
        .iter()
        .filter(|body| {
            body.contains(&ids.initial)
                && !case.is_followup_turn(body)
                && !body.contains(&ids.inbound)
                && !body.contains(&ids.fork)
                && !body.contains(&ids.resume)
        })
        .count();
    let fork_turns = turns
        .iter()
        .filter(|body| body.contains(&ids.fork) && !body.contains(FORK_PROOF))
        .count();
    let resume_turns = turns
        .iter()
        .filter(|body| body.contains(&ids.resume) && !body.contains(RESUME_PROOF))
        .count();
    assert_eq!(initial_turns, 1, "initial prompt must be accepted once");
    assert_eq!(fork_turns, 1, "fork prompt must be submitted once");
    assert_eq!(resume_turns, 1, "resume prompt must be accepted once");
    let token_messages = h
        .run(["events", "--type", "message", "--last", "100"])
        .1
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["data"]["text"].as_str() == Some(ids.initial.as_str()))
        .count();
    assert_eq!(
        token_messages, 1,
        "initial lifecycle must send exactly one recipient message"
    );
    for body in bodies {
        if body.trim_start().starts_with('{') {
            serde_json::from_str::<Value>(&body).unwrap_or_else(|error| {
                panic!("mock received invalid request JSON: {error}\n{body}")
            });
        }
    }
}
