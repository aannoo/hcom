use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::tui::model::Tool;
use crate::tui::rpc::{self, Response};

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
    Launch {
        tool: Tool,
        count: u8,
        tag: String,
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
            let refs: Vec<&str> = recipients.iter().map(String::as_str).collect();
            rpc::rpc_send(&refs, body, intent.as_deref(), *reply_to)
        }
        RpcOp::KillAgent { name } => rpc::rpc_kill(name),
        RpcOp::ForkAgent { name } => rpc::rpc_fork(name),
        RpcOp::KillPid { pid } => rpc::rpc_kill_pid(*pid),
        RpcOp::Tag { name, tag } => rpc::rpc_tag(name, tag),
        RpcOp::Launch {
            tool,
            count,
            tag,
            headless,
            terminal,
            prompt,
        } => rpc::rpc_launch(*tool, *count, tag, *headless, terminal, prompt),
        RpcOp::RelayToggle { enable } => rpc::rpc_relay_toggle(*enable),
        RpcOp::RelayStatus => rpc::rpc_relay_status(),
        RpcOp::RelayNew => rpc::rpc_relay_new(),
        RpcOp::RelayConnect { token } => rpc::rpc_relay_connect(token),
        RpcOp::Command { cmd } => rpc::rpc_command(cmd),
    }
}
