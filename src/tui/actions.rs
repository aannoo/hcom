use crate::tui::app::{App, ConfirmAction};
use crate::tui::model::*;
use crate::tui::rpc::Response;
use crate::tui::rpc_async::{RpcOp, RpcResult};
use crate::tui::theme::Theme;

/// Error flash for a failed RPC operation.
fn rpc_error_flash(label: &str, result: Result<Response, String>) -> Flash {
    let detail = match result {
        Ok(resp) => resp.error_message(),
        Err(e) => e,
    };
    Flash::new(format!("{}: {}", label, detail), Theme::flash_err())
}

/// Extract output lines from an RPC response (for CommandOutput display).
fn rpc_output_lines(result: Result<Response, String>) -> Vec<String> {
    match result {
        Ok(resp) => {
            let lines = resp.combined_output_lines();
            if lines.is_empty() {
                vec!["(no output)".into()]
            } else {
                lines
            }
        }
        Err(e) => vec![format!("Error: {}", e)],
    }
}

impl App {
    /// Reload data from the source, preserving UI state.
    pub fn reload_data(&mut self) {
        self.reload_data_inner(false);
    }

    /// Force refresh even when datasource did not report any changes.
    pub fn reload_data_force(&mut self) {
        self.reload_data_inner(true);
    }

    /// Update FTS search results when a text search query is active.
    pub fn update_search(&mut self) {
        let query = self.active_search_query().map(|q| q.to_owned());
        match query {
            Some(q) => {
                let limit = std::env::var("HCOM_TUI_TIMELINE_LIMIT")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(5000);
                self.data.search_results = Some(self.source.search_timeline(&q, limit));
            }
            None => self.data.search_results = None,
        }
    }

    fn reload_data_inner(&mut self, force: bool) {
        // Save cursor name for stability across reloads
        let saved_cursor_name = self.cursor_agent_name();

        let mut new_data = if force {
            self.source.load()
        } else {
            match self.source.load_if_changed() {
                Some(data) => data,
                None => {
                    self.update_search();
                    return;
                }
            }
        };
        // When "show all stopped" is active, replace stopped_agents with the full list
        if self.ui.show_all_stopped {
            new_data.stopped_agents = self.source.load_all_stopped();
        }
        // Prune selections that no longer exist
        let live_names: std::collections::HashSet<String> = new_data
            .agents
            .iter()
            .map(|a| a.name.clone())
            .chain(new_data.remote_agents.iter().map(|a| a.display_name()))
            .chain(new_data.stopped_agents.iter().map(|a| a.name.clone()))
            .collect();
        self.ui.selected.retain(|n| live_names.contains(n));

        // Edge-trigger the "relay connected" 5-second flash on not-Connected
        // → Connected. Tracks against the derived RelayHealth, not raw KV, so
        // the flash semantics match what the indicator actually shows.
        let now_connected = matches!(new_data.relay_health, crate::relay::RelayHealth::Connected);
        if now_connected && !self.ui.last_relay_was_connected {
            self.ui.relay_text_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
        }
        self.ui.last_relay_was_connected = now_connected;

        self.data = new_data;

        // Sync popup enabled state from DB (source of truth after config.toml change)
        if let Some(popup) = self.ui.relay_popup.as_mut() {
            if !popup.toggling {
                popup.enabled = self.data.relay_enabled;
            }
        }

        // Restore cursor by name
        if let Some(ref name) = saved_cursor_name {
            if let Some(pos) = self.data.agents.iter().position(|a| &a.name == name) {
                self.ui.cursor = pos;
            }
        }

        // Clamp cursor
        let total = self.total_visible_rows();
        if total > 0 && self.ui.cursor >= total {
            self.ui.cursor = total - 1;
        }

        // Update tracked cursor name
        self.ui.cursor_name = self.cursor_agent_name();

        self.update_search();
    }

    /// Name of the agent currently at cursor position (local agents only).
    fn cursor_agent_name(&self) -> Option<String> {
        if self.ui.cursor < self.data.agents.len() {
            Some(self.data.agents[self.ui.cursor].name.clone())
        } else {
            None
        }
    }

