// hcom-managed-plugin: installed by hcom; removed automatically. Do not add this line to your own plugins.
import type { Plugin as Plugin2 } from "@opencode/plugin"
import { spawn } from "node:child_process"
import { appendFileSync, mkdirSync } from "node:fs"
import { createServer, type Server } from "node:net"
import { homedir } from "node:os"
import { dirname } from "node:path"

const HCOM_DIR = process.env.HCOM_DIR || `${homedir()}/.hcom`
const LOG_PATH = `${HCOM_DIR}/.tmp/logs/hcom.log`
const HCOM_TIMEOUT_MS = 15_000
const RECONCILE_MS = 5_000
// How long a delivery may stay unadmitted while the session is idle before it
// is presumed rejected and released for redelivery.
const ADMIT_GRACE_MS = 10_000
// Consecutive unadmitted deliveries redelivered without waiting for a wake.
const MAX_UNADMITTED_RETRIES = 3

function claimPluginHost(): boolean {
  const owner = process.env.HCOM_PLUGIN_HOST_PID
  const current = String(process.pid)
  if (owner && owner !== current) return false
  process.env.HCOM_PLUGIN_HOST_PID = current
  return true
}

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
  })
  try {
    mkdirSync(dirname(LOG_PATH), { recursive: true })
    appendFileSync(LOG_PATH, entry + "\n")
  } catch {}
}

type HcomResult = { code: number; stdout: string; stderr: string }

// Exit code is 124 on timeout and 128+ when hcom dies from a signal, so neither
// can be mistaken for success.
function hcom(args: string[]): Promise<HcomResult> {
  return new Promise((resolve) => {
    const child = spawn("hcom", args, { stdio: ["ignore", "pipe", "pipe"] })
    let stdout = ""
    let stderr = ""
    let settled = false
    const finish = (code: number) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      resolve({ code, stdout, stderr })
    }
    const timer = setTimeout(() => {
      stderr ||= `timed out after ${HCOM_TIMEOUT_MS}ms`
      try { child.kill("SIGKILL") } catch {}
      finish(124)
    }, HCOM_TIMEOUT_MS)
    timer.unref?.()
    child.stdout.setEncoding("utf8")
    child.stderr.setEncoding("utf8")
    child.stdout.on("data", (chunk) => { stdout += chunk })
    child.stderr.on("data", (chunk) => { stderr += chunk })
    child.on("error", (error) => {
      stderr ||= String(error)
      finish(127)
    })
    child.on("close", (code, signal) => {
      if (code === null) stderr ||= `killed by ${signal ?? "signal"}`
      finish(code ?? 129)
    })
  })
}

type RpcReply = Record<string, any>

// Every hcom plugin RPC exits 0 and answers with JSON: a payload on success or
// `{"error": ...}`. Returns the payload, or null (logged) for any failure.
async function rpc(args: string[], instance: string | null): Promise<RpcReply | null> {
  const result = await hcom(args)
  const fail = (extra: Record<string, unknown>) => {
    log("WARN", "plugin.rpc_failed", instance, { command: args[0], ...extra })
    return null
  }
  if (result.code !== 0) return fail({ exit_code: result.code, stderr: result.stderr.slice(0, 300) })
  let reply: unknown
  try {
    reply = JSON.parse(result.stdout)
  } catch {
    return fail({ raw: result.stdout.slice(0, 300) })
  }
  if (!reply || typeof reply !== "object" || Array.isArray(reply)) return fail({ raw: result.stdout.slice(0, 300) })
  if ("error" in reply) return fail({ error: (reply as RpcReply).error })
  return reply as RpcReply
}

type Batch = { messages: any[]; ackId: number; hasMore: boolean }

// The oldest unread messages (hcom bounds the batch), or null when the read failed.
async function readBatch(instance: string): Promise<Batch | null> {
  const reply = await rpc(["opencode-read", "--name", instance], instance)
  if (!reply) return null
  const messages = reply.messages
  const valid =
    Array.isArray(messages) &&
    messages.every((m: any, i: number) => Number.isInteger(m?.event_id) && m.event_id > (i ? messages[i - 1].event_id : 0))
  if (!valid) {
    log("WARN", "plugin.rpc_failed", instance, { command: "opencode-read", error: "malformed batch" })
    return null
  }
  return { messages, ackId: messages.at(-1)?.event_id ?? 0, hasMore: reply.has_more === true }
}

function formatMessagesForInjection(messages: any[], recipientName: string): string {
  const parts = messages.map((m: any) => {
    const prefix = m.intent
      ? m.thread
        ? `[${m.intent}:${m.thread} #${m.event_id}]`
        : `[${m.intent} #${m.event_id}]`
      : m.thread
        ? `[thread:${m.thread} #${m.event_id}]`
        : `[new message #${m.event_id}]`
    return `${prefix} ${m.from} -> ${recipientName}: ${m.message}`
  })
  if (messages.length === 1) return `<hcom>${parts[0]}</hcom>`
  return `<hcom>[${messages.length} new messages] | ${parts.join(" | ")}</hcom>`
}

// The PTY bootstrap types a body-free wake (`<hcom>`, or an envelope preview
// with no `sender -> recipient: body`) because the TUI input box cannot safely
// hold arbitrary message text. The plugin swaps in the real messages.
function isWakeText(text: string): boolean {
  const trimmed = text.trim()
  if (trimmed === "<hcom>" || trimmed === "<hcom></hcom>") return true
  return trimmed.startsWith("<hcom>") && trimmed.endsWith("</hcom>") && !trimmed.includes(": ")
}

function startNotifyServer(onWake: () => void): Promise<Server | null> {
  return new Promise((resolve) => {
    const server = createServer((socket) => {
      socket.end()
      onWake()
    })
    server.on("error", (error) => {
      log("ERROR", "notify_server.start_failed", null, { error: String(error) })
      resolve(null)
    })
    server.listen(0, "127.0.0.1", () => resolve(server))
  })
}

