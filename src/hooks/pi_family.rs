//! Hook RPC dispatch shared by Pi, its fork Oh My Pi, and Hermes. Their plugins
//! call `hcom <tool>-{start,status,read,beforetool,stop}` with the same argv
//! shape; only the tool name and OMP's soft-stop recovery differ.

use std::time::Instant;

use crate::bootstrap;
use crate::db::HcomDb;
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instances;
use crate::log::{log_error, log_info};
use crate::shared::ST_LISTENING;
use crate::shared::context::HcomContext;
use crate::tool::Tool;

use super::common;
use super::plugin_rpc::{self, parse_flag, parse_pid_flag, persist_pid, resolve_process_id};

pub(crate) struct PiFamily {
    pub tool: Tool,
    /// OMP soft-stops on session_shutdown (task subagents share the parent
    /// process), which clears the process binding; the next start recovers it
    /// from `HCOM_INSTANCE_NAME`.
    pub recover_soft_stopped: bool,
}

pub(crate) const PI: PiFamily = PiFamily {
    tool: Tool::Pi,
    recover_soft_stopped: false,
};

pub(crate) const OMP: PiFamily = PiFamily {
    tool: Tool::Omp,
    recover_soft_stopped: true,
};

pub(crate) const HERMES: PiFamily = PiFamily {
    tool: Tool::Hermes,
    recover_soft_stopped: false,
};

