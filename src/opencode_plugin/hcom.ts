import type { Plugin, PluginInput } from "@opencode-ai/plugin"
import type { Event } from "@opencode-ai/sdk"
import { appendFileSync } from "fs"
import { homedir } from "os"

const HCOM_DIR = process.env.HCOM_DIR || `${homedir()}/.hcom`
const LOG_PATH = `${HCOM_DIR}/.tmp/logs/hcom.log`

type PromptModel = {
  providerID: string
  modelID: string
  variant?: string
}

type PermissionAskedEvent = {
  type: "permission.asked"
  properties: {
    id: string
    sessionID: string
    permission: string
  }
}

type HcomEvent = Event | PermissionAskedEvent

function eventSessionID(event: HcomEvent): string | undefined {
  if (!event.properties || typeof event.properties !== "object") return undefined
  const properties = event.properties as Record<string, unknown>
  if (typeof properties.sessionID === "string") return properties.sessionID
  const info = properties.info
  if (info && typeof info === "object" && typeof (info as Record<string, unknown>).id === "string") {
    return (info as Record<string, unknown>).id as string
  }
  return undefined
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

function parseCliModelArg() {
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

function normalizePromptModel(model: unknown, inputVariant?: unknown) {
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

function promptModelRef(model: PromptModel | null) {
  if (!model) return undefined
  return {
    providerID: model.providerID,
    modelID: model.modelID,
  }
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
  try { appendFileSync(LOG_PATH, entry + "\n") } catch {}
}

export const HcomPlugin: Plugin = async ({ client, $ }) => {
  let hcomChecked = false
  let hcomAvailable = false
  let instanceName: string | null = null      // IDEN-03: bound instance name
  let sessionId: string | null = null         // IDEN-02: tracked for messages.transform
  let bootstrapText: string | null = null     // BOOT-01: cached from opencode-start
  let bindingPromise: Promise<void> | null = null  // Prevents duplicate binding
  let reconcileTimer: ReturnType<typeof setInterval> | null = null  // Periodic status sync + delivery fallback
  let reconcileInFlight = false                 // Prevents concurrent reconcile calls from overlapping interval ticks
  let notifyServer: Bun.TCPSocketListener | null = null  // TCP notify server for instant message wake
  let lastReportedStatus: string | null = null  // Skip redundant status updates
  let pendingAckId: number | null = null        // Deferred ack: set by deliverPendingToIdle, acked by transform
  let deliveryInFlight = false                  // Delivery guard flag set before the first await
  let deliveryPending = false                   // Wake arrived while delivery was already in flight
  let deliveryRetryScheduled = false            // Avoid duplicate queued retry passes
  let permissionPending = false                  // Exact permission gate from OpenCode events
  let launchedAgent: string | null = parseCliArgValue("--agent")
  let launchedModel: PromptModel | null = parseCliModelArg()
  let currentAgent: string | null = launchedAgent
  let currentModel: PromptModel | null = launchedModel

  // SAFE-02: Lazy PATH detection on first hook callback
  function checkHcom(): boolean {
    if (!hcomChecked) {
      hcomChecked = true
      hcomAvailable = Bun.which("hcom") !== null
      if (!hcomAvailable) {
        log("WARN", "plugin.no_hcom")
      }
    }
    return hcomAvailable
  }

  function isBoundSession(candidateSessionId?: string | null): boolean {
    return !candidateSessionId || !sessionId || candidateSessionId === sessionId
  }

  function ignoreForeignSession(event: string, candidateSessionId?: string | null): boolean {
    if (isBoundSession(candidateSessionId)) return false
    log("DEBUG", event, instanceName, {
      session_id: candidateSessionId,
      bound_session_id: sessionId,
    })
    return true
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

  function schedulePendingDelivery(sid: string, reason: string): void {
    if (deliveryRetryScheduled) return
    deliveryRetryScheduled = true
    log("DEBUG", "plugin.delivery_retry_scheduled", instanceName, { reason })
    queueMicrotask(() => {
      deliveryRetryScheduled = false
      if (!instanceName) return
      void deliverPendingToIdle(sid)
    })
  }

  // Re-arm a queued wake once nothing is mid-flight. The pendingAckId guard
  // leaves the still-injecting case to the post-ack drain so we do not
  // double-deliver the same unread message batch.
  function drainPendingDelivery(sid: string, reason: string): void {
    if (deliveryPending && pendingAckId === null) {
      deliveryPending = false
      schedulePendingDelivery(sid, reason)
    }
  }

  // Deliver pending messages via promptAsync. Ack is deferred to transform
  // (fires on the loop iteration that actually processes the user message).
  //
  // Three-layer serialization:
  //   deliveryInFlight — guard flag set synchronously before the first await.
  //     Closes the TOCTOU window where TCP notify and idle-status wake paths
  //     could both pass a null check before either one set the value.
  //     Concurrent callers set deliveryPending so their wake is replayed after
  //     the current pass, or after the current injected message is acked.
  //   pendingAckId — set after messages are read, cleared by transform.
  //     Prevents re-delivery while a prior injection is still being processed.
  //     If promptAsync fails to queue, pendingAckId is cleared immediately.
  //   deliveryPending — a wake that arrived while delivery was in-flight or an
  //     injection was pending ack. drainPendingDelivery replays it at each exit
  //     point: normal finally, deferred ack, and promptAsync rejection.
  async function deliverPendingToIdle(sid: string): Promise<boolean> {
    if (permissionPending) {
      log("DEBUG", "plugin.delivery_skipped", instanceName, { reason: "permission_pending" })
      return false
    }
    if (!instanceName) return false
    if (ignoreForeignSession("plugin.delivery_ignored_foreign_session", sid)) {
      return false
    }
    if (deliveryInFlight) {
      deliveryPending = true
      log("DEBUG", "plugin.delivery_skipped", instanceName, { reason: "delivery_in_flight", queued: true })
      return false
    }
    if (pendingAckId !== null) {
      deliveryPending = true
      log("DEBUG", "plugin.delivery_skipped", instanceName, { reason: "pending_ack_in_flight", pending_ack: pendingAckId, queued: true })
      return false
    }
    deliveryInFlight = true
    try {
      const msgResult = await $.nothrow()`hcom opencode-read --name ${instanceName}`.quiet()
      if (msgResult.exitCode !== 0) {
        log("WARN", "plugin.delivery_read_failed", instanceName, { exit_code: msgResult.exitCode, stderr: msgResult.stderr.toString().slice(0, 200) })
        return false
      }
      let rawMessages: any[] = []
      try { rawMessages = JSON.parse(msgResult.text()) } catch (e) {
        log("WARN", "plugin.delivery_parse_failed", instanceName, { error: String(e), raw: msgResult.text().slice(0, 200) })
        return false
      }
      if (!Array.isArray(rawMessages) || rawMessages.length === 0) {
        log("DEBUG", "plugin.delivery_no_messages", instanceName)
        return false
      }

      const maxId = Math.max(...rawMessages.map((m: any) => m.event_id || 0))
      if (maxId === 0) return false

      const formatted = formatMessagesForInjection(rawMessages, instanceName)
      // Don't ack here — defer to transform so cursor advances only when
      // the loop is actually processing the message. This keeps messages
      // unread until delivery is confirmed.
      pendingAckId = maxId
      log("DEBUG", "plugin.delivery_payload", instanceName, {
        session_id: sid,
        current_agent: currentAgent,
        current_model: currentModel?.modelID ?? null,
        current_variant: currentModel?.variant ?? null,
      })
      try {
        // Runtime contract note: keep this cast until the plugin's bundled client
        // typings are aligned across shipped OpenCode builds.
        const promptAsyncResult = client.session.promptAsync({
          path: { id: sid },
          body: {
            agent: currentAgent ?? undefined,
            model: promptModelRef(currentModel),
            variant: currentModel?.variant ?? undefined,
            parts: [{ type: "text", text: formatted }],
          },
        } as any) // SDK types don't expose agent/model on the async variant; body shape matches the sync prompt endpoint
        if (promptAsyncResult && typeof (promptAsyncResult as Promise<unknown>).then === "function") {
          void (promptAsyncResult as Promise<unknown>).catch((e) => {
            if (pendingAckId === maxId) pendingAckId = null
            log("ERROR", "plugin.delivery_prompt_failed", instanceName, {
              error: String(e),
              pending_ack: maxId,
            })
            drainPendingDelivery(sid, "prompt_async_failed_pending_wake")
          })
        }
      } catch (e) {
        pendingAckId = null
        log("ERROR", "plugin.delivery_prompt_failed", instanceName, {
          error: String(e),
          pending_ack: maxId,
          sync_throw: true,
        })
        return false
      }
      log("INFO", "plugin.delivery_pending", instanceName, {
        msg: `promptAsync, ack deferred to transform (maxId=${maxId})`,
        count: rawMessages.length,
        pending_ack: maxId,
      })
      return true
    } finally {
      deliveryInFlight = false
      drainPendingDelivery(sid, "delivery_in_flight_wake")
    }
  }

  // Periodic status sync: polls session status API as a retry mechanism
  // in case the event-driven opencode-status call failed (subprocess error,
  // daemon down, etc. other made up scenario etc.). Does NOT deliver messages — that's handled by
  // TCP notify (on message arrival) and session.status events (on idle).
  async function reconcile(): Promise<void> {
    if (reconcileInFlight) return
    if (permissionPending) return
    if (!instanceName || !sessionId) return
    reconcileInFlight = true
    try {
      const statusResult = await client.session.status()
      if (!statusResult.data) return
      const current = statusResult.data[sessionId]
      const isIdle = !current || current.type === "idle"
      const hcomStatus = isIdle ? "listening" : "active"
      if (hcomStatus !== lastReportedStatus) {
        lastReportedStatus = hcomStatus
        await $.nothrow()`hcom opencode-status --name ${instanceName} --status ${hcomStatus}`.quiet()
        log("INFO", "plugin.reconcile_status", instanceName, { status: hcomStatus })
      }
    } catch (e) {
      log("ERROR", "plugin.reconcile_error", instanceName, { error: String(e) })
    } finally {
      reconcileInFlight = false
    }
  }

  function startReconcileTimer(): void {
    stopReconcileTimer()
    reconcileTimer = setInterval(() => { reconcile() }, 5_000)
  }

  function stopReconcileTimer(): void {
    if (reconcileTimer) { clearInterval(reconcileTimer); reconcileTimer = null }
  }

  // TCP notify server: instant wake when hcom messages arrive.
  // `crate::notify::wake_all` TCP-connects to this port on every send.
  function startNotifyServer(): number | null {
    if (notifyServer) return notifyServer.port
    try {
      notifyServer = Bun.listen({
        hostname: "127.0.0.1",
        port: 0,
        socket: {
          open(socket) {
            socket.end()
            log("DEBUG", "notify_server.wake", instanceName, { status: lastReportedStatus, pending_ack: pendingAckId })
            if (sessionId && instanceName) deliverPendingToIdle(sessionId)
          },
          data() {},
          close() {},
          error() {},
        },
      })
      log("INFO", "notify_server.started", instanceName, { port: notifyServer.port })
      return notifyServer.port
    } catch (e) {
      log("ERROR", "notify_server.start_failed", instanceName, { error: String(e) })
      return null
    }
  }

  function stopNotifyServer(): void {
    if (notifyServer) {
      try { notifyServer.stop(true) } catch {}
      notifyServer = null
    }
  }

  async function bindIdentity(sid: string): Promise<void> {
    if (instanceName || bindingPromise) return
    if (process.env.HCOM_LAUNCHED !== "1") return

    bindingPromise = (async () => {
      try {
        // Start TCP notify server before binding so port is registered atomically
        const notifyPort = startNotifyServer()
        const result = notifyPort
          ? await $.nothrow()`hcom opencode-start --session-id ${sid} --notify-port ${String(notifyPort)}`.quiet()
          : await $.nothrow()`hcom opencode-start --session-id ${sid}`.quiet()
        if (result.exitCode !== 0) { stopNotifyServer(); return }
        const json = JSON.parse(result.text())
        if (json.error) {
          log("WARN", "plugin.bind_failed", null, { error: json.error })
          stopNotifyServer()
          return
        }
        const boundModel = normalizePromptModel(json.model)
        if (typeof json.agent === "string") launchedAgent = json.agent
        if (boundModel) launchedModel = boundModel
        instanceName = json.name
        sessionId = json.session_id
        bootstrapText = json.bootstrap || null
        currentAgent = launchedAgent
        currentModel = launchedModel
        log("INFO", "plugin.bound", instanceName, {
          session_id: sessionId,
          notify_port: notifyPort,
          bootstrap_len: bootstrapText?.length ?? 0,
          launched_agent: launchedAgent,
          launched_model: launchedModel?.modelID ?? null,
        })
      } catch (e) {
        log("ERROR", "plugin.bind_error", null, { error: String(e) })
        stopNotifyServer()
      } finally {
        bindingPromise = null
      }
    })()
    await bindingPromise
  }

  return {
    event: async ({ event }: { event: HcomEvent }) => {
      try {
        if (!checkHcom()) return
        const eventSessionId = eventSessionID(event)
        if (eventSessionId && !sessionId) {
          sessionId = eventSessionId as string
        }
        if (instanceName && ignoreForeignSession("plugin.event_ignored_foreign_session", eventSessionId)) {
          return
        }
        switch (event.type) {
          case "session.created": {
            const createdSessionId = event.properties.info.id
            log("INFO", "plugin.session_created", instanceName, { session_id: createdSessionId })
            if (createdSessionId && !instanceName && !bindingPromise) {
              await bindIdentity(createdSessionId)
            }
            break
          }
          case "permission.asked": {
            permissionPending = true
            const eventSessionId = event.properties.sessionID
            if (eventSessionId && !instanceName && !bindingPromise) {
              await bindIdentity(eventSessionId)
            }
            if (instanceName) {
              lastReportedStatus = "blocked"
              await $.nothrow()`hcom opencode-status --name ${instanceName} --status blocked --context ${"approval"} --detail ${String(event.properties.permission ?? "")}`.quiet()
              log("INFO", "plugin.permission_asked", instanceName, { permission: event.properties.permission, request_id: event.properties.id })
            }
            break
          }
          case "permission.replied": {
            permissionPending = false
            const eventSessionId = event.properties.sessionID
            if (instanceName) {
              const statusResult = await client.session.status()
              const current = eventSessionId ? statusResult.data?.[eventSessionId] : null
              const hcomStatus = !current || current.type === "idle" ? "listening" : "active"
              lastReportedStatus = hcomStatus
              await $.nothrow()`hcom opencode-status --name ${instanceName} --status ${hcomStatus}`.quiet()
              if (hcomStatus === "listening" && eventSessionId) {
                await deliverPendingToIdle(eventSessionId)
              }
            }
            break
          }
          case "session.status": {
            const statusType = event.properties.status.type
            const eventSessionId = event.properties.sessionID

            log("DEBUG", "plugin.session_status", instanceName, { status: statusType })

            // Bind identity on resume (session.created doesn't fire for existing sessions)
            if (eventSessionId && !instanceName && !bindingPromise) {
              await bindIdentity(eventSessionId)
            }

            // Report status to hcom daemon (skip if unchanged)
            if (permissionPending) {
              startReconcileTimer()
              break
            }
            if (instanceName) {
              const hcomStatus = statusType === "idle" ? "listening" : "active"
              if (hcomStatus !== lastReportedStatus) {
                lastReportedStatus = hcomStatus
                await $.nothrow()`hcom opencode-status --name ${instanceName} --status ${hcomStatus}`.quiet()
              }
              // Ensure reconcile timer is running (catches missed idle events)
              startReconcileTimer()
            }

            // Idle transition: deliver any pending messages
            if (statusType === "idle" && instanceName && eventSessionId) {
              await deliverPendingToIdle(eventSessionId)
            }
            break
          }
          case "session.deleted":
            log("INFO", "plugin.session_deleted", instanceName)
            stopNotifyServer()
            stopReconcileTimer()
            if (instanceName) {
              await $.nothrow()`hcom opencode-stop --name ${instanceName} --reason closed`.quiet()
            }
            instanceName = null
            sessionId = null
            bootstrapText = null
            bindingPromise = null
            lastReportedStatus = null
            pendingAckId = null
            deliveryInFlight = false
            deliveryPending = false
            deliveryRetryScheduled = false
            permissionPending = false
            currentAgent = launchedAgent
            currentModel = launchedModel
            break
          case "file.edited": {
            const filePath = event.properties.file
            if (instanceName) {
              await $.nothrow()`hcom opencode-status --name ${instanceName} --status active --context ${"tool:write"} --detail ${String(filePath ?? "")}`.quiet()
            }
            break
          }
        }
      } catch (e) {
        log("ERROR", "plugin.event_error", instanceName, { error: String(e) })
      }
    },

    "chat.message": async (input, output) => {
      try {
        if (!checkHcom()) return
        if (input.sessionID && !sessionId) {
          sessionId = input.sessionID
        }
        if (bindingPromise) await bindingPromise
        if (input.sessionID && !instanceName) {
          await bindIdentity(input.sessionID)
        }
        if (isBoundSession(input.sessionID)) {
          if (input.agent) currentAgent = input.agent
          const resolvedModel = normalizePromptModel(input.model, input.variant)
          if (resolvedModel) currentModel = resolvedModel
        } else {
          ignoreForeignSession("plugin.chat_message_ignored_foreign_session", input.sessionID)
        }
        log("DEBUG", "plugin.chat_message", instanceName, {
          session_id: input.sessionID,
          agent: input.agent,
          model: input.model?.modelID,
          variant: input.variant,
        })
      } catch (e) {
        log("ERROR", "plugin.chat_message_error", instanceName, { error: String(e) })
      }
    },

    "experimental.chat.messages.transform": async (input, output) => {
      try {
        if (!checkHcom()) return
        if (bindingPromise) await bindingPromise
        if (!instanceName && sessionId) await bindIdentity(sessionId)
        if (!instanceName || !sessionId) return

        // OpenCode transform mutations are prompt-local, not persisted to stored
        // session history, so keep injecting the original bootstrap payload.
        const messages = output.messages ?? []
        const msgCount = messages.length
        const userMsgCount = messages.filter((m: any) => m.info.role === "user").length
        if (bootstrapText) {
          const firstUserMsg = messages.find((m: any) => m.info.role === "user")
          if (firstUserMsg) {
            firstUserMsg.parts.push({
              id: crypto.randomUUID(),
              messageID: firstUserMsg.info.id,
              sessionID: firstUserMsg.info.sessionID,
              type: "text",
              text: bootstrapText,
              synthetic: true,
            })
            log("DEBUG", "plugin.transform_bootstrap", instanceName, { msg_count: msgCount, user_msgs: userMsgCount, bootstrap_len: bootstrapText.length })
          } else {
            log("WARN", "plugin.transform_no_user_msg", instanceName, { msg_count: msgCount })
          }
        } else {
          log("DEBUG", "plugin.transform_no_bootstrap", instanceName, { msg_count: msgCount, user_msgs: userMsgCount })
        }

        // Bootstrap body fill: the PTY bootstrap injects a bodyless <hcom> tag
        // (no message text, just envelope) because the TUI input box can't safely
        // hold arbitrary message bodies (@ triggers, width overflow). The transform
        // hook fires on the same loop iteration, so we fetch the real body here,
        // replace the bodyless tag, and ack — giving the agent the full message on
        // its first turn without a wasted round-trip.
        const lastUserMsg = [...messages].reverse().find((m: any) => m.info.role === "user")
        if (lastUserMsg && lastUserMsg.parts) {
          const textPart = lastUserMsg.parts.find((p: any) =>
            p.type === "text" && !p.synthetic && typeof p.text === "string" &&
            p.text.startsWith("<hcom>") && p.text.endsWith("</hcom>") && !p.text.includes(": ")
          )
          if (textPart?.type === "text" && pendingAckId === null) {
            const msgResult = await $.nothrow()`hcom opencode-read --name ${instanceName}`.quiet()
            if (msgResult.exitCode === 0) {
              let rawMessages: any[] = []
              try { rawMessages = JSON.parse(msgResult.text()) } catch {}
              if (Array.isArray(rawMessages) && rawMessages.length > 0) {
                const maxId = Math.max(...rawMessages.map((m: any) => m.event_id || 0))
                if (maxId > 0) {
                  const formatted = formatMessagesForInjection(rawMessages, instanceName)
                  textPart.text = formatted
                  pendingAckId = maxId
                  log("INFO", "plugin.transform_bootstrap_delivery", instanceName, { max_id: maxId, msg_count: rawMessages.length })
                }
              }
            }
          }
        }

        // Deferred ack: deliverPendingToIdle called promptAsync but didn't ack.
        // Transform fires on the loop iteration processing that message — ack now.
        if (pendingAckId !== null) {
          const ackId = pendingAckId
          pendingAckId = null
          await $.nothrow()`hcom opencode-read --name ${instanceName} --ack --up-to ${String(ackId)}`.quiet()
          log("INFO", "plugin.deferred_ack", instanceName, { acked_to: ackId })
          if (deliveryPending && sessionId) {
            drainPendingDelivery(sessionId, "post_ack_pending_wake")
          }
        }
      } catch (e) {
        log("ERROR", "plugin.transform_error", instanceName, { error: String(e) })
      }
    },

    "experimental.session.compacting": async (input, output) => {
      try {
        if (!checkHcom()) return
        if (!instanceName) return

        output.context.push(
          `You are connected to hcom as "${instanceName}". ` +
          `Use --name ${instanceName} for all hcom commands.`
        )
        log("INFO", "plugin.compaction_reset", instanceName)
      } catch (e) {
        log("ERROR", "plugin.compaction_error", instanceName, { error: String(e) })
      }
    },
  }
}