function serverPort(server: Server | null): number | null {
  const address = server?.address()
  return typeof address === "object" && address ? address.port : null
}

type PromptModel = {
  providerID: string
  modelID: string
  variant?: string
}

// Best-effort fallback for non-hcom/manual plugin runs.
// Normal hcom launches seed launch agent/model through the `opencode-start`
// response payload, since the plugin process does not inherit the outer
// `hcom opencode --agent/--model` argv in PTY-launched OpenCode.
function parseCliArgValue(...flags: string[]): string | null {
  for (let i = 0; i < process.argv.length; i++) {
    const token = process.argv[i]
    for (const flag of flags) {
      if (token === flag) return process.argv[i + 1] ?? null
      if (token.startsWith(`${flag}=`)) return token.slice(flag.length + 1)
    }
  }
  return null
}

function parseCliModelArg(): PromptModel | null {
  const raw = parseCliArgValue("--model", "-m")
  if (!raw) return null
  const slash = raw.indexOf("/")
  if (slash <= 0 || slash === raw.length - 1) return null
  const variant = parseCliArgValue("--variant") ?? undefined
  return {
    providerID: raw.slice(0, slash),
    modelID: raw.slice(slash + 1),
    ...(variant ? { variant } : {}),
  }
}

function normalizePromptModel(model: unknown, inputVariant?: unknown): PromptModel | null {
  if (!model || typeof model !== "object") return null
  const providerID = (model as Record<string, unknown>).providerID
  const modelID = (model as Record<string, unknown>).modelID
  if (typeof providerID !== "string" || typeof modelID !== "string") return null
  const modelVariant = (model as Record<string, unknown>).variant
  const variant = typeof inputVariant === "string"
    ? inputVariant
    : typeof modelVariant === "string"
      ? modelVariant
      : undefined
  return {
    providerID,
    modelID,
    ...(variant ? { variant } : {}),
  }
}

type PromptRequest = {
  sessionID: string
  text: string
  agent: string | null
  model: PromptModel | null
  messageID: string
}

// What differs between the OpenCode 1 (and Kilo) and OpenCode 2 hosts.
type Transport = {
  // Queue a user prompt. Resolves to null once accepted, or an error description.
  prompt: (request: PromptRequest) => Promise<string | null>
  // The session's run state from the host, or null when the host cannot say.
  status: (sessionID: string) => Promise<"idle" | "busy" | null>
  // Answer a permission request "once"; null on success, or an error. OpenCode 2
  // approves hcom's commands through config rules instead, so it has none.
  allowOnce?: (sessionID: string, requestID: string) => Promise<string | null>
  // Create a session and show it in the TUI; its ID, or null on failure.
  // OpenCode 2's server plugin cannot show one, so it has none.
  startSession?: () => Promise<string | null>
  // The plugin's OpenCode 2 location. A plugin that OpenCode loads again there
  // binds the session the previous one bound.
  location?: { directory: string; workspaceID?: string }
}

// The session this process last bound, and the location that bound it. OpenCode 2
// reloads a location's plugins when it evicts the idle directory, and no session
// event follows. A launch binds a single agent, so only the latest binding counts.
// Kept on globalThis, which outlives this module if a reload imports it again.
const lastBinding: { location?: string; sessionId?: string; turn?: Promise<void> } = ((globalThis as any)[Symbol.for("hcom.opencode.lastBinding")] ??= {})

// Binds and record updates take turns across the process's plugins, so the record
// keeps the binding hcom made last. Resolves once the previous turn ends; call the
// returned function to end this one.
async function bindingTurn(): Promise<() => void> {
  const previous = lastBinding.turn
  let end!: () => void
  lastBinding.turn = new Promise<void>((resolve) => { end = resolve })
  await previous
  return end
}

// Clears the record if it names this session, unless it names the location to keep.
async function forgetBinding(sid: unknown, keep?: string): Promise<void> {
  const end = await bindingTurn()
  if (sid && sid === lastBinding.sessionId && (keep === undefined || keep !== lastBinding.location)) {
    delete lastBinding.location
    delete lastBinding.sessionId
  }
  end()
}

// A batch handed to OpenCode. It is acknowledged only once it is admitted to
// the session (seen in a model request on OpenCode 1, `session.inbox.delivered`
// on OpenCode 2), so a rejected or dropped prompt stays unread and is redelivered.
type Delivery = {
  text: string
  ackId: number
  hasMore: boolean
  messageID: string
  sentAt: number
  admitted: boolean
}

type HcomEvent = { type: string; properties?: any }

// OpenCode 2 prompt IDs: `msg_` plus a time-ordered suffix, chosen up front so
// the inbox events for this prompt can be told apart from the user's.
const ID_CHARS = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
let idCounter = 0
function newMessageID(): string {
  const time = (BigInt(Date.now()) * 0x1000n + BigInt(idCounter++ & 0xfff)).toString(16).padStart(12, "0").slice(-12)
  const random = Array.from(crypto.getRandomValues(new Uint8Array(14)), (byte) => ID_CHARS[byte % 62]).join("")
  return `msg_${time}${random}`
}

// OpenCode keys a location by directory and workspace.
function keyOf(location?: { directory: string; workspaceID?: string }): string | undefined {
  return location ? JSON.stringify([location.directory, location.workspaceID ?? null]) : undefined
}

