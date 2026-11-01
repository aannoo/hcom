import type { ExtensionAPI, ExtensionContext, InputEvent } from "@earendil-works/pi-coding-agent";
import { appendFileSync, mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { dirname } from "node:path";
import { spawn } from "node:child_process";
import { createServer, type Server } from "node:net";

const HCOM_DIR = process.env.HCOM_DIR || `${homedir()}/.hcom`;
const LOG_PATH = `${HCOM_DIR}/.tmp/logs/hcom.log`;

const HCOM_TIMEOUT_MS = 5_000;

type HcomResult = {
	code: number;
	stdout: string;
	stderr: string;
};

type RpcReply = Record<string, unknown>;

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

// Exit code is 124 on timeout and 128+ when hcom dies from a signal, so neither
// can be mistaken for success.
function hcom(args: string[]): Promise<HcomResult> {
	return new Promise((resolve) => {
		const child = spawn("hcom", args, { stdio: ["ignore", "pipe", "pipe"] });
		let stdout = "";
		let stderr = "";
		let settled = false;
		const finish = (code: number) => {
			if (settled) return;
			settled = true;
			clearTimeout(timer);
			resolve({ code, stdout, stderr });
		};
		const timer = setTimeout(() => {
			stderr ||= `timed out after ${HCOM_TIMEOUT_MS}ms`;
			try {
				child.kill("SIGKILL");
			} catch {}
			finish(124);
		}, HCOM_TIMEOUT_MS);
		timer.unref?.();
		child.stdout.setEncoding("utf8");
		child.stderr.setEncoding("utf8");
		child.stdout.on("data", (chunk) => {
			stdout += chunk;
		});
		child.stderr.on("data", (chunk) => {
			stderr += chunk;
		});
		child.on("error", (error) => {
			stderr ||= String(error);
			finish(127);
		});
		child.on("close", (code, signal) => {
			if (code === null) stderr ||= `killed by ${signal ?? "signal"}`;
			finish(code ?? 129);
		});
	});
}

// Every hcom plugin RPC exits 0 and answers with JSON: a payload on success or
// `{"error": ...}`. Returns the payload, or null (logged) for any failure.
async function rpc(args: string[], instance: string | null): Promise<RpcReply | null> {
	const result = await hcom(args);
	const fail = (extra: Record<string, unknown>) => {
		log("WARN", "plugin.rpc_failed", instance, { command: args[0], ...extra });
		return null;
	};
	if (result.code !== 0) return fail({ exit_code: result.code, stderr: result.stderr.slice(0, 300) });
	let reply: unknown;
	try {
		reply = JSON.parse(result.stdout);
	} catch {
		return fail({ raw: result.stdout.slice(0, 300) });
	}
	if (!reply || typeof reply !== "object" || Array.isArray(reply)) return fail({ raw: result.stdout.slice(0, 300) });
	if ("error" in reply) return fail({ error: (reply as RpcReply).error });
	return reply as RpcReply;
}

type Batch = { messages: any[]; ackId: number; hasMore: boolean };

// Repeat the self-minted `--process-id` on every follow-up RPC: the Rust side
// resolves the owner from the flag first, so a launchless session keeps read/ack
// authorized without inheriting HCOM_PROCESS_ID.
// Set when this session minted its own binding key (`--process-id`). Read/ack
// RPCs repeat it so a launchless session stays authorized after the start.
let mintedProcessId: string | null = null;

function withProcessId(args: string[], minted: string | null = mintedProcessId): string[] {
	return minted ? [...args, "--process-id", minted] : args;
}

// The oldest unread messages (hcom bounds the batch), or null when the read failed.
async function readBatch(instance: string): Promise<Batch | null> {
	const reply = await rpc(withProcessId(["pi-read", "--name", instance], mintedProcessId), instance);
	if (!reply) return null;
	const messages = reply.messages;
	const valid =
		Array.isArray(messages) &&
		messages.every((m: any, i: number) => Number.isInteger(m?.event_id) && m.event_id > (i ? messages[i - 1].event_id : 0));
	if (!valid) {
		log("WARN", "plugin.rpc_failed", instance, { command: "pi-read", error: "malformed batch" });
		return null;
	}
	return { messages, ackId: messages.at(-1)?.event_id ?? 0, hasMore: reply.has_more === true };
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

function messageText(message: any): string {
	const content = message?.content;
	if (typeof content === "string") return content;
	if (!Array.isArray(content)) return "";
	return content
		.filter((part: any) => part?.type === "text" && typeof part.text === "string")
		.map((part: any) => part.text)
		.join("\n");
}

function startNotifyServer(onWake: () => void): Promise<Server | null> {
	return new Promise((resolve) => {
		const server = createServer((socket) => {
			socket.end();
			onWake();
		});
		server.on("error", (error) => {
			log("ERROR", "notify_server.start_failed", null, { error: String(error) });
			resolve(null);
		});
		server.listen(0, "127.0.0.1", () => resolve(server));
	});
}

function serverPort(server: Server | null): number | null {
	const address = server?.address();
	return typeof address === "object" && address ? address.port : null;
}

// A batch handed to Pi. It is acknowledged only once Pi admits it to the
// transcript (its user message ends), so a prompt Pi rejects, or a queued
// delivery dropped by an abort, stays unread and is delivered again.
type Delivery = { text: string; ackId: number; hasMore: boolean; sentAt: number; admitted: boolean };

// How long an idle Pi with nothing queued may take to admit a delivery before it
// is presumed rejected and released for redelivery.
const ADMIT_GRACE_MS = 10_000;
const RECONCILE_MS = 5_000;
const PENDING_POLL_MS = 60_000;
const FALLBACK_PENDING_POLL_MS = 5_000;

export default function hcomExtension(pi: ExtensionAPI) {
	let instanceName: string | null = null;
	let sessionId: string | null = null;
	let bootstrapText: string | null = null;
	let bindingPromise: Promise<void> | null = null;
	// Bumped on every reset. Async work started under an older generation must
	// not touch state after it resumes.
	let generation = 0;
	let notifyServer: Server | null = null;
	let currentCtx: ExtensionContext | null = null;
	let delivery: Delivery | null = null;
	let deliveryInFlight = false; // reading a batch / handing it to Pi
	let deliveryPending = false; // a wake arrived while delivery was gated; replay it once clear
	let deliveryRetryScheduled = false; // dedup the queued replay pass
	let ackInFlight: Promise<void> | null = null;
	let statusChain: Promise<void> = Promise.resolve();
	let reconcileTimer: ReturnType<typeof setInterval> | null = null;
	let reconcileInFlight = false;
	let bootstrapInjectedForSession: string | null = null;
	let lastReportedStatusKey: string | null = null;
	let lastPendingPollAt = 0;

	function statusKey(status: string, context: string, detail: string): string {
		return `${status}\0${context}\0${detail}`;
	}

	function isBoundSession(candidateSessionId?: string | null): boolean {
		return !candidateSessionId || !sessionId || candidateSessionId === sessionId;
	}

	function stopNotifyServer(): void {
		try {
			notifyServer?.close();
		} catch {}
		notifyServer = null;
	}

	async function bindIdentity(ctx: ExtensionContext): Promise<void> {
		currentCtx = ctx;
		if (instanceName || bindingPromise) return bindingPromise ?? Promise.resolve();
		// A session opened by the user can attach when the operator opts in; the
		// launcher path stays the default.
		const isLaunched = process.env.HCOM_LAUNCHED === "1";
		const autoAttach = isLaunched || process.env.HCOM_AUTO_ATTACH === "1";
		if (!autoAttach) return;
		const gen = generation;
		const attempt = (async () => {
			let server: Server | null = null;
			try {
				const sid = ctx.sessionManager.getSessionId();
				const transcriptPath = ctx.sessionManager.getSessionFile();
				server = await startNotifyServer(() => {
					log("DEBUG", "notify_server.wake", instanceName, { pending_ack: delivery?.ackId ?? null });
					if (currentCtx) void deliverPending(currentCtx);
				});
				const port = serverPort(server);
				const args = ["pi-start", "--session-id", sid, "--cwd", ctx.cwd];
				if (transcriptPath) args.push("--transcript-path", transcriptPath);
				if (port) args.push("--notify-port", String(port));
				// Not launched by hcom: mint the binding key and persist the pid, so
				// stale cleanup spares this session.
				if (!isLaunched) {
					mintedProcessId = `pi:${process.pid}:${sid}`;
					args.push("--process-id", mintedProcessId);
					args.push("--pid", String(process.pid));
				}
				const reply = await rpc(args, null);
				if (gen !== generation) return;
				if (!reply || typeof reply.name !== "string" || !reply.name) {
					log("WARN", "plugin.bind_failed", null, { reply });
					return;
				}
				notifyServer = server;
				server = null;
				instanceName = reply.name;
				sessionId = typeof reply.session_id === "string" && reply.session_id ? reply.session_id : sid;
				bootstrapText = typeof reply.bootstrap === "string" ? reply.bootstrap : null;
				startReconcileTimer();
				log("INFO", "plugin.bound", instanceName, {
					session_id: sessionId,
					notify_port: port,
					bootstrap_len: bootstrapText?.length ?? 0,
				});
			} catch (error) {
				log("ERROR", "plugin.bind_error", null, { error: String(error) });
			} finally {
				// Not adopted (failed, or reset while binding): don't leak the listener.
				server?.close();
				if (gen === generation) bindingPromise = null;
			}
		})();
		bindingPromise = attempt;
		await attempt;
	}

	async function deliverPending(ctx: ExtensionContext): Promise<void> {
		currentCtx = ctx;
		await bindIdentity(ctx);
		const name = instanceName;
		const gen = generation;
		if (!name || !sessionId) return;
		if (!isBoundSession(ctx.sessionManager.getSessionId())) return;
		if (deliveryInFlight || delivery) {
			// Drop nothing: record the wake so it is replayed once the outstanding
			// delivery is acknowledged or released.
			deliveryPending = true;
			log("DEBUG", "plugin.delivery_skipped", name, {
				reason: deliveryInFlight ? "delivery_in_flight" : "awaiting_admission",
				pending_ack: delivery?.ackId ?? null,
			});
			return;
		}
		deliveryInFlight = true;
		try {
			const batch = await readBatch(name);
			if (gen !== generation) return;
			if (!batch) {
				lastPendingPollAt = 0; // failed read: the next reconcile retries
				return;
			}
			if (batch.messages.length === 0) return;
			const text = formatMessagesForInjection(batch.messages, name);
			const idle = ctx.isIdle();
			const sent: Delivery = { text, ackId: batch.ackId, hasMore: batch.hasMore, sentAt: Date.now(), admitted: false };
			delivery = sent;
			try {
				// Returns void; Pi reports a rejected prompt only to its own error
				// handler. Admission is observed in message_end instead.
				// Steering reaches the next request after the running tools finish.
				pi.sendUserMessage(text, idle ? undefined : { deliverAs: "steer" });
			} catch (error) {
				if (delivery === sent) delivery = null;
				log("ERROR", "plugin.delivery_send_failed", name, { error: String(error) });
				return;
			}
			log("INFO", "plugin.delivery_sent", name, { count: batch.messages.length, ack_id: batch.ackId, idle });
			const sender = String(batch.messages[0]?.from ?? "");
			void reportStatus(ctx, "active", sender ? `deliver:${sender}` : "deliver");
		} finally {
			if (gen === generation) {
				deliveryInFlight = false;
				drainPendingDelivery("delivery_in_flight_wake");
			}
		}
	}

	// Replay a wake that was queued while delivery was gated. The microtask +
	// dedup flag collapse a burst of queued wakes into one pass.
	function schedulePendingDelivery(reason: string): void {
		if (deliveryRetryScheduled) return;
		deliveryRetryScheduled = true;
		log("DEBUG", "plugin.delivery_retry_scheduled", instanceName, { reason });
		queueMicrotask(() => {
			deliveryRetryScheduled = false;
			if (!instanceName || !currentCtx) return;
			void deliverPending(currentCtx);
		});
	}

	function drainPendingDelivery(reason: string): void {
		if (deliveryPending && !deliveryInFlight && !delivery) {
			deliveryPending = false;
			schedulePendingDelivery(reason);
		}
	}

	// Acknowledge an admitted delivery. Serialized; on failure the delivery stays
	// outstanding (gating redelivery) and reconcile retries.
	function flushAck(source: string): Promise<void> {
		if (ackInFlight) return ackInFlight;
		const sent = delivery;
		const name = instanceName;
		const gen = generation;
		if (!sent?.admitted || !name) return Promise.resolve();
		const attempt = (async () => {
			const reply = await rpc(withProcessId(["pi-read", "--name", name, "--ack", "--up-to", String(sent.ackId)], mintedProcessId), name);
			if (gen !== generation || delivery !== sent) return;
			if (reply?.acked_to !== sent.ackId) {
				if (reply) log("WARN", "plugin.rpc_failed", name, { command: "pi-read", error: "unexpected ack reply", reply });
				return;
			}
			delivery = null;
			log("INFO", "plugin.delivery_acked", name, { acked_to: sent.ackId, source, has_more: sent.hasMore });
			if (sent.hasMore) deliveryPending = true;
			drainPendingDelivery("post_ack_wake");
		})().finally(() => {
			if (gen === generation) ackInFlight = null;
		});
		ackInFlight = attempt;
		return attempt;
	}

	// Pi reports a rejected prompt (no model, no auth, compaction running) only to
	// its error handler, and an abort can drop a queued delivery. Either way Pi
	// goes idle with the delivery never admitted: release it so it is redelivered.
	function releaseUnadmitted(ctx: ExtensionContext, reason: string): void {
		const sent = delivery;
		if (!sent || sent.admitted || deliveryInFlight) return;
		if (!ctx.isIdle() || ctx.hasPendingMessages()) return;
		if (Date.now() - sent.sentAt < ADMIT_GRACE_MS) return;
		delivery = null;
		log("WARN", "plugin.delivery_not_admitted", instanceName, { ack_id: sent.ackId, reason });
		// Retry on the next wake or poll rather than at once, so a prompt that
		// keeps being rejected (e.g. missing auth) is not resent in a tight loop.
		deliveryPending = false;
		lastPendingPollAt = Date.now();
	}

	// Status writes run one at a time so they land in call order, and the cache
	// only records a write hcom confirmed.
	function queueStatus(args: string[], key: string): Promise<void> {
		const gen = generation;
		const run = statusChain.then(async () => {
			const name = instanceName;
			if (!name || gen !== generation) return;
			const reply = await rpc([args[0], "--name", name, ...args.slice(1)], name);
			if (reply && gen === generation) lastReportedStatusKey = key;
		});
		statusChain = run.catch(() => {});
		return run;
	}

	function reportStatus(ctx: ExtensionContext, status: "active" | "listening", context = "", detail = ""): Promise<void> {
		currentCtx = ctx;
		if (!instanceName) return Promise.resolve();
		const args = ["pi-status", "--status", status];
		if (context) args.push("--context", context);
		if (detail) args.push("--detail", detail);
		return queueStatus(args, statusKey(status, context, detail));
	}

	async function reportReconciledStatus(ctx: ExtensionContext): Promise<void> {
		if (lastReportedStatusKey !== statusKey("listening", "", "")) {
			await reportStatus(ctx, "listening");
		}
	}

	async function pollPendingIfDue(ctx: ExtensionContext): Promise<void> {
		const now = Date.now();
		const interval = notifyServer ? PENDING_POLL_MS : FALLBACK_PENDING_POLL_MS;
		if (now - lastPendingPollAt < interval) return;
		lastPendingPollAt = now;
		await deliverPending(ctx);
	}

	// Fallback for missed events and failed RPCs: retry acknowledgement, release
	// deliveries Pi dropped, and when idle restore listening and poll for mail.
	async function reconcile(): Promise<void> {
		const ctx = currentCtx;
		if (reconcileInFlight || !ctx || !instanceName) return;
		reconcileInFlight = true;
		try {
			if (delivery?.admitted) await flushAck("reconcile");
			releaseUnadmitted(ctx, "reconcile");
			if (ctx.isIdle()) {
				await reportReconciledStatus(ctx);
				await pollPendingIfDue(ctx);
			}
		} catch (error) {
			log("ERROR", "plugin.reconcile_error", instanceName, { error: String(error) });
		} finally {
			reconcileInFlight = false;
		}
	}

	function startReconcileTimer(): void {
		stopReconcileTimer();
		reconcileTimer = setInterval(() => void reconcile(), RECONCILE_MS);
		reconcileTimer.unref?.();
	}

	function stopReconcileTimer(): void {
		if (reconcileTimer) clearInterval(reconcileTimer);
		reconcileTimer = null;
	}

	function resetBinding(): void {
		generation++;
		stopReconcileTimer();
		stopNotifyServer();
		instanceName = null;
		sessionId = null;
		bootstrapText = null;
		bindingPromise = null;
		delivery = null;
		deliveryInFlight = false;
		deliveryPending = false;
		deliveryRetryScheduled = false;
		ackInFlight = null;
		statusChain = Promise.resolve();
		reconcileInFlight = false;
		bootstrapInjectedForSession = null;
		lastReportedStatusKey = null;
		lastPendingPollAt = 0;
	}

	pi.on("session_start", async (_event, ctx) => {
		currentCtx = ctx;
		resetBinding();
		await bindIdentity(ctx);
	});

	pi.on("session_shutdown", async (event) => {
		// Only "quit" ends the process. reload/new/resume/fork are followed by
		// session_start (in a fresh extension runtime for reload), whose pi-start
		// rebinds this identity via the process binding — stopping here would
		// delete the live instance. Every reason still releases this runtime's
		// timer and listener.
		const name = instanceName;
		const terminal = event.reason === undefined || event.reason === "quit";
		resetBinding();
		if (name && terminal) {
			await rpc(["pi-stop", "--name", name, "--reason", event.reason ?? "shutdown"], name);
		}
	});

	pi.on("agent_start", async (_event, ctx) => {
		await reportStatus(ctx, "active", "agent");
	});

	pi.on("input", async (event: InputEvent, ctx) => {
		currentCtx = ctx;
		await bindIdentity(ctx);
		const name = instanceName;
		const gen = generation;
		if (!name || event.source === "extension") return { action: "continue" };
		if (isBodylessWake(event.text)) {
			// A PTY wake: replace it with the pending batch, or swallow it when
			// there is none or a delivery is already outstanding.
			if (deliveryInFlight || delivery) {
				deliveryPending = true;
				return { action: "handled" };
			}
			deliveryInFlight = true;
			try {
				const batch = await readBatch(name);
				if (gen !== generation) return { action: "handled" };
				if (!batch) lastPendingPollAt = 0;
				if (!batch?.messages.length) return { action: "handled" };
				const text = formatMessagesForInjection(batch.messages, name);
				delivery = { text, ackId: batch.ackId, hasMore: batch.hasMore, sentAt: Date.now(), admitted: false };
				log("INFO", "plugin.delivery_sent", name, { count: batch.messages.length, ack_id: batch.ackId, via: "wake" });
				return { action: "transform", text };
			} finally {
				if (gen === generation) deliveryInFlight = false;
			}
		}
		await reportStatus(ctx, "active", "prompt");
		return { action: "continue" };
	});

	// The delivery's user message reaching the transcript is the receipt: Pi
	// persists it right after this handler, whether it was sent directly, queued
	// as steering, or substituted for a bare <hcom> wake.
	pi.on("message_end", (event) => {
		const sent = delivery;
		if (!sent || sent.admitted || event.message.role !== "user") return;
		if (!messageText(event.message).includes(sent.text)) return;
		sent.admitted = true;
		log("DEBUG", "plugin.delivery_admitted", instanceName, { ack_id: sent.ackId });
		void flushAck("message_end");
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

	// Observation only: record the tool as current activity, never block it.
	pi.on("tool_call", (event, ctx) => {
		currentCtx = ctx;
		if (!instanceName) return undefined;
		void queueStatus(
			["pi-beforetool", "--tool", event.toolName, "--input-json", JSON.stringify(event.input ?? {})],
			statusKey("active", `tool:${event.toolName}`, ""),
		);
		return undefined;
	});

	pi.on("tool_result", async (_event, ctx) => {
		await deliverPending(ctx);
	});

	pi.on("turn_end", async (_event, ctx) => {
		await deliverPending(ctx);
	});

	// agent_settled: Pi has no retry, compaction recovery, or queued work left
	// for this run. The reconcile timer covers a missed event.
	pi.on("agent_settled", async (_event, ctx) => {
		currentCtx = ctx;
		if (!ctx.isIdle()) return;
		await reportStatus(ctx, "listening");
		releaseUnadmitted(ctx, "agent_settled");
		await deliverPending(ctx);
	});
}