    pub fn apply_rpc_result(&mut self, result: RpcResult) {
        match result.op {
            RpcOp::Send { .. } => match result.result {
                Ok(resp) if resp.ok() => {
                    self.ui.flash = Some(Flash::new("Message sent".into(), Theme::flash_ok()));
                    self.reload_data();
                }
                other => self.ui.flash = Some(rpc_error_flash("Send failed", other)),
            },
            RpcOp::KillAgent { name } => match result.result {
                Ok(resp) if resp.ok() => {
                    self.ui.selected.remove(&name);
                    self.ui.flash =
                        Some(Flash::new(format!("Killed {}", name), Theme::flash_info()));
                    self.reload_data_force();
                }
                other => self.ui.flash = Some(rpc_error_flash("Kill failed", other)),
            },
            RpcOp::ForkAgent { name } => match result.result {
                Ok(resp) if resp.ok() => {
                    self.ui.flash = Some(Flash::new(format!("Forked {}", name), Theme::flash_ok()));
                    self.reload_data_force();
                }
                other => self.ui.flash = Some(rpc_error_flash("Fork failed", other)),
            },
            RpcOp::KillPid { pid } => match result.result {
                Ok(resp) if resp.ok() => {
                    self.ui.flash = Some(Flash::new(
                        format!("Killed PID {}", pid),
                        Theme::flash_info(),
                    ));
                    self.reload_data_force();
                }
                other => self.ui.flash = Some(rpc_error_flash("Kill PID failed", other)),
            },
            RpcOp::Tag { name, tag } => match result.result {
                Ok(resp) if resp.ok() => {
                    if let Some(agent) = self.data.agents.iter_mut().find(|a| a.name == name) {
                        agent.tag = tag.clone();
                    }
                    self.ui.flash = if tag.is_empty() {
                        Some(Flash::new("Tag cleared".into(), Theme::flash_info()))
                    } else {
                        Some(Flash::new(format!("Tagged {}", tag), Theme::flash_ok()))
                    };
                    self.reload_data();
                }
                other => self.ui.flash = Some(rpc_error_flash("Tag failed", other)),
            },
            RpcOp::Project { name, project } => match result.result {
                Ok(resp) if resp.ok() => {
                    if let Some(agent) = self.data.agents.iter_mut().find(|a| a.name == name) {
                        agent.project = project.clone();
                    }
                    self.ui.flash = if project.is_empty() {
                        Some(Flash::new("Project cleared".into(), Theme::flash_info()))
                    } else {
                        Some(Flash::new(format!("Project set to {}", project), Theme::flash_ok()))
                    };
                    self.reload_data();
                }
                other => self.ui.flash = Some(rpc_error_flash("Project set failed", other)),
            },
            RpcOp::Launch { tool, count, .. } => match result.result {
                Ok(resp) if resp.ok() => {
                    self.ui.flash = Some(Flash::new(
                        format!("Launched {} {}", count, tool.name()),
                        Theme::flash_ok(),
                    ));
                    self.reload_data_force();
                }
                other => self.ui.flash = Some(rpc_error_flash("Launch failed", other)),
            },
            RpcOp::RelayToggle { enable } => {
                let ok = matches!(&result.result, Ok(resp) if resp.ok());
                if let Some(popup) = self.ui.relay_popup.as_mut() {
                    popup.toggling = false;
                    if ok {
                        popup.enabled = enable;
                    }
                }
                if ok {
                    self.ui.flash = Some(Flash::new(
                        format!("Relay {}", if enable { "enabled" } else { "disabled" }),
                        if enable {
                            Theme::flash_ok()
                        } else {
                            Theme::flash_info()
                        },
                    ));
                    self.reload_data_force();
                } else {
                    self.ui.flash = Some(rpc_error_flash("Relay toggle failed", result.result));
                }
            }
            RpcOp::RelayStatus => {
                self.show_command_output("relay status", rpc_output_lines(result.result));
            }
            RpcOp::RelayNew => {
                self.show_command_output("relay new", rpc_output_lines(result.result));
            }
            RpcOp::RelayConnect { token } => {
                let success = matches!(&result.result, Ok(resp) if resp.ok());
                let output = rpc_output_lines(result.result);
                if !success {
                    self.ui.flash = Some(Flash::new(
                        "Relay connect failed".into(),
                        Theme::flash_err(),
                    ));
                }
                self.show_command_output(&format!("relay connect {}", token), output);
                self.reload_data_force();
            }
            RpcOp::Command { cmd } => {
                self.show_command_output(&cmd, rpc_output_lines(result.result));
            }
        }
    }