function createCore(transport: Transport) {
  const locationKey = keyOf(transport.location)
  let instanceName: string | null = null
  let sessionId: string | null = null
  let bootstrapText: string | null = null
  let bindingPromise: Promise<void> | null = null
  // Bumped on every reset. Async work started under an older generation must
  // not touch state after it resumes.
  let generation = 0
  let notifyServer: Server | null = null
  let standbyServer: Server | null = null
  let standbyStarting = false
  let reconcileTimer: ReturnType<typeof setInterval> | null = null
  let reconcileInFlight = false
  let delivery: Delivery | null = null
  let deliveryInFlight = false   // reading a batch / handing it to OpenCode
  let deliveryPending = false    // a wake arrived while delivery was gated; replay it once clear
  let deliveryRetryScheduled = false
  let unadmittedRetries = 0
  let ackInFlight: Promise<void> | null = null
  let statusChain: Promise<void> = Promise.resolve()
  let lastReportedStatusKey: string | null = null
  let sessionBusy = false
  // Outstanding requests that hold the session for the user, by request ID.
  const pendingPermissions = new Map<string, string>()
  const pendingQuestions = new Set<string>()
  // OpenCode 1 transform edits are request-local, so a filled wake is refilled
  // with the same text on every later request. Keyed by part ID.
  const filledWakes = new Map<string, string>()
  let disposed = false
  let launchedAgent: string | null = parseCliArgValue("--agent")
  let launchedModel: PromptModel | null = parseCliModelArg()
  let currentAgent: string | null = launchedAgent
  let currentModel: PromptModel | null = launchedModel

  function isBoundSession(candidate: unknown): boolean {
    return !!sessionId && candidate === sessionId
  }

  function statusKey(status: string, context = "", detail = ""): string {
    return `${status}\0${context}\0${detail}`
  }

  function desiredStatus(): [string, string, string] {
    const permission = pendingPermissions.values().next()
    if (!permission.done) return ["blocked", "approval", permission.value]
    if (pendingQuestions.size) return ["blocked", "approval", "question"]
    return sessionBusy ? ["active", "", ""] : ["listening", "", ""]
  }

  function blocked(): boolean {
    return pendingPermissions.size > 0 || pendingQuestions.size > 0
  }

  // Status writes run one at a time so they land in call order. Each write
  // reports the state current when it runs, and the cache only records a write
  // hcom confirmed, so a failed write is retried by the next sync.
  function syncStatus(): Promise<void> {
    const gen = generation
    const run = statusChain.then(async () => {
      const name = instanceName
      if (!name || gen !== generation) return
      const [status, context, detail] = desiredStatus()
      const key = statusKey(status, context, detail)
      if (key === lastReportedStatusKey) return
      const args = ["opencode-status", "--name", name, "--status", status]
      if (context) args.push("--context", context)
      if (detail) args.push("--detail", detail)
      const reply = await rpc(args, name)
      if (reply && gen === generation) {
        lastReportedStatusKey = key
        log("DEBUG", "plugin.status", name, { status, context })
      }
    })
    statusChain = run.catch(() => {})
    return run
  }

  // Tool activity for status/`events --cmd`/`--file`. hcom derives the detail
  // from the tool's status_detail mapping; long strings (write content, patch
  // bodies) are capped so argv stays small.
  function reportTool(tool: string, input: unknown): Promise<void> {
    const gen = generation
    const args: Record<string, unknown> = {}
    for (const [key, value] of Object.entries(input && typeof input === "object" ? input : {})) {
      args[key] = typeof value === "string" && value.length > 2000 ? value.slice(0, 2000) : value
    }
    const run = statusChain.then(async () => {
      const name = instanceName
      if (!name || gen !== generation) return
      const reply = await rpc(["opencode-status", "--name", name, "--tool", tool, "--input-json", JSON.stringify(args)], name)
      // The tool detail is a form of active: a later sync must not overwrite it
      // with a bare active, but must still report the idle edge.
      if (reply && gen === generation) lastReportedStatusKey = statusKey("active")
    })
    statusChain = run.catch(() => {})
    return run
  }

  function stopNotifyServer(): void {
    try { notifyServer?.close() } catch {}
    notifyServer = null
  }

  function stopStandby(): void {
    try { standbyServer?.close() } catch {}
    standbyServer = null
  }

  function startReconcileTimer(): void {
    if (reconcileTimer) return
    reconcileTimer = setInterval(() => { void reconcile() }, RECONCILE_MS)
    reconcileTimer.unref?.()
  }

  function stopReconcileTimer(): void {
    if (reconcileTimer) clearInterval(reconcileTimer)
    reconcileTimer = null
  }

  function reset(): void {
    generation++
    stopNotifyServer()
    stopReconcileTimer()
    instanceName = null
    sessionId = null
    bootstrapText = null
    bindingPromise = null
    delivery = null
    deliveryInFlight = false
    deliveryPending = false
    deliveryRetryScheduled = false
    unadmittedRetries = 0
    ackInFlight = null
    statusChain = Promise.resolve()
    lastReportedStatusKey = null
    sessionBusy = false
    pendingPermissions.clear()
    pendingQuestions.clear()
    filledWakes.clear()
    currentAgent = launchedAgent
    currentModel = launchedModel
  }

  // fromRecord: bind only if the record still names this location and session.
  async function bindIdentity(sid: string, fromRecord = false): Promise<void> {
    if (instanceName) return
    if (bindingPromise) {
      const waitedGen = generation
      await bindingPromise
      // That attempt can end unbound (a reload skips a session a newer bind replaced,
      // or the bind fails), so a caller with its own session tries once. Callers that
      // waited together share that attempt.
      if (fromRecord || instanceName || waitedGen !== generation) return
      if (bindingPromise) return bindingPromise
    }
    if (disposed || process.env.HCOM_LAUNCHED !== "1") return
    const gen = generation
    const attempt = (async () => {
      let server: Server | null = null
      let endTurn = () => {}
      try {
        // Start the notify server before binding so its port registers atomically.
        server = await startNotifyServer(() => {
          log("DEBUG", "notify_server.wake", instanceName, { pending_ack: delivery?.ackId ?? null })
          void deliverPending("notify")
        })
        const port = serverPort(server)
        const args = ["opencode-start", "--session-id", sid]
        if (port) args.push("--notify-port", String(port))
        endTurn = await bindingTurn()
        if (gen !== generation || disposed) return
        if (fromRecord && (lastBinding.location !== locationKey || lastBinding.sessionId !== sid)) {
          log("INFO", "plugin.resume_skipped", null, {
            session_id: sid,
            bound_location: lastBinding.location ?? null,
            bound_session_id: lastBinding.sessionId ?? null,
          })
          return
        }
        const reply = await rpc(args, null)
        if (gen !== generation || disposed) return
        if (!reply || typeof reply.name !== "string" || !reply.name) {
          log("WARN", "plugin.bind_failed", null, { reply })
          return
        }
        const boundModel = normalizePromptModel(reply.model)
        if (typeof reply.agent === "string") launchedAgent = reply.agent
        if (boundModel) launchedModel = boundModel
        notifyServer = server
        server = null
        stopStandby()
        instanceName = reply.name
        sessionId = typeof reply.session_id === "string" && reply.session_id ? reply.session_id : sid
        if (locationKey) Object.assign(lastBinding, { location: locationKey, sessionId })
        bootstrapText = typeof reply.bootstrap === "string" && reply.bootstrap ? reply.bootstrap : null
        currentAgent = launchedAgent
        currentModel = launchedModel
        // opencode-start leaves the instance listening.
        lastReportedStatusKey = statusKey("listening")
        startReconcileTimer()
        log("INFO", "plugin.bound", instanceName, {
          session_id: sessionId,
          notify_port: port,
          bootstrap_len: bootstrapText?.length ?? 0,
          launched_agent: launchedAgent,
          launched_model: launchedModel?.modelID ?? null,
        })
      } catch (e) {
        log("ERROR", "plugin.bind_error", null, { error: String(e) })
      } finally {
        endTurn()
        // Not adopted (failed, or reset while binding): don't leak the listener.
        server?.close()
        if (gen === generation) bindingPromise = null
      }
    })()
    bindingPromise = attempt
    await attempt
  }

  // Replay a wake that was queued while delivery was gated. The microtask +
  // dedup flag collapse a burst of queued wakes into one pass.
  function drainPendingDelivery(reason: string): void {
    if (!deliveryPending || deliveryInFlight || delivery || blocked()) return
    deliveryPending = false
    if (deliveryRetryScheduled) return
    deliveryRetryScheduled = true
    log("DEBUG", "plugin.delivery_retry_scheduled", instanceName, { reason })
    queueMicrotask(() => {
      deliveryRetryScheduled = false
      void deliverPending(reason)
    })
  }

  async function deliverPending(reason: string): Promise<void> {
    const name = instanceName
    const sid = sessionId
    const gen = generation
    if (disposed || !name || !sid) return
    if (deliveryInFlight || delivery || blocked()) {
      // Drop nothing: replayed once the gate clears.
      deliveryPending = true
      log("DEBUG", "plugin.delivery_skipped", name, {
        reason: deliveryInFlight ? "delivery_in_flight" : delivery ? "awaiting_admission" : "blocked",
        pending_ack: delivery?.ackId ?? null,
        wake: reason,
      })
      return
    }
    deliveryInFlight = true
    try {
      const batch = await readBatch(name)
      if (gen !== generation || disposed || !batch || batch.messages.length === 0) return
      const sent: Delivery = {
        text: formatMessagesForInjection(batch.messages, name),
        ackId: batch.ackId,
        hasMore: batch.hasMore,
        messageID: newMessageID(),
        sentAt: Date.now(),
        admitted: false,
      }
      // Set before prompting: admission can be observed before the call returns.
      delivery = sent
      let error: string | null
      try {
        error = await transport.prompt({
          sessionID: sid,
          text: sent.text,
          agent: currentAgent,
          model: currentModel,
          messageID: sent.messageID,
        })
      } catch (e) {
        error = String(e)
      }
      if (gen !== generation) return
      if (error) {
        // Retried on the next wake or idle edge, not at once, so a prompt that
        // keeps failing (e.g. missing auth) is not resent in a loop.
        if (delivery === sent && !sent.admitted) delivery = null
        deliveryPending = false
        log("ERROR", "plugin.delivery_prompt_failed", name, { error: error.slice(0, 300), ack_id: sent.ackId })
        return
      }
      log("INFO", "plugin.delivery_sent", name, { count: batch.messages.length, ack_id: sent.ackId, wake: reason })
    } finally {
      if (gen === generation) {
        deliveryInFlight = false
        drainPendingDelivery("delivery_in_flight_wake")
      }
    }
  }

  // Acknowledge an admitted delivery. Serialized; on failure the delivery stays
  // outstanding (gating redelivery) and reconcile retries.
  function flushAck(source: string): Promise<void> {
    if (ackInFlight) return ackInFlight
    const sent = delivery
    const name = instanceName
    const gen = generation
    if (!sent?.admitted || !name) return Promise.resolve()
    const attempt = (async () => {
      const reply = await rpc(["opencode-read", "--name", name, "--ack", "--up-to", String(sent.ackId)], name)
      if (gen !== generation || delivery !== sent) return
      if (reply?.acked_to !== sent.ackId) {
        if (reply) log("WARN", "plugin.rpc_failed", name, { command: "opencode-read", error: "unexpected ack reply", reply })
        return
      }
      delivery = null
      log("INFO", "plugin.delivery_acked", name, { acked_to: sent.ackId, source, has_more: sent.hasMore })
      if (sent.hasMore) deliveryPending = true
      drainPendingDelivery("post_ack_wake")
    })().finally(() => {
      if (gen === generation) ackInFlight = null
    })
    ackInFlight = attempt
    return attempt
  }

  function admit(source: string): void {
    const sent = delivery
    if (!sent || sent.admitted) return
    sent.admitted = true
    unadmittedRetries = 0
    log("DEBUG", "plugin.delivery_admitted", instanceName, { ack_id: sent.ackId, source })
    void flushAck(source)
  }

  // A delivery is never admitted when OpenCode rejects or drops the prompt;
  // once the session sits idle past the grace period, release it. The idle
  // edge may already have passed, so redeliver now, a bounded number of times
  // in a row; past that, only the next wake retries.
  function releaseUnadmitted(reason: string): void {
    const sent = delivery
    if (!sent || sent.admitted || deliveryInFlight || sessionBusy) return
    if (Date.now() - sent.sentAt < ADMIT_GRACE_MS) return
    delivery = null
    const retry = unadmittedRetries < MAX_UNADMITTED_RETRIES
    if (retry) unadmittedRetries++
    deliveryPending = retry
    log("WARN", "plugin.delivery_not_admitted", instanceName, { ack_id: sent.ackId, reason, retry })
    drainPendingDelivery("not_admitted")
  }

  // Take the next batch for a body-free wake the user's prompt already carries.
  // With a `messageID` (OpenCode 2) it is admitted when that prompt is
  // delivered; otherwise at once, since OpenCode 1 refills the same text into
  // every later request of the session.
  async function claimWake(sid: string, messageID = ""): Promise<string | null> {
    const name = instanceName
    const gen = generation
    if (!name || !isBoundSession(sid) || delivery || deliveryInFlight) return null
    deliveryInFlight = true
    try {
      const batch = await readBatch(name)
      if (gen !== generation || !batch || batch.messages.length === 0) return null
      const text = formatMessagesForInjection(batch.messages, name)
      delivery = {
        text,
        ackId: batch.ackId,
        hasMore: batch.hasMore,
        messageID,
        sentAt: Date.now(),
        admitted: false,
      }
      log("INFO", "plugin.wake_filled", name, { ack_id: batch.ackId, count: batch.messages.length })
      if (!messageID) admit("wake_fill")
      return text
    } finally {
      if (gen === generation) {
        deliveryInFlight = false
        drainPendingDelivery("wake_fill")
      }
    }
  }

  // Periodic sync: retries failed status writes and acks, and recovers state
  // from the host when it can report it. Delivery itself is driven by the
  // notify server and the idle edge.
  async function reconcile(): Promise<void> {
    if (reconcileInFlight || !instanceName || !sessionId) return
    reconcileInFlight = true
    try {
      const status = await transport.status(sessionId).catch(() => null)
      if (status) setBusy(status === "busy")
      await syncStatus()
      if (delivery?.admitted) await flushAck("reconcile")
      releaseUnadmitted("reconcile")
    } catch (e) {
      log("ERROR", "plugin.reconcile_error", instanceName, { error: String(e) })
    } finally {
      reconcileInFlight = false
    }
  }

  function setBusy(busy: boolean): void {
    sessionBusy = busy
    // An idle session is not waiting on the user: drop requests whose reply
    // event was missed, so they cannot gate delivery forever.
    if (!busy) {
      pendingPermissions.clear()
      pendingQuestions.clear()
    }
  }

  // OpenCode 1/Kilo: answer a bash request for exactly one safe hcom command,
  // when auto-approve is on (hcom decides). Done here rather than with config
  // rules, which would replace a user's own plain `"bash": "..."` setting.
  async function approveHcomCommand(requestID: string, properties: any): Promise<boolean> {
    const name = instanceName
    const sid = sessionId
    const command = properties.metadata?.command
    if (!transport.allowOnce || !name || !sid || properties.permission !== "bash" || typeof command !== "string") return false
    const reply = await rpc(["opencode-approve", "--name", name, "--command", command], name)
    if (reply?.approve !== true) return false
    const error = await transport.allowOnce(sid, requestID).catch((e) => String(e))
    if (error) {
      log("WARN", "plugin.auto_approve_failed", name, { request_id: requestID, error: error.slice(0, 300) })
      return false
    }
    log("INFO", "plugin.auto_approved", name, { request_id: requestID })
    return true
  }

  function bindingEventSession(event: HcomEvent): string | undefined {
    const properties = event.properties
    switch (event.type) {
      case "session.created":
        return properties?.info?.id
      case "session.status":
      case "permission.asked":
        return properties?.sessionID
    }
    return undefined
  }

  async function onEvent(event: HcomEvent): Promise<void> {
    const properties = event.properties ?? {}
    // The session belongs to another location now: a reload here must not bind it.
    // Checked before the bound-session test, since an unbound plugin still holds the record.
    // OpenCode 2 sends a move to both locations: either one keeps only a record that
    // names the destination, whichever handles it first.
    if (event.type === "session.moved") void forgetBinding(properties.sessionID, keyOf(properties.location))
    const bindSession = bindingEventSession(event)
    if (typeof bindSession === "string" && !instanceName) await bindIdentity(bindSession)
    else if (bindingPromise) await bindingPromise
    // Nor a deleted session. Cleared after a bind in flight, so a reload that binds
    // the session still stops it below, and before the bound-session test, as above.
    if (event.type === "session.deleted") void forgetBinding(properties.sessionID)
    const eventSession = event.type === "session.created" ? properties.info?.id : properties.sessionID
    if (!instanceName || !isBoundSession(eventSession)) return

    switch (event.type) {
      case "session.status": {
        const type = properties.status?.type
        log("DEBUG", "plugin.session_status", instanceName, { status: type })
        setBusy(type !== "idle")
        await syncStatus()
        if (type === "idle") {
          releaseUnadmitted("idle")
          deliveryPending = true
          drainPendingDelivery("idle")
        }
        break
      }
      case "permission.asked": {
        const id = String(properties.id ?? "")
        if (await approveHcomCommand(id, properties)) break
        pendingPermissions.set(id, String(properties.permission ?? ""))
        log("INFO", "plugin.permission_asked", instanceName, { permission: properties.permission, request_id: id })
        await syncStatus()
        break
      }
      case "permission.replied": {
        const id = properties.requestID ?? properties.permissionID
        if (typeof id === "string") pendingPermissions.delete(id)
        else pendingPermissions.clear()
        await syncStatus()
        deliveryPending = true
        drainPendingDelivery("permission_replied")
        break
      }
      case "question.asked": {
        // Kilo marks questions that leave the prompt usable as non-blocking.
        if (properties.blocking === false) break
        pendingQuestions.add(String(properties.id ?? ""))
        log("INFO", "plugin.question_asked", instanceName, { request_id: properties.id })
        await syncStatus()
        break
      }
      case "question.replied":
      case "question.rejected": {
        if (typeof properties.requestID === "string") pendingQuestions.delete(properties.requestID)
        else pendingQuestions.clear()
        await syncStatus()
        deliveryPending = true
        drainPendingDelivery("question_answered")
        break
      }
      case "session.inbox.delivered":
        if (delivery && properties.inboxID === delivery.messageID) admit("inbox_delivered")
        break
      case "session.inbox.cancelled":
        if (delivery && !delivery.admitted && properties.inboxID === delivery.messageID) {
          log("WARN", "plugin.delivery_cancelled", instanceName, { ack_id: delivery.ackId })
          delivery = null
          deliveryPending = false
        }
        break
      case "session.deleted": {
        const name = instanceName
        log("INFO", "plugin.session_deleted", name)
        void forgetBinding(sessionId)
        reset()
        await rpc(["opencode-stop", "--name", name, "--reason", "closed"], name)
        break
      }
    }
  }

  async function onChatMessage(input: any): Promise<void> {
    const sid = input?.sessionID
    if (typeof sid !== "string") return
    if (!instanceName) await bindIdentity(sid)
    else if (bindingPromise) await bindingPromise
    if (!isBoundSession(sid)) return
    if (input.agent) currentAgent = input.agent
    const resolvedModel = normalizePromptModel(input.model, input.variant)
    if (resolvedModel) currentModel = resolvedModel
    log("DEBUG", "plugin.chat_message", instanceName, {
      agent: input.agent,
      model: input.model?.modelID,
      variant: input.variant,
    })
  }

  // OpenCode 1: the model request's messages, editable for this request only.
  async function onMessagesTransform(messages: any[]): Promise<void> {
    const sid = messages.find((m) => typeof m?.info?.sessionID === "string")?.info.sessionID
    if (!sid) return
    if (bindingPromise) await bindingPromise
    if (!instanceName || !isBoundSession(sid)) return
    const users = messages.filter((m) => m?.info?.role === "user" && Array.isArray(m.parts))
    const textParts = (m: any) => m.parts.filter((p: any) => p?.type === "text" && typeof p.text === "string")
    const partKey = (m: any, p: any) => String(p.id ?? `${m.info.id}:${m.parts.indexOf(p)}`)

    for (const m of users) {
      for (const p of textParts(m)) {
        const filled = filledWakes.get(partKey(m, p))
        if (filled) p.text = filled
      }
    }

    const first = users[0]
    if (bootstrapText && first) {
      first.parts.push({
        id: crypto.randomUUID(),
        messageID: first.info.id,
        sessionID: first.info.sessionID,
        type: "text",
        text: bootstrapText,
        synthetic: true,
      })
    }

    const last = users.at(-1)
    const wake = last && textParts(last).find((p: any) => !p.synthetic && isWakeText(p.text) && !filledWakes.has(partKey(last, p)))
    if (wake) {
      const text = await claimWake(sid)
      if (text) {
        wake.text = text
        filledWakes.set(partKey(last, wake), text)
      }
    }

    const sent = delivery
    if (sent && !sent.admitted && users.some((m) => textParts(m).some((p: any) => p.text === sent.text))) {
      admit("transform")
    }
  }

  // OpenCode 2: the prompt before it is stored, so a filled wake persists.
  async function onPrompt(sid: string, messageID: string, text: string): Promise<string | null> {
    await onChatMessage({ sessionID: sid })
    if (!isWakeText(text)) return null
    return claimWake(sid, messageID)
  }

  function bootstrapFor(sid: unknown): string | null {
    return instanceName && isBoundSession(sid) ? bootstrapText : null
  }

  function compactionContext(sid: unknown): string | null {
    if (!instanceName || !isBoundSession(sid)) return null
    log("INFO", "plugin.compaction_reset", instanceName)
    return `You are connected to hcom as "${instanceName}". Use --name ${instanceName} for all hcom commands.`
  }

  // After a gap in the host's event stream: re-read state and deliver anything
  // the missed events would have.
  function resync(): void {
    void reconcile().then(() => {
      if (!sessionBusy) {
        deliveryPending = true
        drainPendingDelivery("resync")
      }
    })
  }

  // Fresh launch: OpenCode creates no session until the first prompt. Stand by
  // to create one: hcom wakes this port when mail waits and the TUI is ready.
  async function standBy(startSession: () => Promise<string | null>): Promise<void> {
    const server = await startNotifyServer(() => void onStandbyWake(startSession))
    const port = serverPort(server)
    if (!server || !port || disposed || instanceName || bindingPromise) {
      server?.close()
      return
    }
    standbyServer = server
    if (!(await rpc(["opencode-standby", "--port", String(port)], null))) stopStandby()
  }

  // hcom wakes again until the session binds, so a failed start is retried.
  // Binding closes the standby server.
  async function onStandbyWake(startSession: () => Promise<string | null>): Promise<void> {
    if (!standbyServer || standbyStarting || disposed || instanceName || bindingPromise) return
    standbyStarting = true
    try {
      const sid = await startSession()
      if (!sid) return
      log("INFO", "plugin.session_started", null, { session_id: sid })
      // session.created binds it too; whichever runs first does the binding.
      await bindIdentity(sid)
      if (instanceName && sessionId === sid) await deliverPending("session_start")
    } finally {
      standbyStarting = false
    }
  }

  // Resumed session (`hcom r`, or a plugin reloaded where a session was bound):
  // OpenCode emits no session.created, and no session.status until the first
  // prompt, so bind now and deliver anything already pending. Not awaited:
  // plugin load must not wait on hcom.
  const reloadedSessionId = locationKey && locationKey === lastBinding.location ? lastBinding.sessionId : undefined
  const fromRecord = !process.env.HCOM_RESUME_SESSION_ID
  const resumeSessionId = process.env.HCOM_RESUME_SESSION_ID || reloadedSessionId
  if (process.env.HCOM_LAUNCHED === "1") {
    log("INFO", "plugin.loaded", process.env.HCOM_NAME || null, {
      pid: process.pid,
      directory: transport.location?.directory ?? null,
      workspace_id: transport.location?.workspaceID ?? null,
      resume_session_id: resumeSessionId || null,
    })
  }
  const startSession = transport.startSession
  if (!resumeSessionId && startSession && process.env.HCOM_LAUNCHED === "1") {
    void standBy(startSession).catch((e) => log("ERROR", "plugin.standby_error", null, { error: String(e) }))
  }
  if (resumeSessionId) {
    void (async () => {
      await bindIdentity(resumeSessionId, fromRecord)
      if (disposed || !instanceName || sessionId !== resumeSessionId) return
      log("INFO", "plugin.resume_bound", instanceName, { session_id: sessionId })
      await deliverPending("resume")
    })().catch((e) => log("ERROR", "plugin.resume_bind_error", instanceName, { error: String(e) }))
  }

  async function guarded(name: string, fn: () => Promise<void>): Promise<void> {
    try {
      await fn()
    } catch (e) {
      log("ERROR", name, instanceName, { error: String(e) })
    }
  }

  return {
    event: (event: HcomEvent) => guarded("plugin.event_error", () => onEvent(event)),
    chatMessage: (input: any) => guarded("plugin.chat_message_error", () => onChatMessage(input)),
    toolBefore: (sid: unknown, tool: string, input: unknown) =>
      guarded("plugin.tool_status_error", async () => {
        if (instanceName && isBoundSession(sid)) await reportTool(tool, input)
      }),
    messagesTransform: (messages: any[]) => guarded("plugin.transform_error", () => onMessagesTransform(messages)),
    prompt: async (sid: string, messageID: string, text: string) => {
      try {
        return await onPrompt(sid, messageID, text)
      } catch (e) {
        log("ERROR", "plugin.prompt_error", instanceName, { error: String(e) })
        return null
      }
    },
    bootstrapFor,
    compactionContext,
    resync,
    // Plugin unload, not session end: the hcom instance stays bound, since a
    // reloaded plugin rebinds the same session (see bindIdentity).
    dispose: async () => {
      if (process.env.HCOM_LAUNCHED === "1") log("INFO", "plugin.unloaded", instanceName, { session_id: sessionId })
      disposed = true
      generation++
      stopNotifyServer()
      stopStandby()
      stopReconcileTimer()
    },
  }
}