impl PiFamily {
    fn name(&self) -> &'static str {
        self.tool.as_str()
    }

    /// Register the extension's notify port and wake the PTY loop: the bind is
    /// launch readiness for plugin-driven tools (see `delivery.rs`).
    pub(crate) fn upsert_plugin_notify_endpoint(
        &self,
        db: &HcomDb,
        instance_name: &str,
        port: u16,
    ) {
        if let Err(e) = db.upsert_notify_endpoint(instance_name, "plugin", port) {
            log_error(
                "native",
                &format!("{}.register_notify_fail", self.name()),
                &format!(
                    "Failed to register plugin notify port for {}: {}",
                    instance_name, e
                ),
            );
            return;
        }
        crate::notify::wake(db, instance_name, crate::notify::WakeKind::DELIVERY_LOOPS);
    }

    fn recover_binding(
        &self,
        ctx: &HcomContext,
        session_id: &str,
        process_id: &str,
        db: &HcomDb,
    ) -> Option<String> {
        if !self.recover_soft_stopped {
            return None;
        }
        let name = ctx
            .raw_env
            .get("HCOM_INSTANCE_NAME")
            .filter(|s| !s.is_empty())?;
        instance_binding::recover_process_binding_for_instance(db, name, session_id, process_id)
    }

    pub(crate) fn handle_start(
        &self,
        ctx: &HcomContext,
        db: &HcomDb,
        argv: &[String],
    ) -> (i32, String) {
        // Plugin RPC returns JSON errors on exit 0 so the extension can handle
        // setup failures without the agent treating the hook itself as failed.
        let session_id = match parse_flag(argv, "--session-id") {
            Some(sid) => sid,
            None => return (0, r#"{"error":"Missing --session-id"}"#.to_string()),
        };
        let transcript_path = parse_flag(argv, "--transcript-path");
        let cwd = parse_flag(argv, "--cwd");
        let notify_port: Option<u16> =
            parse_flag(argv, "--notify-port").and_then(|s| s.parse().ok());

        let requested_pid = parse_pid_flag(argv);

        let process_id = match resolve_process_id(argv, ctx.process_id.as_deref()) {
            Some(pid) => pid,
            None => return (0, r#"{"error":"HCOM_PROCESS_ID not set"}"#.to_string()),
        };

        // A caller that declares its own binding key (`--process-id`, no launcher)
        // gets an identity created; the launcher/`hcom start` path keeps the previous
        // behaviour exactly, including adopting an `adhoc` row.
        let self_minted = parse_flag(argv, "--process-id").is_some();
        let bound = if self_minted {
            match instance_binding::bind_session_to_process_for_tool(
                db,
                &session_id,
                Some(&process_id),
                self.name(),
                true,
            ) {
                instance_binding::ToolCheckedBind::Bound(name) => Some(name),
                _ => None,
            }
        } else {
            instance_binding::bind_session_to_process(db, &session_id, Some(&process_id))
        };
        let Some(instance_name) =
            bound.or_else(|| self.recover_binding(ctx, &session_id, &process_id, db))
        else {
            return (
                0,
                r#"{"error":"No instance bound to this process"}"#.to_string(),
            );
        };

        plugin_rpc::initialize_last_event_id(db, &instance_name);
        lifecycle::set_status(
            db,
            &instance_name,
            ST_LISTENING,
            "start",
            Default::default(),
        );
        persist_pid(db, &instance_name, requested_pid);
        instance_binding::capture_and_store_launch_context(db, &instance_name);

        let mut updates = serde_json::Map::new();
        updates.insert("tool".into(), serde_json::json!(self.name()));
        updates.insert("session_id".into(), serde_json::json!(&session_id));
        if let Some(path) = transcript_path.as_ref().filter(|p| !p.is_empty()) {
            updates.insert("transcript_path".into(), serde_json::json!(path));
        }
        let cwd_value = cwd
            .as_deref()
            .filter(|p| !p.is_empty())
            .or_else(|| ctx.cwd.to_str());
        if let Some(cwd) = cwd_value {
            updates.insert("directory".into(), serde_json::json!(cwd));
        }
        instances::update_instance_position(db, &instance_name, &updates);
        if let Some(port) = notify_port {
            self.upsert_plugin_notify_endpoint(db, &instance_name, port);
        }
        log_info(
            "hooks",
            &format!("{}-start.bind", self.name()),
            &format!("instance={} session_id={}", instance_name, session_id),
        );
        crate::relay::worker::try_spawn_worker();

        let response = serde_json::json!({
            "name": instance_name,
            "session_id": session_id,
            "bootstrap": bootstrap::get_bootstrap(db, ctx, &instance_name, self.name()),
        });
        (0, response.to_string())
    }

    pub(crate) fn dispatch(&self, hook_name: &str, argv: &[String]) -> (i32, String) {
        let start = Instant::now();
        let ctx = HcomContext::from_os();
        crate::paths::ensure_hcom_directories_at(&ctx.hcom_dir);
        let db = match HcomDb::open() {
            Ok(db) => db,
            Err(e) => {
                log_error(
                    "hooks",
                    "hook.error",
                    &format!("hook={} op=db_open err={}", hook_name, e),
                );
                return plugin_rpc::error(format!("DB open failed: {}", e));
            }
        };
        if !common::hook_gate_check_argv(&ctx, argv) {
            return (0, String::new());
        }
        let handler_argv = argv.strip_prefix(&[hook_name.to_string()]).unwrap_or(argv);
        let hook = hook_name
            .strip_prefix(self.name())
            .and_then(|s| s.strip_prefix('-'))
            .unwrap_or("");
        if matches!(hook, "status" | "read" | "beforetool" | "stop")
            && let Err(reply) = plugin_rpc::require_attached(&ctx, &db, handler_argv)
        {
            return reply;
        }
        let handler_start = Instant::now();
        let (exit_code, output) = common::dispatch_with_panic_guard(
            self.name(),
            hook_name,
            plugin_rpc::error("internal panic"),
            || match hook {
                "start" => self.handle_start(&ctx, &db, handler_argv),
                "status" => plugin_rpc::handle_status(&db, handler_argv),
                "read" => plugin_rpc::handle_read(&db, handler_argv),
                "beforetool" => plugin_rpc::handle_tool(&db, handler_argv, self.name()),
                "stop" => plugin_rpc::handle_stop(&db, handler_argv),
                _ => plugin_rpc::error(format!("Unknown {} hook: {}", self.name(), hook_name)),
            },
        );
        log_info(
            "hooks",
            &format!("{}.dispatch.timing", self.name()),
            &format!(
                "hook={} handler_ms={:.2} total_ms={:.2} exit_code={}",
                hook_name,
                handler_start.elapsed().as_secs_f64() * 1000.0,
                start.elapsed().as_secs_f64() * 1000.0,
                exit_code
            ),
        );
        (exit_code, output)
    }
}
