import type { ExtensionAPI, ExtensionContext, InputEvent } from "@oh-my-pi/pi-coding-agent";
import { appendFileSync, mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { dirname } from "node:path";
import { spawn } from "node:child_process";
import { createServer, type Server } from "node:net";

const HCOM_DIR = process.env.HCOM_DIR || `${homedir()}/.hcom`;
const LOG_PATH = `${HCOM_DIR}/.tmp/logs/hcom.log`;

type HcomResult = {
	code: number;
	stdout: string;
	stderr: string;
};

function log(
	level: "DEBUG" | "INFO" | "WARN" | "ERROR",
	event: string,
	instance?: string | null,
	extra?: Record<string, unknown>,
) {
	const entry = JSON.stringify({
		ts: new Date().toISOString().replace(/\.\d{3}Z$/, "Z"),
		level,
		subsystem: "plugin",
		event,
		...(instance ? { instance } : {}),
		...extra,
	});
	try {
		mkdirSync(dirname(LOG_PATH), { recursive: true });
		appendFileSync(LOG_PATH, `${entry}\n`);
	} catch {}
}

function hcom(args: string[]): Promise<HcomResult> {
	return new Promise((resolve) => {
		const child = spawn("hcom", args, { stdio: ["ignore", "pipe", "pipe"] });
		let stdout = "";
		let stderr = "";
		child.stdout.setEncoding("utf8");
		child.stderr.setEncoding("utf8");
		child.stdout.on("data", (chunk) => {
			stdout += chunk;
		});
		child.stderr.on("data", (chunk) => {
			stderr += chunk;
		});
		child.on("error", (error) => resolve({ code: 127, stdout, stderr: String(error) }));
		child.on("close", (code) => resolve({ code: code ?? 0, stdout, stderr }));
	});
}

function formatMessagesForInjection(messages: any[], recipientName: string): string {
	const parts = messages.map((m: any) => {
		const prefix = m.intent
			? m.thread
				? `[${m.intent}:${m.thread} #${m.event_id}]`
				: `[${m.intent} #${m.event_id}]`
			: m.thread
				? `[thread:${m.thread} #${m.event_id}]`
				: `[new message #${m.event_id}]`;
		return `${prefix} ${m.from} -> ${recipientName}: ${m.message}`;
	});
	if (messages.length === 1) return `<hcom>${parts[0]}</hcom>`;
	return `<hcom>[${messages.length} new messages] | ${parts.join(" | ")}</hcom>`;
}

function isBodylessWake(text: string): boolean {
	const trimmed = text.trim();
	return trimmed === "<hcom>" || trimmed === "<hcom></hcom>";
}

export default function hcomExtension(pi: ExtensionAPI) {
	let instanceName: string | null = null;
	let sessionId: string | null = null;
	let bootstrapText: string | null = null;
	let bindingPromise: Promise<void> | null = null;
	let notifyServer: Server | null = null;
	let notifyPort: number | null = null;
	let currentCtx: ExtensionContext | null = null;
	let pendingAckId: number | null = null;
	let deliveryInFlight = false;
	let reconcileTimer: ReturnType<typeof setInterval> | null = null;
	let reconcileInFlight = false;
	let bootstrapInjectedForSession: string | null = null;
	let lastReportedStatusKey: string | null = null;
	let lastPendingPollAt = 0;
	let agentActive = false;
	let idleTimer: ReturnType<typeof setTimeout> | null = null;

	const PENDING_POLL_MS = 60_000;
	const FALLBACK_PENDING_POLL_MS = 5_000;
	const IDLE_DEBOUNCE_MS = 250;

	function statusKey(status: string, context: string, detail: string): string {
		return `${status}\0${context}\0${detail}`;
	}

	function isBoundSession(candidateSessionId?: string | null): boolean {
		return !candidateSessionId || !sessionId || candidateSessionId === sessionId;
	}

	function startNotifyServer(): Promise<number | null> {
		if (notifyServer && notifyPort) return Promise.resolve(notifyPort);
		return new Promise((resolve) => {
			const server = createServer((socket) => {
				socket.end();
				log("DEBUG", "notify_server.wake", instanceName, { pending_ack: pendingAckId });
				if (currentCtx) void deliverPending(currentCtx);
			});
			server.on("error", (error) => {
				log("ERROR", "notify_server.start_failed", instanceName, { error: String(error) });
				resolve(null);
			});
			server.listen(0, "127.0.0.1", () => {
				notifyServer = server;
				const address = server.address();
				notifyPort = typeof address === "object" && address ? address.port : null;
				log("INFO", "notify_server.started", instanceName, { port: notifyPort });
				resolve(notifyPort);
			});
		});
	}

	function stopNotifyServer(): void {
		if (notifyServer) {
			try {
				notifyServer.close();
			} catch {}
		}
		notifyServer = null;
		notifyPort = null;
	}

	async function bindIdentity(ctx: ExtensionContext): Promise<void> {
		currentCtx = ctx;
		if (instanceName || bindingPromise) return bindingPromise ?? Promise.resolve();
		if (process.env.HCOM_LAUNCHED !== "1") return;
		bindingPromise = (async () => {
			try {
				const sid = ctx.sessionManager.getSessionId();
				const transcriptPath = ctx.sessionManager.getSessionFile();
				const port = await startNotifyServer();
				const args = ["omp-start", "--session-id", sid, "--cwd", ctx.cwd];
				if (transcriptPath) args.push("--transcript-path", transcriptPath);
				if (port) args.push("--notify-port", String(port));
				const result = await hcom(args);
				if (result.code !== 0) {
					stopNotifyServer();
					log("WARN", "plugin.bind_failed", null, { exit_code: result.code, stderr: result.stderr.slice(0, 300) });
					return;
				}
				const json = JSON.parse(result.stdout || "{}");
				if (json.error) {
					stopNotifyServer();
					log("WARN", "plugin.bind_failed", null, { error: json.error });
					return;
				}
				instanceName = json.name;
				sessionId = json.session_id || sid;
				bootstrapText = typeof json.bootstrap === "string" ? json.bootstrap : null;
				log("INFO", "plugin.bound", instanceName, {
					session_id: sessionId,
					notify_port: port,
					bootstrap_len: bootstrapText?.length ?? 0,
				});
			} catch (error) {
				stopNotifyServer();
				log("ERROR", "plugin.bind_error", null, { error: String(error) });
			} finally {
				bindingPromise = null;
			}
		})();
		await bindingPromise;
	}

	async function fetchPending(): Promise<{ messages: any[]; maxId: number } | null> {
		if (!instanceName) return null;
		const result = await hcom(["omp-read", "--name", instanceName]);
		if (result.code !== 0) {
			log("WARN", "plugin.delivery_read_failed", instanceName, { exit_code: result.code, stderr: result.stderr.slice(0, 300) });
			return null;
		}
		let messages: any[] = [];
		try {
			messages = JSON.parse(result.stdout || "[]");
		} catch (error) {
			log("WARN", "plugin.delivery_parse_failed", instanceName, { error: String(error), raw: result.stdout.slice(0, 300) });
			return null;
		}
		if (!Array.isArray(messages) || messages.length === 0) return null;
		const maxId = Math.max(...messages.map((m: any) => m.event_id || 0));
		if (maxId <= 0) return null;
		return { messages, maxId };
	}

	async function deliverPending(ctx: ExtensionContext): Promise<boolean> {
		currentCtx = ctx;
		await bindIdentity(ctx);
		if (!instanceName || !sessionId) return false;
		if (!isBoundSession(ctx.sessionManager.getSessionId())) return false;
		if (deliveryInFlight || pendingAckId !== null) return false;
		deliveryInFlight = true;
		try {
			const pending = await fetchPending();
			if (!pending) return false;
			const formatted = formatMessagesForInjection(pending.messages, instanceName);
			pendingAckId = pending.maxId;
			try {
				const isIdle = ctx.isIdle();
				if (isIdle) {
					await pi.sendUserMessage(formatted);
				} else {
					await pi.sendUserMessage(formatted, { deliverAs: "followUp" });
				}
				const sender = String(pending.messages[0]?.from ?? "");
				await reportStatus(ctx, "active", sender ? `deliver:${sender}` : "deliver");
				log("INFO", "plugin.delivery_pending", instanceName, {
					count: pending.messages.length,
					pending_ack: pending.maxId,
					idle: isIdle,
				});
				await ackPending(isIdle ? "sendUserMessage" : "followUp");
				return true;
			} catch (error) {
				if (pendingAckId === pending.maxId) pendingAckId = null;
				log("ERROR", "plugin.delivery_send_failed", instanceName, { error: String(error) });
				return false;
			}
		} finally {
			deliveryInFlight = false;
		}
	}

	async function ackPending(source: string): Promise<void> {
		if (!instanceName || pendingAckId === null) return;
		const ackId = pendingAckId;
		pendingAckId = null;
		await hcom(["omp-read", "--name", instanceName, "--ack", "--up-to", String(ackId)]);
		log("INFO", "plugin.deferred_ack", instanceName, { acked_to: ackId, source });
	}

	async function reportStatus(ctx: ExtensionContext, status: "active" | "listening", context = "", detail = ""): Promise<void> {
		await bindIdentity(ctx);
		if (!instanceName) return;
		const args = ["omp-status", "--name", instanceName, "--status", status];
		if (context) args.push("--context", context);
		if (detail) args.push("--detail", detail);
		await hcom(args);
		lastReportedStatusKey = statusKey(status, context, detail);
	}

	async function reportReconciledStatus(ctx: ExtensionContext): Promise<void> {
		const key = statusKey("listening", "", "");
		if (lastReportedStatusKey !== key) {
			await reportStatus(ctx, "listening");
		}
	}

	async function pollPendingIfDue(ctx: ExtensionContext): Promise<void> {
		const now = Date.now();
		const interval = notifyPort ? PENDING_POLL_MS : FALLBACK_PENDING_POLL_MS;
		if (now - lastPendingPollAt < interval) return;
		lastPendingPollAt = now;
		await deliverPending(ctx);
	}

	function clearIdleTimer(): void {
		if (idleTimer) clearTimeout(idleTimer);
		idleTimer = null;
	}

	async function reconcile(): Promise<void> {
		if (reconcileInFlight || !currentCtx || !instanceName) return;
		reconcileInFlight = true;
		try {
			if (currentCtx.isIdle()) {
				await reportReconciledStatus(currentCtx);
				await pollPendingIfDue(currentCtx);
			}
		} catch (error) {
			log("ERROR", "plugin.reconcile_error", instanceName, { error: String(error) });
		} finally {
			reconcileInFlight = false;
		}
	}

	function startReconcileTimer(): void {
		if (!reconcileTimer) reconcileTimer = setInterval(() => void reconcile(), 5_000);
	}

	function resetBinding(): void {
		stopNotifyServer();
		instanceName = null;
		sessionId = null;
		bootstrapText = null;
		bindingPromise = null;
		pendingAckId = null;
		deliveryInFlight = false;
		bootstrapInjectedForSession = null;
		lastReportedStatusKey = null;
		lastPendingPollAt = 0;
		agentActive = false;
		clearIdleTimer();
	}

	pi.on("session_start", async (_event, ctx) => {
		currentCtx = ctx;
		resetBinding();
		await bindIdentity(ctx);
		startReconcileTimer();
	});

	pi.on("session_shutdown", async (event) => {
		if (instanceName) {
			await hcom(["omp-stop", "--name", instanceName, "--reason", event.reason ?? "shutdown"]);
		}
		resetBinding();
	});

	pi.on("agent_start", async (_event, ctx) => {
		currentCtx = ctx;
		clearIdleTimer();
		agentActive = true;
		await reportStatus(ctx, "active", "agent");
	});

	pi.on("input", async (event: InputEvent, ctx) => {
		currentCtx = ctx;
		await bindIdentity(ctx);
		if (!instanceName) return { action: "continue" };
		if (event.source === "extension") {
			await ackPending(event.streamingBehavior ?? "extension");
			return { action: "continue" };
		}
		if (isBodylessWake(event.text) && pendingAckId === null) {
			const pending = await fetchPending();
			if (pending) {
				pendingAckId = pending.maxId;
				return { action: "transform", text: formatMessagesForInjection(pending.messages, instanceName) };
			}
			return { action: "handled" };
		}
		await reportStatus(ctx, "active", event.text.trim() === "<hcom>" ? "trigger" : "prompt");
		return { action: "continue" };
	});

	pi.on("before_agent_start", async (_event, ctx) => {
		currentCtx = ctx;
		await bindIdentity(ctx);
		if (!instanceName || !bootstrapText) return undefined;
		const sid = ctx.sessionManager.getSessionId();
		if (bootstrapInjectedForSession === sid) return undefined;
		bootstrapInjectedForSession = sid;
		log("DEBUG", "plugin.hidden_bootstrap", instanceName, { bootstrap_len: bootstrapText.length });
		return {
			message: {
				customType: "hcom-bootstrap",
				content: bootstrapText,
				display: false,
			},
		};
	});

	pi.on("tool_call", async (event, ctx) => {
		currentCtx = ctx;
		await bindIdentity(ctx);
		if (!instanceName) return undefined;
		await reportStatus(ctx, "active", `tool:${event.toolName}`, String((event.input as any)?.path ?? (event.input as any)?.command ?? ""));
		const result = await hcom([
			"omp-beforetool",
			"--name",
			instanceName,
			"--tool",
			event.toolName,
			"--input-json",
			JSON.stringify(event.input ?? {}),
		]);
		try {
			const json = JSON.parse(result.stdout || "{}");
			if (json.decision === "block") {
				return { block: true, reason: String(json.reason || "Blocked by hcom") };
			}
		} catch {}
		return undefined;
	});

	pi.on("tool_result", async (event, ctx) => {
		currentCtx = ctx;
		await reportStatus(ctx, "active", `tool:${event.toolName}`);
		await deliverPending(ctx);
	});

	pi.on("turn_end", async (_event, ctx) => {
		currentCtx = ctx;
		await deliverPending(ctx);
	});

	pi.on("agent_end", async (_event, ctx) => {
		currentCtx = ctx;
		if (!agentActive) return;
		agentActive = false;
		clearIdleTimer();
		idleTimer = setTimeout(() => {
			idleTimer = null;
			if (currentCtx?.isIdle()) {
				void (async () => {
					await reportStatus(currentCtx, "listening");
					await deliverPending(currentCtx);
				})();
			}
		}, IDLE_DEBOUNCE_MS);
		idleTimer.unref?.();
	});
}