// The calls the plugin makes on the OpenCode 1 / Kilo SDK client. Structural, so
// one plugin fits both apps' clients; `tests/plugin_types.ts` checks it against
// each app's own `Plugin` type.
type OpenCode1Client = {
  session: {
    create(options?: any): Promise<any>
    promptAsync(options: any): Promise<any>
    status(options?: any): Promise<any>
  }
  tui: {
    publish(options: any): Promise<any>
  }
  postSessionIdPermissionsPermissionId(options: any): Promise<any>
}

export const HcomPlugin = async ({ client }: { client: OpenCode1Client }) => {
  if (!claimPluginHost()) return {}
  const core = createCore({
    async prompt({ sessionID, text, agent, model }) {
      // SDK types don't expose agent/model on the async variant; the body shape
      // matches the sync prompt endpoint. The SDK resolves (not rejects) with
      // `error` set for a refused request.
      const result = await client.session.promptAsync({
        path: { id: sessionID },
        body: {
          agent: agent ?? undefined,
          model: model ? { providerID: model.providerID, modelID: model.modelID } : undefined,
          variant: model?.variant ?? undefined,
          parts: [{ type: "text", text }],
        },
      })
      return result?.error ? JSON.stringify(result.error) : null
    },
    async status(sessionID) {
      const result = await client.session.status()
      if (!result?.data || result.error) return null
      const current = result.data[sessionID]
      return !current || current.type === "idle" ? "idle" : "busy"
    },
    async allowOnce(sessionID, requestID) {
      const result = await client.postSessionIdPermissionsPermissionId({
        path: { id: sessionID, permissionID: requestID },
        body: { response: "once" },
      })
      return result?.error ? JSON.stringify(result.error) : null
    },
    async startSession() {
      const created = await client.session.create({ body: {} })
      const sessionID = created?.data?.id
      if (typeof sessionID !== "string" || !sessionID) {
        log("WARN", "plugin.session_create_failed", null, { error: JSON.stringify(created?.error ?? null) })
        return null
      }
      // The SDK types leave this event out of /tui/publish, but the server takes it.
      const shown = await client.tui.publish({ body: { type: "tui.session.select", properties: { sessionID } } as any })
      if (shown?.error) log("WARN", "plugin.session_select_failed", null, { error: JSON.stringify(shown.error) })
      return sessionID
    },
  })
  return {
    event: async ({ event }: { event: any }) => core.event(event),
    "chat.message": async (input: any) => core.chatMessage(input),
    "tool.execute.before": async (input: any, output: any) => core.toolBefore(input.sessionID, input.tool, output.args),
    "experimental.chat.messages.transform": async (_input: any, output: any) => core.messagesTransform(output.messages ?? []),
    "experimental.session.compacting": async (input: any, output: any) => {
      const context = core.compactionContext(input.sessionID)
      if (context) output.context.push(context)
    },
    dispose: core.dispose,
  }
}

