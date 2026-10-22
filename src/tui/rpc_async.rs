use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::tool::Tool;
use crate::tui::commands;
use crate::tui::rpc::{self, Response, build_launch_argv};

pub enum RpcOp {
    Send {
        recipients: Vec<String>,
        body: String,
        intent: Option<String>,
        reply_to: Option<u64>,
    },
    KillAgent {
        name: String,
    },
    ForkAgent {
        name: String,
        silent: bool,
    },
    ResumeAgent {
        name: String,
        silent: bool,
    },
    KillPid {
        pid: u32,
    },
    Tag {
        name: String,
        tag: String,
    },
    Launch {
        settings: Box<crate::tui::model::LaunchState>,
        tool: Tool,
        count: u8,
        tag: String,
        headless: bool,
        headless_pty: bool,
        terminal: String,
        prompt: String,
    },
    RelayToggle {
        enable: bool,
    },
    RelayStatus,
    RelayNew,
    RelayConnect {
        token: String,
    },
    Command {
        cmd: String,
    },
    /// Answer a question waiting in an agent's terminal.
    AnswerPrompt {
        name: String,
        steps: Vec<
            crate::commands::term::ScreenStep<crate::tui::transcript_view::prompt::ScreenPrompt>,
        >,
    },
}

pub struct RpcResult {
    pub op: RpcOp,
    pub result: Result<Response, String>,
}

pub struct RpcClient {
    tx: Sender<RpcOp>,
    rx: Receiver<RpcResult>,
    pending: usize,
}

impl RpcClient {
    pub fn start() -> Self {
        Self::start_with(run_op)
    }

    pub(crate) fn start_with(
        run: impl Fn(&RpcOp) -> Result<Response, String> + Send + 'static,
    ) -> Self {
        let (tx_req, rx_req) = mpsc::channel::<RpcOp>();
        let (tx_res, rx_res) = mpsc::channel::<RpcResult>();

        thread::spawn(move || {
            for op in rx_req {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&op)))
                    .unwrap_or_else(|_| Err("hcom RPC worker operation panicked".into()));
                let _ = tx_res.send(RpcResult { op, result });
            }
        });

        Self {
            tx: tx_req,
            rx: rx_res,
            pending: 0,
        }
    }

    pub fn submit(&mut self, op: RpcOp) -> Result<(), String> {
        self.tx
            .send(op)
            .map_err(|e| format!("rpc queue send: {}", e))?;
        self.pending += 1;
        Ok(())
    }

    pub fn drain(&mut self) -> Vec<RpcResult> {
        let mut out = Vec::new();
        while let Ok(result) = self.rx.try_recv() {
            self.pending = self.pending.saturating_sub(1);
            out.push(result);
        }
        out
    }

    pub fn pending_count(&self) -> usize {
        self.pending
    }
}

fn run_op(op: &RpcOp) -> Result<Response, String> {
    match op {
        RpcOp::Send {
            recipients,
            body,
            intent,
            reply_to,
        } => {
            let mut argv: Vec<String> = vec!["send".into(), "--from".into(), "bigboss".into()];
            if let Some(i) = intent {
                argv.extend(["--intent".into(), i.clone()]);
            }
            if let Some(id) = reply_to {
                argv.extend(["--reply-to".into(), id.to_string()]);
            }
            for r in recipients {
                argv.push(format!("@{r}"));
            }
            argv.push("--".into());
            argv.push(body.clone());
            commands::run_native(&argv)
        }

        RpcOp::Tag { name, tag } => commands::run_native(&[
            "config".into(),
            "-i".into(),
            name.clone(),
            "tag".into(),
            tag.clone(),
        ]),

        RpcOp::RelayToggle { enable } => {
            let flag = if *enable { "on" } else { "off" };
            commands::run_native(&["relay".into(), flag.into()])
        }

        RpcOp::RelayStatus => commands::run_native(&["relay".into()]),

        RpcOp::RelayNew => commands::run_native(&["relay".into(), "new".into()]),

        RpcOp::RelayConnect { token } => {
            commands::run_native(&["relay".into(), "connect".into(), token.clone()])
        }

        RpcOp::Command { cmd } => {
            let argv = rpc::parse_command_argv(cmd)?;
            commands::run_native(&argv)
        }

        RpcOp::AnswerPrompt { name, steps } => {
            let db = crate::db::HcomDb::open().map_err(|e| e.to_string())?;
            let stdout = crate::commands::term::answer_on_screen(&db, name, steps)?;
            Ok(Response {
                exit_code: 0,
                stdout,
                stderr: String::new(),
            })
        }

        RpcOp::KillAgent { name } => commands::run_native(&["kill".into(), name.clone()]),

        RpcOp::ForkAgent { name, .. } => commands::run_native(&[
            "--go".into(),
            "f".into(),
            name.clone(),
            "--no-run-here".into(),
        ]),

        RpcOp::ResumeAgent { name, .. } => commands::run_native(&[
            "--go".into(),
            "r".into(),
            name.clone(),
            "--no-run-here".into(),
        ]),

        RpcOp::KillPid { pid } => commands::run_native(&["kill".into(), pid.to_string()]),

        RpcOp::Launch {
            tool,
            count,
            tag,
            headless,
            headless_pty,
            terminal,
            prompt,
            ..
        } => {
            let argv = build_launch_argv(
                *tool,
                *count,
                tag,
                *headless,
                *headless_pty,
                terminal,
                prompt,
            );
            commands::run_native(&argv)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_survives_panic_and_drains_pending() {
        let mut client = RpcClient::start_with(|op| {
            if matches!(op, RpcOp::RelayStatus) {
                panic!("test operation panic");
            }
            Ok(Response {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        });
        client.submit(RpcOp::RelayStatus).unwrap();
        client.submit(RpcOp::RelayNew).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut results = Vec::new();
        while client.pending_count() > 0 && std::time::Instant::now() < deadline {
            results.extend(client.drain());
            std::thread::yield_now();
        }
        assert_eq!(client.pending_count(), 0);
        assert_eq!(results.len(), 2);
        assert!(results[0].result.is_err());
        assert!(results[1].result.is_ok());
    }
}
