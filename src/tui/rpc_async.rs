use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::tui::commands;
use crate::tui::model::Tool;
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
    },
    KillPid {
        pid: u32,
    },
    Tag {
        name: String,
        tag: String,
    },
    Project {
        name: String,
        project: String,
    },
    Launch {
        tool: Tool,
        count: u8,
        tag: String,
        project: String,
        headless: bool,
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
        let (tx_req, rx_req) = mpsc::channel::<RpcOp>();
        let (tx_res, rx_res) = mpsc::channel::<RpcResult>();

        thread::spawn(move || {
            for op in rx_req {
                let result = run_op(&op);
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

        RpcOp::Project { name, project } => commands::run_native(&[
            "config".into(),
            "-i".into(),
            name.clone(),
            "project".into(),
            project.clone(),
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

        RpcOp::KillAgent { name } => commands::run_native(&["kill".into(), name.clone()]),

        RpcOp::ForkAgent { name } => commands::run_native(&["f".into(), name.clone()]),

        RpcOp::KillPid { pid } => commands::run_native(&["kill".into(), pid.to_string()]),

        RpcOp::Launch {
            tool,
            count,
            tag,
            project,
            headless,
            terminal,
            prompt,
        } => {
            let argv = build_launch_argv(*tool, *count, tag, project, *headless, terminal, prompt);
            commands::run_native(&argv)
        }
    }
}