// Reshapes OpenCode 2 events into the OpenCode 1 shape the core handles.
// Execution events carry the busy/idle edge.
function v1Event({ type, data }: { type: string; data: any }): HcomEvent | null {
  switch (type) {
    case "session.created":
      return { type, properties: { info: { ...data, id: data.sessionID } } }
    case "session.execution.started":
      return { type: "session.status", properties: { sessionID: data.sessionID, status: { type: "busy" } } }
    case "session.execution.succeeded":
    case "session.execution.failed":
    case "session.execution.interrupted":
      return { type: "session.status", properties: { sessionID: data.sessionID, status: { type: "idle" } } }
    case "permission.asked":
      return { type, properties: { id: data.id, sessionID: data.sessionID, permission: data.action } }
    case "session.status":
    case "permission.replied":
    case "session.deleted":
    case "session.moved":
    case "session.inbox.delivered":
    case "session.inbox.cancelled":
      return { type, properties: data }
  }
  return null
}

type LaunchModel = { providerID: string; id: string; variant?: string }

// `provider/model[#variant]`, the form `opencode --model` took.
function parseLaunchModel(raw: string | undefined): LaunchModel | undefined {
  const [ref, variant] = (raw ?? "").split("#")
  const slash = ref.indexOf("/")
  if (slash <= 0 || slash === ref.length - 1) return undefined
  return { providerID: ref.slice(0, slash), id: ref.slice(slash + 1), ...(variant ? { variant } : {}) }
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms)
    signal.addEventListener("abort", () => { clearTimeout(timer); resolve() }, { once: true })
  })
}

