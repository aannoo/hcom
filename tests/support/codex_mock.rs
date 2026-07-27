//! Minimal mock of the OpenAI Responses API for the real Codex CLI test.
//!
//! Codex has no built-in fake-response mode, so the only way to drive a real,
//! pinned `codex` binary deterministically and for free is to point it at a
//! localhost HTTP provider and script the `text/event-stream` turns ourselves.
//! The external oracle lives outside the repo (the real codex binary parsing a
//! real Responses SSE stream), so the test cannot be gamed by editing hcom to
//! match the test.
//!
//! Transport lives in [`super::mock_http`]; this module is the OpenAI Responses
//! codec (SSE builders + body-addressed scripting). Codex only needs each turn's
//! events terminated by `response.completed`; streaming deltas are not required.

use serde_json::Value;

use super::Hcom;
pub use super::mock_http::Reply;
use super::mock_http::{MockHttp, RecordedRequest};
use super::real_tool::{
    FORK_PROOF, INBOUND_PROOF, INITIAL_PROOF, RESUME_PROOF, ScenarioIds, ToolCase, ToolMeta,
};

const CODEX_META: ToolMeta = ToolMeta {
    tool: "codex",
    binary: "codex",
    pinned_version: "0.145.0",
    install_command: "npm install --global @openai/codex@0.145.0",
};

/// Codex adapter for the shared real-tool lifecycle. Codex has no fake-response
/// mode, so it runs against a localhost OpenAI Responses mock; every turn is
/// scripted as `text/event-stream`. The Responses input carries the full
/// history in each request, so turns are matched freshest-signal-first.
#[derive(Clone)]
pub struct CodexCase;

impl ToolCase for CodexCase {
    fn meta(&self) -> &ToolMeta {
        &CODEX_META
    }

    fn file_context(&self) -> &'static str {
        "tool:apply_patch"
    }

    fn file_detail(&self, ids: &ScenarioIds) -> String {
        ids.file_rel.clone()
    }

    fn provider_base_url(&self, port: u16) -> String {
        format!("http://127.0.0.1:{port}/v1")
    }

    fn prepare(&self, h: &Hcom, base_url: &str) {
        h.prepare_codex_config(base_url);
        let (code, stdout, stderr) = h.run(["config", "codex_sandbox_mode", "danger-full-access"]);
        assert_eq!(
            code, 0,
            "set Codex lifecycle sandbox mode failed: stdout={stdout} stderr={stderr}"
        );
    }

    fn launch_args(&self, _h: &Hcom) -> Vec<String> {
        // hcom supplies Codex's sandbox/trust/add-dir flags itself.
        Vec::new()
    }

    fn is_followup_turn(&self, body: &str) -> bool {
        body.contains("function_call_output") || body.contains("custom_tool_call_output")
    }

    fn is_turn_request(&self, _req: &RecordedRequest) -> bool {
        // Codex has only the single Responses route; every request is a turn.
        true
    }

    fn delivery_envelope_markers(&self) -> &'static [&'static str] {
        &["<hcom>", "request"]
    }

    fn respond(&self, req: &RecordedRequest, ids: &ScenarioIds) -> Reply {
        let body = &req.body;
        let has_output =
            |call_id: &str| body.contains("function_call_output") && body.contains(call_id);
        let has_custom =
            |call_id: &str| body.contains("custom_tool_call_output") && body.contains(call_id);
        let write_cmd = if cfg!(windows) {
            format!(
                "node -e \"require('fs').writeFileSync('{}', '{}')\"",
                ids.shell_rel.replace('\\', "\\\\").replace('\'', "\\'"),
                ids.initial.replace('\\', "\\\\").replace('\'', "\\'")
            )
        } else {
            format!("echo {} > {}", ids.initial, ids.shell_rel)
        };
        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+{}\n*** End Patch\n",
            ids.file_rel, ids.initial
        );
        if body.contains(&ids.resume) {
            Reply::Sse(sse(&[
                created("RESP_R"),
                message("ITEM_R", &format!("{RESUME_PROOF} {}", ids.resume)),
                completed("RESP_R"),
            ]))
        } else if body.contains(&ids.fork) {
            Reply::Sse(sse(&[
                created("RESP_F"),
                message("ITEM_F", &format!("{FORK_PROOF} {}", ids.fork)),
                completed("RESP_F"),
            ]))
        } else if body.contains(&ids.inbound) {
            Reply::Sse(sse(&[
                created("RESP_D"),
                message("ITEM_D", &format!("{INBOUND_PROOF} {}", ids.inbound)),
                completed("RESP_D"),
            ]))
        } else if has_output("CALL2") {
            Reply::Sse(sse(&[
                created("RESP3"),
                message("ITEM3", &format!("{INITIAL_PROOF} {}", ids.initial)),
                completed("RESP3"),
            ]))
        } else if has_output("CALL1") {
            Reply::Sse(sse(&[
                created("RESP2"),
                shell_call("CALL2", &ids.send_cmd),
                completed("RESP2"),
            ]))
        } else if has_custom("PATCH1") {
            Reply::Sse(sse(&[
                created("RESP1B"),
                shell_call("CALL1", &write_cmd),
                completed("RESP1B"),
            ]))
        } else if body.contains(&ids.initial) {
            Reply::Sse(sse(&[
                created("RESP1"),
                custom_tool_call("PATCH1", "apply_patch", &patch),
                completed("RESP1"),
            ]))
        } else {
            Reply::Status(500)
        }
    }
}