    fn show_command_output(&mut self, label: &str, output: Vec<String>) {
        self.ui.command_result = Some(CommandResult {
            label: label.into(),
            output,
        });
        if self.ui.view_mode == ViewMode::Inline {
            // Eject to scrollback instead of entering CommandOutput mode
            self.ui.pending_eject_cmd = true;
            self.ui.mode = InputMode::Navigate;
        } else {
            self.ui.mode = InputMode::CommandOutput;
            self.ui.msg_scroll = 0;
        }
    }

    pub fn execute_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::KillAgents(ref names) => {
                for name in names {
                    if let Err(e) = self.enqueue_rpc(RpcOp::KillAgent { name: name.clone() }) {
                        self.ui.flash = Some(Flash::new(
                            format!("Kill failed: {}", e),
                            Theme::flash_err(),
                        ));
                        return;
                    }
                }
                let label = if names.len() == 1 {
                    format!("Killing {}", names[0])
                } else {
                    format!("Killing {} agents", names.len())
                };
                self.ui.flash = Some(Flash::new(label, Theme::flash_info()));
            }
            ConfirmAction::ForkAgents(ref names) => {
                for name in names {
                    if let Err(e) = self.enqueue_rpc(RpcOp::ForkAgent { name: name.clone() }) {
                        self.ui.flash = Some(Flash::new(
                            format!("Fork failed: {}", e),
                            Theme::flash_err(),
                        ));
                        return;
                    }
                }
                let label = if names.len() == 1 {
                    format!("Forking {}", names[0])
                } else {
                    format!("Forking {} agents", names.len())
                };
                self.ui.flash = Some(Flash::new(label, Theme::flash_info()));
            }
            ConfirmAction::KillOrphan(pid) => {
                if !self.data.orphans.iter().any(|o| o.pid == pid) {
                    self.ui.flash = Some(Flash::new(
                        format!("PID {} no longer tracked", pid),
                        Theme::flash_err(),
                    ));
                    return;
                }
                if let Err(e) = self.enqueue_rpc(RpcOp::KillPid { pid }) {
                    self.ui.flash = Some(Flash::new(
                        format!("Kill PID failed: {}", e),
                        Theme::flash_err(),
                    ));
                } else {
                    self.ui.flash = Some(Flash::new(
                        format!("Killing PID {}", pid),
                        Theme::flash_info(),
                    ));
                }
            }
            ConfirmAction::OrphanAction(_) => {
                unreachable!("OrphanAction dispatched in handle_confirm");
            }
        }
    }

    pub fn recover_orphan(&mut self, pid: u32) {
        if !self.data.orphans.iter().any(|o| o.pid == pid) {
            self.ui.flash = Some(Flash::new(
                format!("PID {} no longer tracked", pid),
                Theme::flash_err(),
            ));
            return;
        }
        let cmd = format!("start --orphan {}", pid);
        if let Err(e) = self.enqueue_rpc(RpcOp::Command { cmd }) {
            self.ui.flash = Some(Flash::new(
                format!("Recover failed: {}", e),
                Theme::flash_err(),
            ));
        } else {
            self.ui.flash = Some(Flash::new(
                format!("Recovering PID {}", pid),
                Theme::flash_info(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{App, DataState};
    use crate::tui::data::DataSource;
    use crate::tui::model::{Agent, InputMode, RelayPopupState, ViewMode};

    struct TestSource {
        snapshot: DataState,
    }

    impl TestSource {
        fn new(snapshot: DataState) -> Self {
            Self { snapshot }
        }
    }

    impl DataSource for TestSource {
        fn load(&mut self) -> DataState {
            self.snapshot.clone()
        }

        fn load_if_changed(&mut self) -> Option<DataState> {
            Some(self.snapshot.clone())
        }

        fn load_all_stopped(&mut self) -> Vec<Agent> {
            self.snapshot.stopped_agents.clone()
        }
    }

    fn make_agent(name: &str) -> Agent {
        crate::tui::test_helpers::make_test_agent(name, 120.0)
    }

    fn ok_response() -> Response {
        Response {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn test_app() -> App {
        let mut app = App::new();
        app.rpc_client = None;

        let mut data = DataState::empty();
        data.agents = vec![make_agent("nova")];
        app.data = data.clone();
        app.source = Box::new(TestSource::new(data));
        app
    }

    fn flash_text(app: &App) -> String {
        app.ui
            .flash
            .as_ref()
            .map(|f| f.text.clone())
            .unwrap_or_default()
    }

    #[test]
    fn execute_confirm_kill_orphan_missing_pid_sets_error_flash() {
        let mut app = test_app();
        app.execute_confirm(ConfirmAction::KillOrphan(4242));
        assert!(flash_text(&app).contains("no longer tracked"));
    }

    #[test]
    fn apply_rpc_result_kill_agent_success_clears_selection() {
        let mut app = test_app();
        app.ui.selected.insert("nova".into());

        app.apply_rpc_result(RpcResult {
            op: RpcOp::KillAgent {
                name: "nova".into(),
            },
            result: Ok(ok_response()),
        });

        assert!(!app.ui.selected.contains("nova"));
        assert!(flash_text(&app).contains("Killed nova"));
    }

    #[test]
    fn apply_rpc_result_command_inline_keeps_navigate_and_marks_eject() {
        let mut app = test_app();
        app.ui.view_mode = ViewMode::Inline;

        app.apply_rpc_result(RpcResult {
            op: RpcOp::Command { cmd: "list".into() },
            result: Ok(Response {
                exit_code: 0,
                stdout: "line1\nline2\n".into(),
                stderr: String::new(),
            }),
        });

        assert_eq!(app.ui.mode, InputMode::Navigate);
        assert!(app.ui.pending_eject_cmd);
        let cr = app.ui.command_result.as_ref().expect("command result");
        assert_eq!(cr.label, "list");
        assert_eq!(cr.output, vec!["line1".to_string(), "line2".to_string()]);
    }

    #[test]
    fn apply_rpc_result_command_vertical_enters_command_output() {
        let mut app = test_app();
        app.ui.view_mode = ViewMode::Vertical;
        app.ui.mode = InputMode::Navigate;

        app.apply_rpc_result(RpcResult {
            op: RpcOp::Command { cmd: "list".into() },
            result: Ok(Response {
                exit_code: 0,
                stdout: "ok\n".into(),
                stderr: String::new(),
            }),
        });

        assert_eq!(app.ui.mode, InputMode::CommandOutput);
        assert!(!app.ui.pending_eject_cmd);
    }

    #[test]
    fn apply_rpc_result_relay_toggle_updates_popup() {
        let mut app = test_app();
        app.data.relay_enabled = true;
        app.source = Box::new(TestSource::new(app.data.clone()));
        let mut popup = RelayPopupState::new(false);
        popup.toggling = true;
        app.ui.relay_popup = Some(popup);

        app.apply_rpc_result(RpcResult {
            op: RpcOp::RelayToggle { enable: true },
            result: Ok(ok_response()),
        });

        let popup = app.ui.relay_popup.as_ref().expect("relay popup");
        assert!(popup.enabled);
        assert!(!popup.toggling);
        assert!(flash_text(&app).contains("Relay enabled"));
    }
}