// OpenCode 2 loads plugins from the module's default export and hands `setup`
// a context with no v1 `client`/`$`.
async function setupOpenCode2(ctx: Plugin2.Context) {
  if (!claimPluginHost()) return async () => {}

  const core = createCore({
    location: ctx.location,
    async prompt({ sessionID, text, messageID }) {
      // Steering reaches the model at the next step boundary of a running turn.
      await ctx.session.prompt({ sessionID, id: messageID, text, delivery: "steer" })
      return null
    },
    // OpenCode 2 has no run-state query; status comes from events alone, and
    // the event stream is resubscribed after a gap.
    async status() {
      return null
    },
  })

  const registrations: { dispose: () => Promise<void> | void }[] = []
  const disposeAll = async () => {
    try {
      await Promise.all(registrations.map((r) => r.dispose()))
    } finally {
      await core.dispose()
    }
  }
  // `hcom opencode --agent/--model`, moved to env by the launcher: OpenCode 2's TUI
  // has no such flags. The agent becomes the server default. The model is switched
  // on each session's first prompt, before its turn runs: agent files override any
  // default model set here, since OpenCode applies them after this plugin's setup.
  const launchAgent = process.env.HCOM_OPENCODE_AGENT
  const launchModel = parseLaunchModel(process.env.HCOM_OPENCODE_MODEL)
  // One switch per session, shared by concurrent first prompts; a failed switch
  // is dropped so the next prompt retries it.
  const modelSwitches = new Map<string, Promise<unknown>>()
  function switchToLaunchModel(sessionID: string, model: LaunchModel) {
    let pending = modelSwitches.get(sessionID)
    if (!pending) {
      pending = ctx.session.switchModel({ sessionID, model } as any)
      modelSwitches.set(sessionID, pending)
      pending.catch(() => modelSwitches.delete(sessionID))
    }
    return pending
  }

  try {
    if (launchAgent) registrations.push(await ctx.agent.transform((editor) => editor.default(launchAgent)))
    registrations.push(await ctx.session.hook("prompt", async (draft) => {
      if (launchModel) await switchToLaunchModel(draft.sessionID, launchModel)
      const filled = await core.prompt(draft.sessionID, draft.messageID, draft.prompt.text)
      if (filled) draft.prompt.text = filled
    }))
    registrations.push(await ctx.session.hook("context", (draft) => {
      const bootstrap = core.bootstrapFor(draft.sessionID)
      if (bootstrap) draft.system.push({ type: "text", text: bootstrap })
    }))
    registrations.push(await ctx.tool.hook("execute.before", (event) => core.toolBefore(event.sessionID, event.tool, event.input)))
    registrations.push(await ctx.session.hook("compaction", (draft) => {
      const context = core.compactionContext(draft.sessionID)
      if (context) draft.system.push({ type: "text", text: context })
    }))
  } catch (e) {
    await disposeAll().catch(() => {}) // the registration error is the one to report
    throw e
  }

  const controller = new AbortController()
  const consuming = (async () => {
    let backoff = 250
    for (let attempt = 0; !controller.signal.aborted; attempt++) {
      try {
        if (attempt > 0) core.resync()
        for await (const event of ctx.event.subscribe({ signal: controller.signal } as any)) {
          backoff = 250
          const mapped = v1Event(event as any)
          if (mapped) await core.event(mapped)
        }
        if (!controller.signal.aborted) log("WARN", "plugin.event_stream_ended", null)
      } catch (e) {
        if (!controller.signal.aborted) log("ERROR", "plugin.event_subscription_failed", null, { error: String(e) })
      }
      await sleep(backoff, controller.signal)
      backoff = Math.min(backoff * 2, 10_000)
    }
  })()

  return async () => {
    controller.abort()
    await consuming
    await disposeAll()
  }
}

// OpenCode 1 rejects a default export without `server`; pointing it at
// HcomPlugin keeps a single instance under both loaders.
export default {
  id: "hcom",
  server: HcomPlugin,
  setup: setupOpenCode2,
}