/// A scripted localhost Responses provider — a thin Codex-flavored wrapper over
/// the shared [`MockHttp`] transport that keeps the body-only responder the
/// Codex test scripts against (the Responses input carries full history, so the
/// freshest signal is matched first; path/headers are irrelevant for Codex).
pub struct MockResponses {
    inner: MockHttp,
}

impl MockResponses {
    /// Start serving on an ephemeral localhost port. `responder` maps each
    /// request body to a [`Reply`]; it runs on worker threads so it must be
    /// `Send + Sync`.
    pub fn start<F>(responder: F) -> std::io::Result<Self>
    where
        F: Fn(&str) -> Reply + Send + Sync + 'static,
    {
        let inner = MockHttp::start(move |request: &RecordedRequest| responder(&request.body))?;
        Ok(Self { inner })
    }

    /// Base URL for a Codex `model_providers` entry (`.../v1`).
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.inner.port())
    }

    /// Every request body observed so far, in arrival order.
    pub fn requests(&self) -> Vec<String> {
        self.inner.request_bodies()
    }

    /// Request bodies the responder rejected with [`Reply::Status`].
    pub fn unexpected(&self) -> Vec<String> {
        self.inner
            .unexpected()
            .into_iter()
            .map(|request| request.body)
            .collect()
    }

    /// Transport-level errors observed by the shared HTTP server.
    pub fn transport_errors(&self) -> Vec<String> {
        self.inner.transport_errors()
    }
}

/// Frame a list of `(event_type, json)` pairs into a Responses SSE body.
pub fn sse(events: &[(&str, Value)]) -> Vec<u8> {
    let mut out = String::new();
    for (typ, obj) in events {
        out.push_str("event: ");
        out.push_str(typ);
        out.push_str("\ndata: ");
        out.push_str(&serde_json::to_string(obj).expect("serialize SSE event"));
        out.push_str("\n\n");
    }
    out.into_bytes()
}

/// `response.created` event for `id`.
pub fn created(id: &str) -> (&'static str, Value) {
    (
        "response.created",
        serde_json::json!({"type": "response.created", "response": {"id": id}}),
    )
}

/// A completed assistant text message output item.
pub fn message(id: &str, text: &str) -> (&'static str, Value) {
    (
        "response.output_item.done",
        serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "id": id,
                "content": [{"type": "output_text", "text": text}]
            }
        }),
    )
}

/// A function-call output item. Codex 0.139 exposes its shell tool as the
/// `exec_command` function (no `local_shell_call`); `arguments` is a JSON
/// string, e.g. `{"cmd": "echo hi"}`.
pub fn function_call(call_id: &str, name: &str, arguments: &str) -> (&'static str, Value) {
    (
        "response.output_item.done",
        serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments
            }
        }),
    )
}

/// Platform-specific shell function advertised by pinned Codex.
pub fn shell_call(call_id: &str, command: &str) -> (&'static str, Value) {
    if cfg!(windows) {
        function_call(
            call_id,
            "shell_command",
            &serde_json::json!({ "command": command }).to_string(),
        )
    } else {
        function_call(
            call_id,
            "exec_command",
            &serde_json::json!({ "cmd": command }).to_string(),
        )
    }
}

/// A freeform custom-tool call, used by Codex for `apply_patch`.
pub fn custom_tool_call(call_id: &str, name: &str, input: &str) -> (&'static str, Value) {
    (
        "response.output_item.done",
        serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "custom_tool_call",
                "call_id": call_id,
                "name": name,
                "input": input
            }
        }),
    )
}

/// `response.completed` with the zeroed usage object Codex requires.
pub fn completed(id: &str) -> (&'static str, Value) {
    (
        "response.completed",
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": id,
                "usage": {
                    "input_tokens": 0,
                    "input_tokens_details": null,
                    "output_tokens": 0,
                    "output_tokens_details": null,
                    "total_tokens": 0
                }
            }
        }),
    )
}
